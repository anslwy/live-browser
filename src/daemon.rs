use anyhow::{Context, Result};
use axum::{
    Json, Router,
    extract::{Query, State},
    http::{StatusCode, header},
    response::{Html, IntoResponse},
    routing::{get, post},
};
use chromiumoxide::browser::{Browser, BrowserConfig};
use chromiumoxide::page::Page;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

use crate::cli::DaemonArgs;

// ---------------------------------------------------------------------------
// Shared state
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
struct HandoverState {
    active: bool,
    message: String,
    /// True when the human claimed control via UI/CLI (vs. agent requested it).
    #[serde(default)]
    claimed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    requested_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Default)]
struct CursorState {
    x: f64,
    y: f64,
    kind: String,
    /// "human" or "agent" (default) — who triggered the action.
    #[serde(default)]
    by: String,
    at: i64,
}

/// Per-tab state (a tab is one CDP target / Page).
struct TabState {
    page: Page,
    handover: HandoverState,
    zoom: f64,
    cursor: CursorState,
    last_url: String,
}

struct Tabs {
    map: std::collections::HashMap<u32, TabState>,
    active: u32,
    next_id: u32,
}

#[derive(Clone)]
struct AppState {
    tabs: Arc<Mutex<Tabs>>,
    browser: Arc<Mutex<Browser>>,
    log: Arc<Mutex<Vec<LogEntry>>>,
    /// Optional passcode guarding the HTTP API. `None`/empty = open server.
    passcode: Option<String>,
    /// HTTP port this daemon serves (for actionable restart hints).
    port: u16,
    /// Directory browser downloads are saved to (see `Browser.setDownloadBehavior`).
    download_dir: String,
    /// How the browser was launched — reused for in-process browser switches
    /// (Settings → Browser) without restarting the daemon process.
    launch: Arc<Mutex<LaunchConfig>>,
    /// Background CloakBrowser download progress (Settings → Browser).
    cloak_dl: Arc<Mutex<crate::cloak::DownloadState>>,
}

/// The knobs that define the running browser. Cloned out of [`DaemonArgs`]
/// at startup; [`api_browsers_use`] swaps `chrome` and relaunches.
#[derive(Debug, Clone)]
struct LaunchConfig {
    profile: String,
    headed: bool,
    no_sandbox: bool,
    chrome: Option<String>,
}

impl LaunchConfig {
    fn from_args(args: &DaemonArgs) -> Self {
        // Explicit --chrome / LB_CHROME wins; otherwise reuse the last
        // Settings → Browser choice, if any. Nothing is persisted here: the
        // choice is saved only after a successful launch (see run_daemon),
        // so a bad --chrome value can never poison later starts.
        let chrome = match args.chrome.clone().map(|c| c.trim().to_string()).filter(|c| !c.is_empty()) {
            Some(raw) => Some(resolve_chrome_path(&raw)),
            None => load_persisted_chrome(),
        };
        let base = args.profile.clone().unwrap_or_else(default_profile_dir);
        Self {
            // Same per-kind profile the Settings → Browser switch uses
            // (system <-> base, cloak <-> base-cloak): an older build
            // trap-crashes on a profile a newer one has touched, so a cloak
            // restart must NOT reuse the system profile dir.
            profile: launch_profile_for(&base, chrome.as_deref()),
            headed: !args.headless_mode(),
            no_sandbox: args.no_sandbox,
            chrome,
        }
    }
}

/// "cloak" for CloakBrowser paths, "system" for everything else (including
/// auto-detect). Same heuristic as the kind detection in [`api_browsers`].
fn chrome_kind(path: Option<&str>) -> &'static str {
    match path {
        Some(p) if p.contains(".cloakbrowser") || p.contains("cloak") => "cloak",
        _ => "system",
    }
}

/// Profile dir for `chrome` under `base`: the shared rule for startup and
/// the Settings → Browser switch (see [`kind_profile`] — stable, so repeated
/// switches and restarts land on the same dirs).
fn launch_profile_for(base: &str, chrome: Option<&str>) -> String {
    kind_profile(base, chrome_kind(chrome))
}

/// File holding the last browser-binary choice (`{"chrome": "<path>"}`).
/// Lives next to the pidfiles (`~/.cache/live-browser/browser.json`), so a
/// Settings → Browser switch (or an explicit `--chrome`) survives `lb web`
/// restarts that pass no `--chrome` flag. One file for all ports/profiles:
/// the binary choice is a user preference, not per-server state.
fn browser_prefs_path() -> std::path::PathBuf {
    cache_base().join("live-browser").join("browser.json")
}

/// Last persisted chrome path, if any. Best-effort: corrupt / missing files
/// and vanished binaries (uninstalled CloakBrowser) fall back to auto-detect
/// so startup never breaks because of a stale preference.
fn load_persisted_chrome() -> Option<String> {
    load_persisted_chrome_from(&browser_prefs_path())
}

fn load_persisted_chrome_from(path: &std::path::Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    let raw = v.get("chrome")?.as_str()?.trim().to_string();
    if raw.is_empty() {
        return None;
    }
    let resolved = resolve_chrome_path(&raw);
    if std::path::Path::new(&resolved).exists() {
        Some(resolved)
    } else {
        tracing::warn!(
            "persisted browser {resolved} is gone (uninstalled?) — falling back to auto-detect"
        );
        None
    }
}

/// Persist the browser choice (best-effort; a failure only logs).
/// `None` clears the file so the next start auto-detects.
fn save_persisted_chrome(chrome: Option<&str>) {
    save_persisted_chrome_to(&browser_prefs_path(), chrome);
}

fn save_persisted_chrome_to(path: &std::path::Path, chrome: Option<&str>) {
    if let Some(parent) = path.parent() {
        if std::fs::create_dir_all(parent).is_err() {
            return;
        }
    }
    let result = match chrome.map(|c| c.trim()).filter(|c| !c.is_empty()) {
        Some(c) => std::fs::write(path, format!("{{\"chrome\":{}}}\n", serde_json::Value::String(c.to_string()))),
        None => {
            // Clear: remove the file (missing file = auto-detect anyway).
            match std::fs::remove_file(path) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(e),
            }
        }
    };
    if let Err(e) = result {
        tracing::warn!("could not persist browser choice to {}: {e:#}", path.display());
    }
}

/// Normalize a `--chrome` value into an absolute path for launch + storage:
/// trims, expands a leading `~/`, and resolves a single `*` glob segment
/// (so `~/.cloakbrowser/chromium-*/chrome` works quoted as well as expanded
/// by the shell — the resolved absolute path is what gets persisted).
fn resolve_chrome_path(raw: &str) -> String {
    let mut s = raw.trim().to_string();
    if s.is_empty() {
        return s;
    }
    if s == "~" || s.starts_with("~/") {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
        s = format!("{home}{}", &s[1..]);
    }
    if !s.contains('*') && !s.contains('?') && !s.contains('[') {
        return s;
    }
    match expand_glob_segment(&s) {
        Some(resolved) => resolved,
        None => s,
    }
}

/// Resolve the first path segment containing a glob metacharacter by listing
/// its parent dir and picking the newest (lexicographically last) match.
/// Enough for versioned layouts like `chromium-*/chrome`; returns `None`
/// when nothing matches (caller keeps the literal, launch reports it).
fn expand_glob_segment(pattern: &str) -> Option<String> {
    use std::path::PathBuf;
    let is_abs = pattern.starts_with('/');
    let parts: Vec<&str> = pattern.split('/').collect();
    let wild_idx = parts.iter().position(|p| p.contains('*') || p.contains('?') || p.contains('['))?;
    let parent_parts = &parts[..wild_idx];
    let parent = if parent_parts.is_empty() || (parent_parts.len() == 1 && parent_parts[0].is_empty()) {
        PathBuf::from("/")
    } else {
        let mut p = PathBuf::new();
        if is_abs {
            p.push("/");
        }
        for part in parent_parts {
            if part.is_empty() {
                continue;
            }
            p.push(part);
        }
        p
    };
    let entries = std::fs::read_dir(&parent).ok()?;
    let mut hits: Vec<String> = vec![];
    for e in entries.filter_map(|x| x.ok()) {
        let name = e.file_name().to_string_lossy().to_string();
        if segment_matches(parts[wild_idx], &name) {
            // Plain lexicographic max = newest version.
            hits.push(name);
        }
    }
    if hits.is_empty() {
        return None;
    }
    hits.sort();
    let best = hits.last().unwrap();
    let mut out = PathBuf::from(&parent);
    out.push(best);
    for part in &parts[wild_idx + 1..] {
        out.push(part);
    }
    Some(out.to_string_lossy().to_string())
}

/// `*` / `?` segment match (no character classes — enough for our patterns).
fn segment_matches(pattern: &str, name: &str) -> bool {
    fn rec(p: &[u8], n: &[u8]) -> bool {
        if p.is_empty() {
            return n.is_empty();
        }
        match p[0] {
            b'*' => {
                // Collapse consecutive stars, then try every split.
                let mut pi = 1;
                while pi < p.len() && p[pi] == b'*' {
                    pi += 1;
                }
                for ni in 0..=n.len() {
                    if rec(&p[pi..], &n[ni..]) {
                        return true;
                    }
                }
                false
            }
            b'?' => !n.is_empty() && rec(&p[1..], &n[1..]),
            c => !n.is_empty() && n[0] == c && rec(&p[1..], &n[1..]),
        }
    }
    rec(pattern.as_bytes(), name.as_bytes())
}

/// Default startup / fresh-tab URL: the New Tab page (not `about:blank`).
/// Chrome always opens one initial tab on launch (NTP — or the
/// `-third-party` variant on ungoogled-chromium), so creating a second tab
/// via `new_page` used to leave two tabs on every `lb web` start. Startup
/// now reuses that initial tab instead (see [`startup_page`]).
const DEFAULT_START_URL: &str = "chrome://new-tab-page/";

/// Build the chromiumoxide config for `launch` (shared by startup and
/// in-process browser switches — keep them identical).
fn build_browser_config(launch: &LaunchConfig) -> Result<BrowserConfig, String> {
    let mut cfg = BrowserConfig::builder()
        .user_data_dir(std::path::PathBuf::from(&launch.profile))
        .window_size(1280, 900)
        .launch_timeout(Duration::from_secs(60))
        .request_timeout(Duration::from_secs(20));
    if let Some(chrome) = &launch.chrome {
        cfg = cfg.chrome_executable(chrome.as_str());
    } else if let Some(detected) = detect_chrome() {
        cfg = cfg.chrome_executable(detected);
    }
    if !launch.headed {
        // Headless is the default (no visible window); --headed opts out.
        // Explicit `--headless=new`: the old `--headless` flag was removed in
        // Chrome 132 — newer Chrome ignores it and opens a *visible* window
        // (seen on Windows). The new headless mode is a full browser without
        // a visible window and behaves like headed for automation.
        cfg = cfg.new_headless_mode();
    } else {
        cfg = cfg.with_head();
    }
    if launch.no_sandbox {
        cfg = cfg.no_sandbox();
    }
    // Extra flags are *appended* to chromiumoxide's defaults (which already
    // include --headless/--no-sandbox handling). Do NOT use
    // disable_default_args() here — it would drop the --headless flag itself.
    // NOTE: no --disable-blink-features=AutomationControlled — it breaks
    // native mouse-drag behaviors (text selection, slider scrub).
    cfg = cfg.args(vec![
        "--no-first-run",
        "--no-default-browser-check",
        // Never restore a previous session on launch: a restored
        // `chrome://settings/help` (or any other leftover) tab plus our
        // startup tab is exactly the "two tabs on start" confusion. The
        // startup path additionally closes stray tabs (see `startup_page`),
        // these flags just keep Chrome from showing the restore bubble.
        "--disable-session-crashed-bubble",
        "--disable-infobars",
    ]);
    cfg.build().map_err(|e| anyhow::anyhow!(e).to_string())
}

// ---------------------------------------------------------------------------
// Failure helpers: timeouts + dead-browser detection
// ---------------------------------------------------------------------------

/// Bound for a single CDP round-trip. Without this, one wedged renderer
/// (JS dialog, crashed target) hangs the awaiting endpoint forever — and
/// because some endpoints fan out over tabs, one bad tab used to wedge
/// the whole daemon (`/api/tabs`, `/api/close`, …).
const CDP_TIMEOUT: Duration = Duration::from_secs(20);
/// Tighter bound for per-tab probes inside fan-out endpoints.
const TAB_PROBE_TIMEOUT: Duration = Duration::from_secs(8);

/// True when `e` looks like a dead transport (covers chromiumoxide's
/// "send failed because receiver is gone"). Ambiguous on its own: a dropped
/// *session* (one closed tab) reports the same as a dead browser — callers
/// must consult [`browser_alive`] to tell them apart.
fn is_transport_gone_msg(msg: &str) -> bool {
    let m = msg.to_lowercase();
    m.contains("receiver is gone")
        || m.contains("send failed")
        || m.contains("event loop is terminated")
        || m.contains("connection closed")
        || m.contains("broken pipe")
}

/// Whether the browser connection is up, as observed by the background
/// handler task. One process runs one daemon, so a process-global flag is
/// sufficient — and it is what lets error mapping distinguish "one tab died"
/// (close it, carry on) from "the browser died" (restart, tabs are lost).
static BROWSER_ALIVE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(true);

fn browser_alive() -> bool {
    BROWSER_ALIVE.load(std::sync::atomic::Ordering::SeqCst)
}

fn mark_browser_dead() {
    BROWSER_ALIVE.store(false, std::sync::atomic::Ordering::SeqCst);
}

/// Generation counter for the browser handler watcher: each (re)launch bumps
/// it, and a watcher only marks the browser dead when its generation is still
/// current — so a stale watcher from a switched-out browser can't flag the
/// new one as dead.
static BROWSER_GEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Drive a browser's background handler; record death only while current.
/// Shared by startup and in-process browser switches.
fn spawn_browser_watcher(mut handler: chromiumoxide::handler::Handler) {
    let gen = BROWSER_GEN.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
    tokio::spawn(async move {
        while let Some(h) = handler.next().await {
            if h.is_err() {
                break;
            }
        }
        if BROWSER_GEN.load(std::sync::atomic::Ordering::SeqCst) == gen {
            mark_browser_dead();
            tracing::warn!("browser handler ended — the browser process is gone");
        }
    });
}

/// Recovery text for a dead tab: close it and carry on — with the escalation
/// in case the whole browser is actually down.
fn tab_gone_msg(port: u16) -> String {
    format!(
        "this tab's page is gone (renderer closed or crashed — Alt+F4 and window.close() do this) — \
         `lb close` it and `lb open` a fresh tab. \
         If every tab fails this way, the browser itself is down: restart with `lb web --port {port} restart`"
    )
}

/// True when `e` looks like a dead *tab* (renderer closed/crashed — e.g.
/// Alt+F4, `window.close()`), including dropped session channels while the
/// browser itself is still alive.
fn is_tab_gone_msg(msg: &str) -> bool {
    let m = msg.to_lowercase();
    m.contains("session with given id")
        || m.contains("no target with given id")
        || m.contains("target crashed")
        || m.contains("cannot find target")
        || m.contains("no such target")
        || m.contains("target closed")
        || (is_transport_gone_msg(&m) && browser_alive())
}

/// Old name kept for the explicit browser-death probe: true only for
/// transport errors observed while the browser is known to be down.
fn is_browser_gone_msg(msg: &str) -> bool {
    is_transport_gone_msg(msg) && !browser_alive()
}

/// Actionable replacement for raw transport errors: tells the user the
/// browser process is gone and exactly how to recover (instead of
/// `send failed because receiver is gone`, which means nothing).
fn gone_err(port: u16) -> serde_json::Value {
    serde_json::json!({ "ok": false,
        "error": format!("browser process died — restart it with `lb web --port {port} restart` (open tabs will be lost)") })
}

/// Map a CDP failure into a user-facing error string: dead-tab errors name
/// the recovery (`lb close` + fresh tab), dead-browser errors point at
/// `lb web restart`, everything else passes through (truncated).
fn friendly_cdp_error(port: u16, context: &str, e: impl std::fmt::Display) -> String {
    let raw = format!("{e:#}");
    tracing::warn!("CDP failure ({context}): {}", raw.chars().take(300).collect::<String>());
    if is_tab_gone_msg(&raw) {
        return tab_gone_msg(port);
    }
    if is_browser_gone_msg(&raw) {
        return gone_err(port)
            .get("error")
            .and_then(|x| x.as_str())
            .unwrap_or("browser process died")
            .to_string();
    }
    let short: String = raw.chars().take(400).collect();
    format!("{context}: {short}")
}

// ---------------------------------------------------------------------------
// Passcode auth (optional, like `opencode web` + OPENCODE_SERVER_PASSWORD)
// ---------------------------------------------------------------------------

/// True when `req` carries the configured passcode, via any of:
/// - `Authorization: Bearer <code>`
/// - `Authorization: Basic <base64(user:code)>` (password part must match;
///   username is ignored, so browsers / curl `--user x:<code>` just work)
/// - `x-lb-passcode: <code>` header (what the `lb` CLI and web UI send)
/// - `?passcode=<code>` (or `?password=`) query (what `<img>` tags need,
///   since they can't set headers)
fn request_has_passcode(req: &axum::http::Request<axum::body::Body>, passcode: &str) -> bool {
    if let Some(v) = req.headers().get("x-lb-passcode") {
        if v.as_bytes() == passcode.as_bytes() {
            return true;
        }
    }
    if let Some(v) = req.headers().get(axum::http::header::AUTHORIZATION) {
        if let Ok(s) = v.to_str() {
            if let Some(token) = s.strip_prefix("Bearer ") {
                if token.trim() == passcode {
                    return true;
                }
            } else if let Some(b64) = s.strip_prefix("Basic ") {
                if let Ok(decoded) = base64::Engine::decode(
                    &base64::engine::general_purpose::STANDARD,
                    b64.trim(),
                ) {
                    // `user:password` — password may itself contain ':'.
                    if let Some(idx) = decoded.iter().rposition(|b| *b == b':') {
                        if &decoded[idx + 1..] == passcode.as_bytes() {
                            return true;
                        }
                    } else if decoded.as_slice() == passcode.as_bytes() {
                        return true;
                    }
                }
            }
        }
    }
    // Query fallback for <img src="/api/screenshot"> and quick curl checks.
    if let Some(q) = req.uri().query() {
        for pair in q.split('&') {
            let (k, v) = match pair.split_once('=') {
                Some(x) => x,
                None => continue,
            };
            if k == "passcode" || k == "password" {
                // Query values are percent-encoded; decode minimally.
                let decoded = percent_decode(v);
                if decoded == passcode {
                    return true;
                }
            }
        }
    }
    false
}

fn percent_decode(s: &str) -> String {
    let mut out = Vec::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                out.push(h << 4 | l);
                i += 3;
                continue;
            }
        }
        if bytes[i] == b'+' {
            out.push(b' ');
        } else {
            out.push(bytes[i]);
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Axum middleware guarding every `/api/*` route. Open server (no passcode
/// configured) lets everything through; otherwise missing/wrong credentials
/// get a 401 JSON (deliberately *without* `WWW-Authenticate`, so browsers
/// show our login form instead of a native dialog).
async fn require_passcode(
    State(s): State<AppState>,
    req: axum::http::Request<axum::body::Body>,
    next: axum::middleware::Next,
) -> impl IntoResponse {
    // Only gate the JSON API + screenshots; the static UI stays public so
    // it can render its own passcode form.
    if !req.uri().path().starts_with("/api/") {
        return next.run(req).await;
    }
    let configured = s.passcode.clone().unwrap_or_default();
    if configured.is_empty() {
        return next.run(req).await;
    }
    if request_has_passcode(&req, &configured) {
        return next.run(req).await;
    }
    (
        StatusCode::UNAUTHORIZED,
        [(header::CONTENT_TYPE, "application/json"), (header::CACHE_CONTROL, "no-store")],
        axum::Json(serde_json::json!({
            "ok": false,
            "error": "unauthorized — wrong or missing passcode (set --passcode / LB_PASSCODE)",
        }))
        .to_string(),
    )
        .into_response()
}

/// Resolve a tab selector (None = active tab) to its id + a Page handle.
async fn tab_page(s: &AppState, tab: Option<u32>) -> anyhow::Result<(u32, Page)> {
    let t = s.tabs.lock().await;
    let id = tab.unwrap_or(t.active);
    t.map
        .get(&id)
        .map(|ts| (id, ts.page.clone()))
        .ok_or_else(|| anyhow::anyhow!("no such tab: {id} — see `lb list`"))
}

macro_rules! tab {
    ($s:expr, $t:expr) => {
        match tab_page($s, $t).await {
            Ok(x) => x,
            Err(e) => return err(format!("{e:#}")),
        }
    };
}

/// Mutate one tab's state.
async fn with_tab<F>(s: &AppState, id: u32, f: F)
where
    F: FnOnce(&mut TabState),
{
    if let Some(ts) = s.tabs.lock().await.map.get_mut(&id) {
        f(ts);
    }
}

/// Adopt browser pages the page opened itself (`target=_blank` links,
/// `window.open` popups): they bypass our tab map, so without this a click
/// on an "open in new tab" link appears to do nothing in the live view.
/// The newest adopted tab becomes active (foreground-open semantics).
/// Cheap: `browser.pages()` is an in-memory handler query (no CDP round
/// trip), and inserts are re-checked under the tabs lock so concurrent
/// reconciles can't double-adopt the same target.
async fn reconcile_tabs(s: &AppState) {
    let pages = {
        let browser = s.browser.lock().await;
        match tokio::time::timeout(TAB_PROBE_TIMEOUT, browser.pages()).await {
            Ok(Ok(p)) => p,
            _ => return,
        }
    };
    if pages.is_empty() {
        return;
    }
    // Snapshot known targets (no CDP while holding the tabs lock).
    let known: Vec<_> = {
        let t = s.tabs.lock().await;
        t.map.values().map(|ts| ts.page.target_id().clone()).collect()
    };
    let strays: Vec<Page> = pages
        .into_iter()
        .filter(|p| !known.contains(p.target_id()))
        .collect();
    if strays.is_empty() {
        return;
    }
    // Resolve URLs before taking the tabs lock (bounded: a wedged renderer
    // must not stall tab bookkeeping).
    let mut fresh: Vec<(Page, String)> = vec![];
    for p in strays {
        let url = tokio::time::timeout(TAB_PROBE_TIMEOUT, p.url())
            .await
            .ok()
            .and_then(|r| r.ok())
            .flatten()
            .unwrap_or_default();
        fresh.push((p, url));
    }
    let mut adopted: Vec<(Page, u32)> = vec![];
    {
        let mut t = s.tabs.lock().await;
        for (page, url) in fresh {
            // Re-check under the lock: a concurrent reconcile may have
            // adopted this target after our snapshot.
            if t.map.values().any(|ts| ts.page.target_id() == page.target_id()) {
                continue;
            }
            let id = t.next_id;
            t.next_id += 1;
            t.map.insert(
                id,
                TabState {
                    page: page.clone(),
                    handover: HandoverState { active: false, message: String::new(), claimed: false, requested_at: None },
                    zoom: 1.0,
                    cursor: CursorState::default(),
                    last_url: url,
                },
            );
            t.active = id;
            adopted.push((page, id));
        }
    }
    for (page, id) in adopted {
        spawn_dialog_guard(page, s.log.clone(), id);
        s.push_log("open/tab", &format!("tab {id} opened by the page (adopted)"));
    }
}

#[derive(Debug, Clone, Serialize)]
struct LogEntry {
    at: String,
    action: String,
    detail: String,
}

/// Stepped zoom levels offered by the UI / CLI.
const ZOOM_LEVELS: &[f64] = &[0.5, 0.67, 0.8, 0.9, 1.0, 1.1, 1.25, 1.5];

fn step_zoom(current: f64, dir: &str) -> f64 {
    match dir {
        "reset" => 1.0,
        "out" => ZOOM_LEVELS
            .iter()
            .copied()
            .rfind(|z| *z < current - 1e-9)
            .unwrap_or(0.5),
        _ => ZOOM_LEVELS
            .iter()
            .copied()
            .find(|z| *z > current + 1e-9)
            .unwrap_or(1.5), // "in" and anything else
    }
}

async fn apply_zoom(page: &Page, zoom: f64) {
    let js = format!("document.body && (document.body.style.zoom = {zoom})");
    let _ = page.evaluate(js.as_str()).await;
}

impl AppState {
    fn push_log(&self, action: &str, detail: &str) {
        let entry = LogEntry {
            at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            action: action.to_string(),
            detail: detail.chars().take(300).collect(),
        };
        let log = self.log.clone();
        tokio::spawn(async move {
            let mut l = log.lock().await;
            l.push(entry);
            if l.len() > 200 {
                let n = l.len() - 200;
                l.drain(..n);
            }
        });
    }

    async fn set_cursor(&self, id: u32, x: f64, y: f64, kind: &str, by: &str) {
        let by = if by == "human" { "human".into() } else { "agent".into() };
        with_tab(self, id, move |ts| {
            ts.cursor.x = x;
            ts.cursor.y = y;
            ts.cursor.kind = kind.to_string();
            ts.cursor.by = by;
            ts.cursor.at = chrono::Utc::now().timestamp_millis();
        })
        .await;
    }
}

fn ok(msg: &str) -> Json<serde_json::Value> {
    Json(serde_json::json!({ "ok": true, "message": msg }))
}

fn err(msg: String) -> Json<serde_json::Value> {
    Json(serde_json::json!({ "ok": false, "error": msg }))
}

// ---------------------------------------------------------------------------
// JS-dialog guard: alert()/confirm()/prompt() used to wedge the tab (and,
// via fan-out endpoints, the whole daemon) with no recovery. Every page gets
// a background task that auto-dismisses dialogs and logs them, so automation
// can always proceed. Humans driving via `lb web --headed` see the dismissal
// in the action log.
// ---------------------------------------------------------------------------

/// Attach the auto-dismiss guard to `page`. Safe to call on every new tab.
fn spawn_dialog_guard(page: Page, log: Arc<Mutex<Vec<LogEntry>>>, tab_id: u32) {
    tokio::spawn(async move {
        use chromiumoxide::cdp::browser_protocol::page::EventJavascriptDialogOpening;
        let mut events = match page.event_listener::<EventJavascriptDialogOpening>().await {
            Ok(e) => e,
            Err(_) => return,
        };
        use futures::StreamExt;
        while let Some(ev) = events.next().await {
            let kind = format!("{:?}", ev.r#type).to_lowercase();
            // beforeunload is accepted (lets the pending navigation proceed);
            // alert/confirm/prompt are dismissed (equivalent to Cancel/Esc).
            let accept = kind.contains("beforeunload");
            let msg: String = ev.message.chars().take(200).collect();
            let params = chromiumoxide::cdp::browser_protocol::page::HandleJavaScriptDialogParams::new(accept);
            let _ = tokio::time::timeout(Duration::from_secs(5), page.execute(params)).await;
            let entry = LogEntry {
                at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                action: "dialog".into(),
                detail: format!(
                    "tab {tab_id}: auto-{} {kind} dialog: {msg}",
                    if accept { "accepted" } else { "dismissed" }
                )
                .chars()
                .take(300)
                .collect(),
            };
            let log = log.clone();
            tokio::spawn(async move {
                let mut l = log.lock().await;
                l.push(entry);
                if l.len() > 200 {
                    let n = l.len() - 200;
                    l.drain(..n);
                }
            });
        }
    });
}

// ---------------------------------------------------------------------------
// Snapshot / interaction JS
// ---------------------------------------------------------------------------

const SNAPSHOT_JS: &str = r#"(() => {
  document.querySelectorAll('[data-lb-ref]').forEach(e => e.removeAttribute('data-lb-ref'));
  // data-lb-* inside same-origin iframes is namespaced per frame so a ref
  // query can be resolved without collisions.
  try {
    document.querySelectorAll('iframe').forEach(f => {
      try {
        const d = f.contentDocument;
        if (d) d.querySelectorAll('[data-lb-ref]').forEach(e => e.removeAttribute('data-lb-ref'));
      } catch (e) { /* cross-origin: skip */ }
    });
  } catch (e) {}
  const SELECTOR = 'a, button, input, select, textarea, [role="button"], [role="link"], [role="textbox"], [onclick], summary';
  const out = [];
  let ref = 1;
  const push = (el, frame, dx, dy) => {
    if (out.length >= 200) return;
    try {
      const rect = el.getBoundingClientRect();
      const style = (el.ownerDocument.defaultView || window).getComputedStyle(el);
      if (style.display === 'none' || style.visibility === 'hidden') return;
      if (rect.width === 0 && rect.height === 0 && rect.top === 0 && rect.left === 0) return;
      el.setAttribute('data-lb-ref', String(ref));
      let name = el.getAttribute('aria-label')
        || (el.innerText || '')
        || el.getAttribute('value')
        || el.getAttribute('placeholder')
        || el.getAttribute('name')
        || el.getAttribute('title')
        || '';
      name = String(name).trim().replace(/\s+/g, ' ').slice(0, 80);
      out.push({
        ref: ref,
        tag: el.tagName.toLowerCase(),
        type: el.getAttribute('type') || '',
        name: name,
        href: el.getAttribute('href') || '',
        placeholder: el.getAttribute('placeholder') || '',
        value: (el.value !== undefined ? String(el.value).slice(0, 80) : ''),
        frame: frame,
        x: Math.round((rect.left + rect.width / 2 + dx) * 10) / 10,
        y: Math.round((rect.top + rect.height / 2 + dy) * 10) / 10,
        options: (el.tagName === 'SELECT'
          ? Array.from(el.options).map(o => ({
              value: o.value,
              text: String(o.text || '').trim().replace(/\s+/g, ' ').slice(0, 40),
              selected: o.selected
            }))
          : [])
      });
      ref++;
    } catch (e) {}
  };
  document.querySelectorAll(SELECTOR).forEach(el => push(el, '', 0, 0));
  // Same-origin iframes: cross-origin frames throw on contentDocument access
  // and are skipped (their elements still work via lb click-at with UI coords).
  try {
    document.querySelectorAll('iframe').forEach((f, i) => {
      try {
        const d = f.contentDocument;
        if (!d) return;
        const r = f.getBoundingClientRect();
        d.querySelectorAll(SELECTOR).forEach(el => push(el, 'iframe[' + i + ']', r.left, r.top));
      } catch (e) { /* cross-origin: skip */ }
    });
  } catch (e) {}
  return out;
})()"#;

/// Resolve a snapshot ref to viewport coordinates, searching the main
/// document *and* same-origin iframes. Used as the fallback when a
/// main-frame CSS lookup fails (iframe elements have their
/// `data-lb-ref` inside the frame document).
const REF_COORDS_JS: &str = r#"((want) => {
  const inDoc = (doc, dx, dy) => {
    const el = doc.querySelector('[data-lb-ref="' + want + '"]');
    if (!el) return null;
    const r = el.getBoundingClientRect();
    return [r.left + r.width / 2 + dx, r.top + r.height / 2 + dy];
  };
  const pt = inDoc(document, 0, 0);
  if (pt) return pt;
  const frames = document.querySelectorAll('iframe');
  for (let i = 0; i < frames.length; i++) {
    try {
      const d = frames[i].contentDocument;
      if (!d) continue;
      const r = frames[i].getBoundingClientRect();
      const hit = inDoc(d, r.left, r.top);
      if (hit) return hit;
    } catch (e) { /* cross-origin: skip */ }
  }
  return null;
})"#;

/// Focus the element behind a snapshot ref (main frame or same-origin
/// iframe), optionally clearing it first. Returns "ok", "not-found", or
/// an "error: …" string. Powers type/click fallbacks for framed elements.
const FOCUS_REF_JS: &str = r#"((want, clear) => {
  const inDoc = (doc) => doc.querySelector('[data-lb-ref="' + want + '"]');
  let el = inDoc(document);
  if (!el) {
    const frames = document.querySelectorAll('iframe');
    for (let i = 0; i < frames.length; i++) {
      try {
        const d = frames[i].contentDocument;
        if (!d) continue;
        el = inDoc(d);
        if (el) break;
      } catch (e) { /* cross-origin: skip */ }
    }
  }
  if (!el) return 'not-found';
  try {
    el.scrollIntoView({ block: 'center' });
    el.focus();
    if (clear && 'value' in el) {
      el.value = '';
      el.dispatchEvent(new Event('input', { bubbles: true }));
      el.dispatchEvent(new Event('change', { bubbles: true }));
    }
    return 'ok';
  } catch (e) { return 'error: ' + e; }
})"#;

/// Presence probe for `lb wait`: true when the target exists. Accepts a
/// snapshot ref ("12"), a `css=` selector, or plain CSS.
const WAIT_PROBE_PREFIX: &str = r#"((target) => {
  let sel = target.trim();
  if (!sel.startsWith('css=')) {
    if (/^\d+$/.test(sel)) sel = '[data-lb-ref="' + sel + '"]';
  } else {
    sel = sel.slice(4);
  }
  if (document.querySelector(sel)) return true;
  const frames = document.querySelectorAll('iframe');
  for (let i = 0; i < frames.length; i++) {
    try {
      const d = frames[i].contentDocument;
      if (d && d.querySelector(sel)) return true;
    } catch (e) {}
  }
  return false;
})"#;

/// Resolve a CLI target to a CSS selector.
/// - `css:.foo` -> `.foo`
/// - `12`       -> `[data-lb-ref="12"]` (requires a fresh snapshot first)
/// - anything else -> treated as CSS selector
fn target_to_selector(target: &str) -> String {
    let t = target.trim();
    if let Some(rest) = t.strip_prefix("css=") {
        return rest.to_string();
    }
    if t.parse::<u32>().is_ok() {
        return format!("[data-lb-ref=\"{t}\"]");
    }
    t.to_string()
}

async fn page_url(page: &Page) -> String {
    page.url().await.unwrap_or(None).unwrap_or_default()
}

async fn page_title(page: &Page) -> String {
    page.get_title().await.unwrap_or(None).unwrap_or_default()
}

// ---------------------------------------------------------------------------
// API handlers
// ---------------------------------------------------------------------------

/// `?tab=N` query shared by GET endpoints (default: active tab).
#[derive(Deserialize)]
struct TabQuery {
    #[serde(default)]
    tab: Option<u32>,
}

async fn api_status(
    State(s): State<AppState>,
    Query(q): Query<TabQuery>,
) -> Json<serde_json::Value> {
    // Adopt page-opened tabs first so the live view follows target=_blank links.
    reconcile_tabs(&s).await;
    let (tab_id, page) = tab!(&s, q.tab);
    // Bounded reads: a wedged renderer must not hang even the status call.
    // Falls back to the last known URL so status stays useful.
    let url = match tokio::time::timeout(TAB_PROBE_TIMEOUT, page.url()).await {
        Ok(Ok(Some(u))) => u,
        _ => s.tabs.lock().await.map.get(&tab_id).map(|ts| ts.last_url.clone()).unwrap_or_default(),
    };
    let title = match tokio::time::timeout(TAB_PROBE_TIMEOUT, page.get_title()).await {
        Ok(Ok(Some(t))) => t,
        _ => String::new(),
    };
    let t = s.tabs.lock().await;
    let ts = t.map.get(&tab_id).unwrap();
    Json(serde_json::json!({
        "ok": true,
        "tab": tab_id,
        "active": tab_id == t.active,
        "url": url,
        "title": title,
        "handover": ts.handover,
        "zoom": ts.zoom,
        "cursor": ts.cursor,
    }))
}

#[derive(Deserialize)]
struct OpenReq {
    /// Tab id (default: active tab).
    #[serde(default)]
    tab: Option<u32>,
    url: String,
    /// Navigate the active tab in place instead of opening a new tab
    /// (same as passing the active tab id explicitly).
    #[serde(default)]
    here: bool,
}
/// Omnibox-style normalization: full URLs pass through, bare domains get
/// https://, and anything else becomes a Google search.
fn normalize_open_url(input: &str) -> Result<String, String> {
    let t = input.trim();
    if t.is_empty() {
        return Err("empty url".into());
    }
    // Anything with whitespace is free text, never a URL — even when it
    // contains "://" (e.g. `ht!tp://bad url`). The old code passed such
    // input straight to `page.goto`, which hung until the client timed out.
    if t.contains("://") {
        if t.chars().any(|c| c.is_whitespace()) {
            return Ok(format!("https://www.google.com/search?q={}", urlencode_query(t)));
        }
        let scheme = t.split("://").next().unwrap_or("");
        let scheme_ok =
            !scheme.is_empty() && scheme.chars().all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '-' || c == '.');
        if !scheme_ok {
            return Err(format!(
                "malformed URL {t:?} — scheme {scheme:?} is invalid (need e.g. https://…)"
            ));
        }
        return Ok(t.to_string());
    }
    if t.starts_with("about:") || t.starts_with("data:") || t.starts_with("file:") {
        if t.chars().any(|c| c.is_whitespace()) {
            return Err(format!("malformed URL {t:?} — URLs cannot contain spaces"));
        }
        return Ok(t.to_string());
    }
    let single_token = !t.chars().any(|c| c.is_whitespace());
    if single_token && (t.contains('.') || t.starts_with("localhost") || t.contains(':')) {
        // `host:port` without a scheme still navigates; anything with a
        // colon that isn't host:port-shaped is almost certainly a typo.
        if t.contains(':') && !t.starts_with("localhost") {
            let parts: Vec<&str> = t.split(':').collect();
            let host_ok = !parts[0].is_empty() && parts.len() == 2 && !parts[1].is_empty();
            if !host_ok {
                return Err(format!("malformed URL {t:?} — did you mean https://{t}?"));
            }
        }
        return Ok(format!("https://{t}"));
    }
    Ok(format!("https://www.google.com/search?q={}", urlencode_query(t)))
}

fn urlencode_query(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'0'..=b'9' | b'a'..=b'z' | b'A'..=b'Z' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

async fn api_open(State(s): State<AppState>, Json(b): Json<OpenReq>) -> Json<serde_json::Value> {
    let input = b.url.trim().to_string();
    if input.is_empty() {
        return err("empty url".into());
    }
    let url = match normalize_open_url(&input) {
        Ok(u) => u,
        Err(e) => return err(e),
    };
    // With an explicit tab (or --here): navigate it in place.
    // Without: open a new tab (pass --here to reuse the active one —
    // otherwise repeated `lb open` calls pile up tabs).
    let tab_sel = b.tab.or(if b.here {
        Some(s.tabs.lock().await.active)
    } else {
        None
    });
    let (tab_id, page) = match tab_sel {
        Some(id) => tab!(&s, Some(id)),
        None => {
            let new_page = {
                let browser = s.browser.lock().await;
                match tokio::time::timeout(CDP_TIMEOUT, browser.new_page(&url)).await {
                    Ok(Ok(p)) => p,
                    Ok(Err(e)) => {
                        return err(friendly_cdp_error(s.port, "new tab failed", e));
                    }
                    Err(_) => {
                        return err(format!(
                            "new tab timed out after {}s — the browser may be wedged (a JS dialog blocks it?); check `lb log`",
                            CDP_TIMEOUT.as_secs()
                        ));
                    }
                }
            };
            // Scope the lock: it must be released before open_navigate,
            // which locks again (holding it across the call deadlocks).
            let id = {
                let mut t = s.tabs.lock().await;
                let id = t.next_id;
                t.next_id += 1;
                t.map.insert(
                    id,
                    TabState {
                        page: new_page.clone(),
                        handover: HandoverState { active: false, message: String::new(), claimed: false, requested_at: None },
                        zoom: 1.0,
                        cursor: CursorState::default(),
                        last_url: url.clone(),
                    },
                );
                t.active = id;
                id
            };
            s.push_log("open/tab", &format!("tab {id} <- {url}"));
            spawn_dialog_guard(new_page.clone(), s.log.clone(), id);
            return open_navigate(&s, id, new_page, &url).await;
        }
    };
    s.push_log("open", &format!("tab {tab_id} <- {url}"));
    open_navigate(&s, tab_id, page, &url).await
}

/// Shared goto-with-retry used by open (new or existing tab).
/// Never hangs: every CDP step is bounded so the UI/CLI always get a response.
async fn open_navigate(s: &AppState, tab_id: u32, page: Page, url: &str) -> Json<serde_json::Value> {
    // about:blank needs no navigation at all — just confirm the tab.
    if url == "about:blank" {
        with_tab(s, tab_id, |ts| ts.last_url = "about:blank".into()).await;
        return Json(serde_json::json!({ "ok": true, "tab": tab_id,
            "message": format!("tab {tab_id}: blank tab ready") }));
    }
    // The network here can flap (VPN/tailscale), so retry transient failures.
    let mut last_err = String::new();
    for attempt in 1..=3 {
        let goto = tokio::time::timeout(Duration::from_secs(25), page.goto(url)).await;
        let goto = match goto {
            Ok(r) => r,
            Err(_) => {
                last_err = "goto timed out after 25s".into();
                s.push_log("open/retry", &format!("attempt {attempt}: {last_err}"));
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
        };
        // goto may report transient network errors (e.g. ERR_NETWORK_CHANGED)
        // while the navigation still succeeds — verify by current URL.
        tokio::time::sleep(Duration::from_millis(1500)).await;
        let current = page_url(&page).await;
        if goto.is_ok() || !current.starts_with("chrome-error") {
            // Give the page a moment to keep loading; don't hard-fail on waits.
            // Non-http(s) targets (about:blank, data:, …) settle instantly.
            let wait = if url.starts_with("http") { 10 } else { 1 };
            let _ = tokio::time::timeout(Duration::from_secs(wait), page.wait_for_navigation()).await;
            let landed = page_url(&page).await;
            // Remember the last healthy URL so reload can recover from errors.
            if !landed.starts_with("chrome-error") && !landed.is_empty() {
                with_tab(s, tab_id, |ts| ts.last_url = landed.clone()).await;
            }
            // Navigations reset document zoom — re-apply the stored level.
            let zoom = s.tabs.lock().await.map.get(&tab_id).map(|ts| ts.zoom).unwrap_or(1.0);
            apply_zoom(&page, zoom).await;
            let note = if attempt > 1 { format!(" (after {attempt} attempts)") } else { String::new() };
            return Json(serde_json::json!({ "ok": true, "tab": tab_id,
                "message": format!("tab {tab_id}: opened {url} (now at {landed}){note}") }));
        }
        last_err = format!("{:#}", goto.unwrap_err());
        s.push_log("open/retry", &format!("attempt {attempt}: {last_err}"));
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    err(format!("goto failed after 3 attempts (network flapping?): {last_err}"))
}

async fn api_snapshot(
    State(s): State<AppState>,
    Query(q): Query<TabQuery>,
) -> Json<serde_json::Value> {
    let (tab_id, page) = tab!(&s, q.tab);
    let url = page_url(&page).await;
    let title = page_title(&page).await;
    match tokio::time::timeout(CDP_TIMEOUT, page.evaluate(SNAPSHOT_JS)).await {
        Ok(Ok(eval)) => match eval.into_value::<serde_json::Value>() {
            Ok(elements) => Json(serde_json::json!({
                "ok": true, "tab": tab_id, "url": url, "title": title, "elements": elements,
                "hint": "click with `lb click <ref>`, type with `lb type <ref> <text>`. Re-run snapshot after navigation."
            })),
            Err(e) => err(format!("decode snapshot failed: {e:#}")),
        },
        Ok(Err(e)) => err(friendly_cdp_error(s.port, "snapshot failed (page still loading?)", e)),
        Err(_) => err(format!(
            "snapshot timed out after {}s — the page may be stuck (heavy load, infinite loop). Try `lb reload --tab {tab_id}` (global --tab flag) or `lb eval` instead",
            CDP_TIMEOUT.as_secs()
        )),
    }
}

/// Stable, dependency-free 64-bit FNV-1a hash of screenshot bytes.
/// Used as the ETag / `?have=` value for conditional screenshot requests.
/// (std's DefaultHasher is not stable across processes; FNV-1a is.)
fn image_hash(bytes: &[u8]) -> String {
    const OFFSET: u64 = 0xcbf29ce484222325;
    const PRIME: u64 = 0x100000001b3;
    let mut h = OFFSET;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(PRIME);
    }
    format!("{h:016x}")
}

/// Strip `W/`, quotes and whitespace so `If-None-Match: "abc"` matches `abc`.
fn normalize_etag_value(s: &str) -> &str {
    let s = s.trim();
    let s = s.strip_prefix("W/").unwrap_or(s);
    s.trim_matches('"').trim()
}

#[derive(Deserialize)]
struct ScreenshotQuery {
    #[serde(default)]
    tab: Option<u32>,
    /// Client's last image hash (from `ETag` / `X-Image-Hash`). When it
    /// matches the freshly captured frame the server replies `304` with no
    /// body instead of re-sending ~47kB of JPEG.
    #[serde(default)]
    have: Option<String>,
}

async fn api_screenshot(
    State(s): State<AppState>,
    headers: axum::http::HeaderMap,
    Query(q): Query<ScreenshotQuery>,
) -> impl IntoResponse {
    let (_tab_id, page) = match tab_page(&s, q.tab).await {
        Ok(x) => x,
        Err(e) => {
            return (
                StatusCode::NOT_FOUND,
                [(header::CONTENT_TYPE, "text/plain")],
                format!("{e:#}").into_bytes(),
            )
                .into_response()
        }
    };
    // Never let a slow screenshot pile up: the UI polls every ~2s, so fail
    // fast (the UI just retries on the next tick).
    let params = chromiumoxide::page::ScreenshotParams::builder()
        .format(chromiumoxide::cdp::browser_protocol::page::CaptureScreenshotFormat::Jpeg)
        .quality(70)
        .build();
    let shot = tokio::time::timeout(Duration::from_secs(10), page.screenshot(params)).await;
    match shot {
        Ok(Ok(bytes)) => {
            let hash = image_hash(&bytes);
            let etag = format!("\"{hash}\"");
            // Conditional request? `?have=` (fetch flow) or standard
            // `If-None-Match` (browser / curl flow).
            let client_have = q.have.as_deref().map(normalize_etag_value).unwrap_or("");
            let inm_match = headers
                .get(axum::http::header::IF_NONE_MATCH)
                .and_then(|v| v.to_str().ok())
                .map(|v| {
                    v.split(',')
                        .any(|tag| normalize_etag_value(tag) == hash || tag.trim() == "*")
                })
                .unwrap_or(false);
            if client_have == hash || inm_match {
                let mut hm = axum::http::HeaderMap::new();
                if let Ok(v) = axum::http::HeaderValue::from_str(&etag) {
                    hm.insert(header::ETAG, v);
                }
                hm.insert(header::CACHE_CONTROL, header::HeaderValue::from_static("no-store"));
                return (StatusCode::NOT_MODIFIED, hm, Vec::new()).into_response();
            }
            let mut hm = axum::http::HeaderMap::new();
            hm.insert(header::CONTENT_TYPE, header::HeaderValue::from_static("image/jpeg"));
            hm.insert(header::CACHE_CONTROL, header::HeaderValue::from_static("no-store"));
            if let Ok(v) = axum::http::HeaderValue::from_str(&etag) {
                hm.insert(header::ETAG, v);
            }
            if let Ok(v) = axum::http::HeaderValue::from_str(&hash) {
                hm.insert(axum::http::HeaderName::from_static("x-image-hash"), v);
            }
            (StatusCode::OK, hm, bytes).into_response()
        }
        Ok(Err(e)) => (
            StatusCode::BAD_GATEWAY,
            [(header::CONTENT_TYPE, "text/plain"), (header::CACHE_CONTROL, "no-store")],
            format!("screenshot failed: {e:#}").into_bytes(),
        )
            .into_response(),
        Err(_) => (
            StatusCode::GATEWAY_TIMEOUT,
            [(header::CONTENT_TYPE, "text/plain"), (header::CACHE_CONTROL, "no-store")],
            b"screenshot timed out (page still loading?)".to_vec(),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
struct ClickReq {
    /// Tab id (default: active tab).
    #[serde(default)]
    tab: Option<u32>,
    target: String,
    /// "human" when triggered from the web UI; anything else = agent.
    #[serde(default)]
    by: String,
    /// "single" (default), "double" or "right".
    #[serde(default = "default_click_kind")]
    kind: String,
}
fn default_click_kind() -> String {
    "single".to_string()
}
async fn api_click(State(s): State<AppState>, Json(b): Json<ClickReq>) -> Json<serde_json::Value> {
    let sel = target_to_selector(&b.target);
    let (tab_id, page) = tab!(&s, b.tab);
    let kind = b.kind.to_lowercase();
    if kind != "single" && kind != "double" && kind != "right" {
        return err(format!("unknown click kind: {:?} (use single, double or right)", b.kind));
    }
    s.push_log("click", &format!("tab {tab_id} <- {sel} [{kind}]"));
    let label = if kind == "single" { String::new() } else { format!(" [{kind}]") };
    // Ensure refs exist (snapshot tags elements) — but only for numeric refs.
    // Raw `css=` selectors don't need DOM re-tagging; skipping the full
    // SNAPSHOT_JS scan here saves a costly layout-forcing evaluate.
    if !b.target.trim().starts_with("css=") && b.target.trim().parse::<u32>().is_ok() {
        let _ = tokio::time::timeout(CDP_TIMEOUT, page.evaluate(SNAPSHOT_JS)).await;
    }
    let do_click = async {
        match page.find_element(&sel).await {
            Ok(el) => {
                // Report the click point for the UI's fake cursor.
                let center_js = format!(
                    "(() => {{ const el = document.querySelector({}); if (!el) return null; const r = el.getBoundingClientRect(); return [r.left + r.width / 2, r.top + r.height / 2]; }})()",
                    serde_json::to_string(&sel).unwrap()
                );
                if let Ok(ev) = page.evaluate(center_js.as_str()).await {
                    if let Ok(pt) = ev.into_value::<Vec<f64>>() {
                        if pt.len() == 2 {
                            s.set_cursor(tab_id, pt[0], pt[1], "click", &b.by).await;
                        }
                    }
                }
                let res: Result<(), String> = if kind == "double" {
                    el.click_with(chromiumoxide::types::ClickOptions::builder().click_count(2).build()).await.map(|_| ()).map_err(|e| format!("{e:#}"))
                } else if kind == "right" {
                    right_click_element(&page, &el).await
                } else {
                    el.click().await.map(|_| ()).map_err(|e| format!("{e:#}"))
                };
                match res {
                    Ok(_) => {
                        // Brief settle so SPAs commit DOM updates; the caller
                        // (UI poll / next snapshot) observes the rest. Kept short
                        // on purpose — this sleep is on the latency hot path.
                        tokio::time::sleep(Duration::from_millis(150)).await;
                        Ok(format!("clicked {sel}{label}"))
                    }
                    Err(e) => Err(friendly_cdp_error(s.port, "click failed", e)),
                }
            }
            Err(_) => {
                // Main-frame lookup failed: the element may live in a
                // same-origin iframe (snapshot tags those too). Resolve its
                // viewport coordinates and click there instead.
                match ref_coords(&page, &b.target).await {
                    Some((x, y)) => {
                        s.set_cursor(tab_id, x, y, "click", &b.by).await;
                        let point = chromiumoxide::layout::Point::new(x, y);
                        let res: Result<(), String> = if kind == "double" {
                            page.click_with(point, chromiumoxide::types::ClickOptions::builder().click_count(2).build()).await.map(|_| ()).map_err(|e| format!("{e:#}"))
                        } else if kind == "right" {
                            right_click_point(&page, point).await
                        } else {
                            page.click(point).await.map(|_| ()).map_err(|e| format!("{e:#}"))
                        };
                        match res {
                            Ok(_) => {
                                tokio::time::sleep(Duration::from_millis(150)).await;
                                Ok(format!("clicked {sel}{label} (via coordinates — element is inside an iframe)"))
                            }
                            Err(e) => Err(friendly_cdp_error(s.port, "click failed", e)),
                        }
                    }
                    None => Err(format!(
                        "element not found for `{sel}` (run `lb snapshot` first)"
                    )),
                }
            }
        }
    };
    match tokio::time::timeout(CDP_TIMEOUT, do_click).await {
        Ok(Ok(msg)) => ok(&msg),
        Ok(Err(e)) => err(e),
        Err(_) => err(format!(
            "click timed out after {}s — the page may be stuck; retry or reload",
            CDP_TIMEOUT.as_secs()
        )),
    }
}

/// Viewport coordinates of a snapshot ref (main frame or same-origin
/// iframe), or None when the ref doesn't exist.
async fn ref_coords(page: &Page, target: &str) -> Option<(f64, f64)> {
    let t = target.trim();
    if t.starts_with("css=") {
        return None;
    }
    let js = format!("{REF_COORDS_JS}({})", serde_json::to_string(t).unwrap());
    let ev = tokio::time::timeout(CDP_TIMEOUT, page.evaluate(js.as_str())).await.ok()?.ok()?;
    let pt: Vec<f64> = ev.into_value().ok()?;
    if pt.len() == 2 && pt[0].is_finite() && pt[1].is_finite() {
        Some((pt[0], pt[1]))
    } else {
        None
    }
}

/// Right-click at a viewport point via raw CDP mouse events.
async fn right_click_point(page: &Page, point: chromiumoxide::layout::Point) -> Result<(), String> {
    use chromiumoxide::cdp::browser_protocol::input::{DispatchMouseEventParams, DispatchMouseEventType, MouseButton};
    page.execute(
        DispatchMouseEventParams::builder()
            .r#type(DispatchMouseEventType::MousePressed)
            .x(point.x)
            .y(point.y)
            .button(MouseButton::Right)
            .buttons(2)
            .build()
            .unwrap(),
    )
    .await
    .map(|_| ())
    .map_err(|e| format!("{e:#}"))?;
    page.execute(
        DispatchMouseEventParams::builder()
            .r#type(DispatchMouseEventType::MouseReleased)
            .x(point.x)
            .y(point.y)
            .button(MouseButton::Right)
            .buttons(0)
            .build()
            .unwrap(),
    )
    .await
    .map(|_| ())
    .map_err(|e| format!("{e:#}"))?;
    Ok(())
}

/// Right-click an element: resolve its clickable point, then dispatch.
async fn right_click_element(page: &Page, el: &chromiumoxide::Element) -> Result<(), String> {
    el.scroll_into_view().await.map_err(|e| format!("{e:#}"))?;
    let pt = el.clickable_point().await.map_err(|e| format!("{e:#}"))?;
    right_click_point(page, chromiumoxide::layout::Point::new(pt.x, pt.y)).await
}

#[derive(Deserialize)]
struct TypeReq {
    /// Tab id (default: active tab).
    #[serde(default)]
    tab: Option<u32>,
    target: String,
    text: String,
    #[serde(default)]
    submit: bool,
    /// Keep existing content and append (default clears the field first).
    #[serde(default)]
    append: bool,
}
async fn api_type(State(s): State<AppState>, Json(b): Json<TypeReq>) -> Json<serde_json::Value> {
    let sel = target_to_selector(&b.target);
    let (_tab_id, page) = tab!(&s, b.tab);
    s.push_log("type", &format!("{sel} <- {:?}", b.text));
    let run = async {
        if !b.target.trim().starts_with("css=") && b.target.trim().parse::<u32>().is_ok() {
            let _ = tokio::time::timeout(CDP_TIMEOUT, page.evaluate(SNAPSHOT_JS)).await;
        }
        match page.find_element(&sel).await {
            Ok(el) => type_into_element(&page, &el, &sel, &b.text, b.submit, b.append).await,
            Err(_) => {
                // Main-frame lookup failed — the element may live in a
                // same-origin iframe. Focus it via JS, then drive it with
                // trusted input (works across frame boundaries).
                let focus_js = format!(
                    "{FOCUS_REF_JS}({}, {})",
                    serde_json::to_string(b.target.trim()).unwrap(),
                    if b.append { "false" } else { "true" }
                );
                let focused = tokio::time::timeout(CDP_TIMEOUT, page.evaluate(focus_js.as_str())).await;
                match focused {
                    Ok(Ok(ev)) => match ev.into_value::<String>() {
                        Ok(which) if which == "ok" => {
                            insert_text(&page, &b.text).await?;
                            if b.submit {
                                press_enter(&page).await?;
                            }
                            tokio::time::sleep(Duration::from_millis(150)).await;
                            Ok(format!("typed into {sel} (inside an iframe)"))
                        }
                        _ => Err(format!("element not found for `{sel}` (run `lb snapshot` first)")),
                    },
                    _ => Err(format!("element not found for `{sel}` (run `lb snapshot` first)")),
                }
            }
        }
    };
    match tokio::time::timeout(CDP_TIMEOUT, run).await {
        Ok(Ok(msg)) => ok(&msg),
        Ok(Err(e)) => err(e),
        Err(_) => err(format!(
            "type timed out after {}s — the page may be stuck; retry or reload",
            CDP_TIMEOUT.as_secs()
        )),
    }
}

/// Core keystroke path for an already-resolved main-frame element.
async fn type_into_element(
    page: &Page,
    el: &chromiumoxide::Element,
    sel: &str,
    text: &str,
    submit: bool,
    append: bool,
) -> Result<String, String> {
    if let Err(e) = el.click().await {
        return Err(format!("focus failed: {e:#}"));
    }
    if !append {
        // Clear existing content via JS, then type (typing triggers real key events).
        let clear_js = format!(
            "(() => {{ const el = document.querySelector({}); if (el) {{ el.focus(); if ('value' in el) {{ el.value=''; el.dispatchEvent(new Event('input', {{bubbles:true}})); }} }} }})()",
            serde_json::to_string(&sel).unwrap()
        );
        let _ = page.evaluate(clear_js.as_str()).await;
    }
    // Type line-by-line: '\n' has no key definition, so press Enter between
    // lines. Leading whitespace is stripped and left to the editor's
    // auto-indent (typing it verbatim would double-indent); electric
    // characters like `}` re-indent themselves in code editors.
    // Single-line text keeps its exact spacing.
    if !text.contains('\n') {
        if let Err(e) = el.type_str(text).await {
            return Err(format!("type failed: {e:#}"));
        }
    } else {
        let mut first = true;
        for line in text.replace('\t', "    ").split('\n') {
            if !first {
                if let Err(e) = el.press_key("Enter").await {
                    return Err(format!("newline failed: {e:#}"));
                }
            }
            first = false;
            let content = line.trim_start();
            if !content.is_empty() {
                if let Err(e) = el.type_str(content).await {
                    return Err(format!("type failed: {e:#}"));
                }
            }
        }
    }
    if submit {
        press_enter(page).await?;
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    Ok(format!("typed into {sel}"))
}

/// Trusted bulk-text insertion at the current cursor (survives newlines and
/// any characters; also drives the frame-focused fallback path).
async fn insert_text(page: &Page, text: &str) -> Result<(), String> {
    use chromiumoxide::cdp::browser_protocol::input::InsertTextParams;
    if text.is_empty() {
        return Ok(());
    }
    page.execute(InsertTextParams::new(text.to_string()))
        .await
        .map(|_| ())
        .map_err(|e| format!("type failed: {e:#}"))
}

async fn press_enter(page: &Page) -> Result<(), String> {
    use chromiumoxide::cdp::browser_protocol::input::{DispatchKeyEventParams, DispatchKeyEventType};
    let params = DispatchKeyEventParams::builder()
        .r#type(DispatchKeyEventType::RawKeyDown)
        .key("Enter".to_string())
        .code("Enter".to_string())
        .windows_virtual_key_code(13)
        .native_virtual_key_code(13)
        .build()
        .unwrap();
    page.execute(params)
        .await
        .map(|_| ())
        .map_err(|e| format!("submit (Enter) failed: {e:#}"))
}

#[derive(Deserialize)]
struct PressReq {
    /// Tab id (default: active tab).
    #[serde(default)]
    tab: Option<u32>,
    key: String,
}
async fn api_press(State(s): State<AppState>, Json(b): Json<PressReq>) -> Json<serde_json::Value> {
    let (_tab_id, page) = tab!(&s, b.tab);
    s.push_log("press", &b.key);
    let run = async {
        // Modifier chords ("ctrl+c", "ctrl+shift+t", "alt+F4", "cmd+v"):
        // split into modifiers + final key and dispatch raw CDP key events.
        if b.key.contains('+') {
            return press_chord(&page, &b.key)
                .await
                .map_err(|e| friendly_cdp_error(s.port, "press failed", e));
        }
        match tokio::time::timeout(CDP_TIMEOUT, page.find_element("body")).await {
            Ok(Ok(el)) => match tokio::time::timeout(CDP_TIMEOUT, el.press_key(b.key.as_str())).await {
                Ok(Ok(_)) => {
                    tokio::time::sleep(Duration::from_millis(80)).await;
                    Ok(format!("pressed {}", b.key))
                }
                Ok(Err(e)) => Err(friendly_cdp_error(s.port, "press failed", e)),
                Err(_) => Err(press_timeout()),
            },
            _ => Err(press_timeout()),
        }
    };
    match tokio::time::timeout(CDP_TIMEOUT, run).await {
        Ok(Ok(msg)) => ok(&msg),
        Ok(Err(e)) => err(e),
        Err(_) => err(press_timeout()),
    }
}

fn press_timeout() -> String {
    format!(
        "press timed out after {}s — the page may be stuck; retry or reload",
        CDP_TIMEOUT.as_secs()
    )
}

/// (CDP key, code, Windows virtual-key code) for keys usable in chords and
/// single presses. Letters/digits are derived programmatically; this covers
/// the named non-printables.
fn named_key(key: &str) -> Option<(&'static str, &'static str, i64)> {
    Some(match key {
        "Enter" => ("Enter", "Enter", 13),
        "Tab" => ("Tab", "Tab", 9),
        "Escape" | "Esc" => ("Escape", "Escape", 27),
        "Backspace" => ("Backspace", "Backspace", 8),
        "Delete" | "Del" => ("Delete", "Delete", 46),
        " " | "Space" | "Spacebar" => (" ", "Space", 32),
        "ArrowLeft" | "Left" => ("ArrowLeft", "ArrowLeft", 37),
        "ArrowUp" | "Up" => ("ArrowUp", "ArrowUp", 38),
        "ArrowRight" | "Right" => ("ArrowRight", "ArrowRight", 39),
        "ArrowDown" | "Down" => ("ArrowDown", "ArrowDown", 40),
        "Home" => ("Home", "Home", 36),
        "End" => ("End", "End", 35),
        "PageUp" => ("PageUp", "PageUp", 33),
        "PageDown" => ("PageDown", "PageDown", 34),
        "F1" => ("F1", "F1", 112),
        "F2" => ("F2", "F2", 113),
        "F3" => ("F3", "F3", 114),
        "F4" => ("F4", "F4", 115),
        "F5" => ("F5", "F5", 116),
        "F6" => ("F6", "F6", 117),
        "F7" => ("F7", "F7", 118),
        "F8" => ("F8", "F8", 119),
        "F9" => ("F9", "F9", 120),
        "F10" => ("F10", "F10", 121),
        "F11" => ("F11", "F11", 122),
        "F12" => ("F12", "F12", 123),
        _ => return None,
    })
}

/// Resolve a single key to (key, code, key_code, text): printable ASCII
/// first (letters/digits/punctuation via US layout rules), then named keys.
fn resolve_key(key: &str) -> Option<(String, String, i64, Option<String>)> {
    if key.chars().count() == 1 {
        let c = key.chars().next().unwrap();
        if c.is_ascii_alphabetic() {
            let upper = c.to_ascii_uppercase();
            return Some((upper.to_string(), format!("Key{upper}"), upper as i64, Some(key.to_string())));
        }
        if c.is_ascii_digit() {
            return Some((key.to_string(), format!("Digit{key}"), c as i64, Some(key.to_string())));
        }
        if c.is_ascii_graphic() || c == ' ' {
            return Some((key.to_string(), String::new(), 0, Some(key.to_string())));
        }
        return None;
    }
    named_key(key).map(|(k, code, kc)| (k.to_string(), code.to_string(), kc, None))
}

/// Press a modifier chord: e.g. "ctrl+c", "ctrl+shift+t", "alt+F4".
/// Modifier names: ctrl/control, alt, shift, meta/cmd/command/win.
async fn press_chord(page: &Page, chord: &str) -> Result<String, String> {
    use chromiumoxide::cdp::browser_protocol::input::{DispatchKeyEventParams, DispatchKeyEventType};
    let parts: Vec<&str> = chord.split('+').map(|p| p.trim()).filter(|p| !p.is_empty()).collect();
    if parts.len() < 2 {
        return Err(format!("malformed chord {chord:?} — use e.g. ctrl+c"));
    }
    let mut mask: i64 = 0;
    let mut mods: Vec<(&str, &str, i64)> = vec![];
    for m in &parts[..parts.len() - 1] {
        let lower = m.to_lowercase();
        let def = match lower.as_str() {
            "ctrl" | "control" => {
                mask |= 2;
                ("Control", "ControlLeft", 17)
            }
            "alt" | "option" => {
                mask |= 1;
                ("Alt", "AltLeft", 18)
            }
            "shift" => {
                mask |= 8;
                ("Shift", "ShiftLeft", 16)
            }
            "meta" | "cmd" | "command" | "win" | "super" => {
                mask |= 4;
                ("Meta", "MetaLeft", 91)
            }
            _ => return Err(format!("unknown modifier {m:?} in {chord:?} (use ctrl, alt, shift, meta)")),
        };
        mods.push(def);
    }
    let key_part = parts[parts.len() - 1];
    // A trailing modifier ("ctrl+shift") just taps the modifiers together.
    let key_def = match parts[parts.len() - 1].to_lowercase().as_str() {
        "ctrl" | "control" | "alt" | "option" | "shift" | "meta" | "cmd" | "command" | "win" | "super" => {
            let lower = parts[parts.len() - 1].to_lowercase();
            let def = match lower.as_str() {
                "alt" | "option" => {
                    mask |= 1;
                    ("Alt", "AltLeft", 18)
                }
                "ctrl" | "control" => {
                    mask |= 2;
                    ("Control", "ControlLeft", 17)
                }
                "meta" | "cmd" | "command" | "win" | "super" => {
                    mask |= 4;
                    ("Meta", "MetaLeft", 91)
                }
                _ => {
                    mask |= 8;
                    ("Shift", "ShiftLeft", 16)
                }
            };
            mods.push(def);
            None
        }
        _ => Some(resolve_key(key_part).ok_or_else(|| {
            format!("unknown key {key_part:?} in {chord:?} (letters, digits, Enter, Tab, F1–F12, arrows, …)")
        })?),
    };
    let down = |key: &str, code: &str, kc: i64, modifiers: i64, text: Option<String>| {
        let mut cmd = DispatchKeyEventParams::builder()
            .r#type(DispatchKeyEventType::RawKeyDown)
            .modifiers(modifiers)
            .key(key.to_string())
            .windows_virtual_key_code(kc)
            .native_virtual_key_code(kc);
        if !code.is_empty() {
            cmd = cmd.code(code.to_string());
        }
        if let Some(t) = text {
            cmd = cmd.text(t.clone()).unmodified_text(t);
        }
        cmd.build().unwrap()
    };
    let up = |key: &str, code: &str, kc: i64, modifiers: i64| {
        let mut cmd = DispatchKeyEventParams::builder()
            .r#type(DispatchKeyEventType::KeyUp)
            .modifiers(modifiers)
            .key(key.to_string())
            .windows_virtual_key_code(kc)
            .native_virtual_key_code(kc);
        if !code.is_empty() {
            cmd = cmd.code(code.to_string());
        }
        cmd.build().unwrap()
    };
    // Modifier key-downs first (each sees the modifiers pressed so far).
    let mut seen: i64 = 0;
    for (k, code, kc) in &mods {
        seen |= match *k {
            "Alt" => 1,
            "Control" => 2,
            "Meta" => 4,
            _ => 8,
        };
        page.execute(down(k, code, *kc, seen, None))
            .await
            .map_err(|e| format!("press failed: {e:#}"))?;
    }
    if let Some((k, code, kc, text)) = key_def {
        // With Ctrl held, the key must NOT carry text (else it types "c"
        // instead of Copy); without Ctrl, text makes it printable.
        let with_text = if mask & 2 != 0 { None } else { text };
        page.execute(down(&k, &code, kc, mask, with_text))
            .await
            .map_err(|e| format!("press failed: {e:#}"))?;
        page.execute(up(&k, &code, kc, mask))
            .await
            .map_err(|e| format!("press failed: {e:#}"))?;
    }
    for (k, code, kc) in mods.iter().rev() {
        let rest = mask
            & !(match *k {
                "Alt" => 1,
                "Control" => 2,
                "Meta" => 4,
                _ => 8,
            });
        page.execute(up(k, code, *kc, rest))
            .await
            .map_err(|e| format!("press failed: {e:#}"))?;
    }
    tokio::time::sleep(Duration::from_millis(80)).await;
    Ok(format!("pressed {chord}"))
}

#[derive(Deserialize)]
struct EvalReq {
    /// Tab id (default: active tab).
    #[serde(default)]
    tab: Option<u32>,
    js: String,
}
async fn api_eval(State(s): State<AppState>, Json(b): Json<EvalReq>) -> Json<serde_json::Value> {
    let (tab_id, page) = tab!(&s, b.tab);
    s.push_log("eval", &b.js);
    match tokio::time::timeout(CDP_TIMEOUT, page.evaluate(b.js.as_str())).await {
        Ok(Ok(r)) => match r.into_value::<serde_json::Value>() {
            Ok(v) => Json(serde_json::json!({ "ok": true, "tab": tab_id, "result": v })),
            Err(_) => Json(serde_json::json!({ "ok": true, "result": "(non-JSON value — side effects applied)" })),
        },
        Ok(Err(e)) => err(friendly_js_error(s.port, e)),
        Err(_) => err(format!(
            "eval timed out after {}s — the page may be blocked (check `lb log` for auto-dismissed dialogs)",
            CDP_TIMEOUT.as_secs()
        )),
    }
}

/// Turn a `page.evaluate` failure into something a human/agent can act on.
/// chromiumoxide surfaces JS exceptions as a Rust-Debug dump of
/// `ExceptionDetails` (unreadable); extract the JS `description`
/// (`SyntaxError: …`, `ReferenceError: …`) when present.
fn friendly_js_error(port: u16, e: impl std::fmt::Debug + std::fmt::Display) -> String {
    let disp = format!("{e:#}");
    tracing::warn!("CDP failure (eval): {}", disp.chars().take(300).collect::<String>());
    let display = disp.as_str();
    if is_tab_gone_msg(display) {
        return tab_gone_msg(port);
    }
    if is_browser_gone_msg(display) {
        return gone_err(port)
            .get("error")
            .and_then(|x| x.as_str())
            .unwrap_or("browser process died")
            .to_string();
    }
    let debug = format!("{e:?}");
    for key in ["description: Some(\"", "text: \""] {
        if let Some(start) = debug.find(key) {
            let rest = &debug[start + key.len()..];
            // Description is a Rust-escaped string; unescape the basics.
            let mut out = String::new();
            let mut chars = rest.chars();
            let mut closed = false;
            while let Some(c) = chars.next() {
                if c == '"' {
                    closed = true;
                    break;
                }
                if c == '\\' {
                    match chars.next() {
                        Some('n') => out.push('\n'),
                        Some('t') => out.push('\t'),
                        Some('r') => out.push('\r'),
                        Some('"') => out.push('"'),
                        Some('\\') => out.push('\\'),
                        Some(o) => {
                            out.push('\\');
                            out.push(o);
                        }
                        None => break,
                    }
                } else {
                    out.push(c);
                }
                if out.len() > 500 {
                    break;
                }
            }
            if closed && !out.is_empty() {
                return format!("eval failed: {out}");
            }
        }
    }
    let short: String = display.chars().take(400).collect();
    format!("eval failed: {short}")
}

#[derive(Deserialize)]
struct NavReq {
    /// Tab id (default: active tab).
    #[serde(default)]
    tab: Option<u32>,
    action: String,
}
async fn api_nav(State(s): State<AppState>, Json(b): Json<NavReq>) -> Json<serde_json::Value> {
    let (tab_id, page) = tab!(&s, b.tab);
    s.push_log(&b.action, &format!("tab {tab_id}"));
    // Reloading a chrome-error page just redisplays the error — recover by
    // re-opening the last healthy URL instead.
    if b.action == "reload" {
        let current = page_url(&page).await;
        if current.starts_with("chrome-error") {
            let last = s.tabs.lock().await.map.get(&tab_id).map(|ts| ts.last_url.clone()).unwrap_or_default();
            if last.is_empty() {
                return err("error page with no previous URL — use open instead".into());
            }
            s.push_log("reload/recover", &last);
            let goto = page.goto(&last).await;
            tokio::time::sleep(Duration::from_millis(1500)).await;
            let landed = page_url(&page).await;
            if goto.is_ok() || !landed.starts_with("chrome-error") {
                if !landed.starts_with("chrome-error") && !landed.is_empty() {
                    with_tab(&s, tab_id, |ts| ts.last_url = landed.clone()).await;
                }
                let zoom = s.tabs.lock().await.map.get(&tab_id).map(|ts| ts.zoom).unwrap_or(1.0);
                apply_zoom(&page, zoom).await;
                return ok(&format!("recovered to {landed}"));
            }
            return err("still failing — the network is flapping, try again in a bit".into());
        }
    }
    let js = match b.action.as_str() {
        "back" => "history.back()",
        "forward" => "history.forward()",
        "reload" => "location.reload()",
        other => return err(format!("unknown nav action: {other} (use back, forward or reload)")),
    };
    match tokio::time::timeout(CDP_TIMEOUT, page.evaluate(js)).await {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => return err(friendly_cdp_error(s.port, &format!("navigated {}", b.action), e)),
        Err(_) => {
            return err(format!(
                "nav {} timed out after {}s — the page is likely still navigating; re-check `lb status`",
                b.action,
                CDP_TIMEOUT.as_secs()
            ));
        }
    }
    tokio::time::sleep(Duration::from_millis(150)).await;
    let zoom = s.tabs.lock().await.map.get(&tab_id).map(|ts| ts.zoom).unwrap_or(1.0);
    apply_zoom(&page, zoom).await;
    ok(&format!("navigated {}", b.action))
}

#[derive(Deserialize)]
struct ClickAtReq {
    /// Tab id (default: active tab).
    #[serde(default)]
    tab: Option<u32>,
    x: f64,
    y: f64,
    /// "human" when triggered from the web UI; anything else = agent.
    #[serde(default)]
    by: String,
    /// "single" (default), "double" or "right".
    #[serde(default = "default_click_kind")]
    kind: String,
}
/// Click at viewport CSS-pixel coordinates (used by the clickable screenshot).
async fn api_click_at(State(s): State<AppState>, Json(b): Json<ClickAtReq>) -> Json<serde_json::Value> {
    if !b.x.is_finite() || !b.y.is_finite() || b.x < 0.0 || b.y < 0.0 {
        return err("invalid coordinates".into());
    }
    let (tab_id, page) = tab!(&s, b.tab);
    let kind = b.kind.to_lowercase();
    if kind != "single" && kind != "double" && kind != "right" {
        return err(format!("unknown click kind: {:?} (use single, double or right)", b.kind));
    }
    s.push_log("click-at", &format!("{}, {} [{kind}]", b.x.round(), b.y.round()));
    s.set_cursor(tab_id, b.x, b.y, "click", &b.by).await;
    let point = chromiumoxide::layout::Point::new(b.x, b.y);
    let run = async {
        if kind == "double" {
            page.click_with(point, chromiumoxide::types::ClickOptions::builder().click_count(2).build()).await.map(|_| ()).map_err(|e| format!("{e:#}"))
        } else if kind == "right" {
            right_click_point(&page, point).await
        } else {
            page.click(point).await.map(|_| ()).map_err(|e| format!("{e:#}"))
        }
    };
    match tokio::time::timeout(CDP_TIMEOUT, run).await {
        Ok(Ok(_)) => {
            tokio::time::sleep(Duration::from_millis(120)).await;
            ok("clicked")
        }
        Ok(Err(e)) => err(friendly_cdp_error(s.port, "click failed", e)),
        Err(_) => err(format!("click timed out after {}s", CDP_TIMEOUT.as_secs())),
    }
}

#[derive(Deserialize)]
struct ScrollReq {
    /// Tab id (default: active tab).
    #[serde(default)]
    tab: Option<u32>,
    #[serde(default)]
    dx: f64,
    #[serde(default)]
    dy: f64,
}
async fn api_scroll(State(s): State<AppState>, Json(b): Json<ScrollReq>) -> Json<serde_json::Value> {
    if !b.dx.is_finite() || !b.dy.is_finite() {
        return err("invalid scroll delta".into());
    }
    let (_tab_id, page) = tab!(&s, b.tab);
    s.push_log("scroll", &format!("{}, {}", b.dx, b.dy));
    let js = format!("window.scrollBy({}, {})", b.dx, b.dy);
    match tokio::time::timeout(CDP_TIMEOUT, page.evaluate(js.as_str())).await {
        Ok(Ok(_)) => {
            tokio::time::sleep(Duration::from_millis(60)).await;
            ok("scrolled")
        }
        Ok(Err(e)) => err(friendly_cdp_error(s.port, "scroll failed", e)),
        Err(_) => err(format!("scroll timed out after {}s", CDP_TIMEOUT.as_secs())),
    }
}

#[derive(Deserialize)]
struct KeyReq {
    /// Tab id (default: active tab).
    #[serde(default)]
    tab: Option<u32>,
    /// "down" or "up"
    event: String,
    /// JS KeyboardEvent.key (e.g. "a", "Enter", "Backspace")
    #[serde(default)]
    key: String,
    /// JS KeyboardEvent.code (e.g. "KeyA", "Enter")
    #[serde(default)]
    code: String,
    /// JS keyCode (Windows virtual-key code for most keys)
    #[serde(default)]
    key_code: i64,
    /// Printable character to insert (single char)
    #[serde(default)]
    text: String,
    #[serde(default)]
    repeat: bool,
    /// Modifier bitmask: Alt=1, Ctrl=2, Meta=4, Shift=8
    #[serde(default)]
    modifiers: i64,
}
#[derive(Deserialize)]
struct SelectReq {
    /// Tab id (default: active tab).
    #[serde(default)]
    tab: Option<u32>,
    target: String,
    /// Option value (or visible text — matched case-insensitively as fallback).
    value: String,
}
/// Set a <select> option by value (or visible text). Native dropdown popups
/// don't work through CDP clicks, so this drives the element directly and
/// fires the same input/change events a real choice would.
async fn api_select(State(s): State<AppState>, Json(b): Json<SelectReq>) -> Json<serde_json::Value> {
    let sel = target_to_selector(&b.target);
    let (_tab_id, page) = tab!(&s, b.tab);
    s.push_log("select", &format!("{sel} <- {:?}", b.value));
    if !b.target.trim().starts_with("css=") && b.target.trim().parse::<u32>().is_ok() {
        let _ = page.evaluate(SNAPSHOT_JS).await;
    }
    const SET_JS: &str = r#"((sel, want) => {
      const el = document.querySelector(sel);
      if (!el || el.tagName !== 'SELECT') return { error: 'not a select element' };
      const wantLc = String(want).toLowerCase();
      const opt = Array.from(el.options).find(o =>
        o.value === want || o.text.trim() === want || o.value.toLowerCase() === wantLc || o.text.trim().toLowerCase() === wantLc);
      if (!opt) return { error: 'no such option: ' + want };
      el.value = opt.value;
      el.dispatchEvent(new Event('input', { bubbles: true }));
      el.dispatchEvent(new Event('change', { bubbles: true }));
      return { value: opt.value, text: opt.text.trim() };
    })"#;
    let call = format!(
        "{SET_JS}({}, {})",
        serde_json::to_string(&sel).unwrap(),
        serde_json::to_string(&b.value).unwrap()
    );
    match tokio::time::timeout(CDP_TIMEOUT, page.evaluate(call.as_str())).await {
        Ok(Ok(r)) => match r.into_value::<serde_json::Value>() {
            Ok(v) => {
                if v.get("error").and_then(|x| x.as_str()).is_some() {
                    // Fall back to keyboard-driving the native control.
                    return select_via_keyboard(&page, &sel, &b.value).await;
                }
                tokio::time::sleep(Duration::from_millis(120)).await;
                Json(serde_json::json!({ "ok": true,
                    "message": format!("selected {:?}", v.get("text").and_then(|x| x.as_str()).unwrap_or(&b.value)) }))
            }
            Err(e) => err(format!("decode select failed: {e:#}")),
        },
        Ok(Err(e)) => err(friendly_cdp_error(s.port, "select failed", e)),
        Err(_) => err(format!("select timed out after {}s", CDP_TIMEOUT.as_secs())),
    }
}

/// Fallback: focus the select and type the option text (native control
/// type-ahead), confirming with Enter.
async fn select_via_keyboard(page: &Page, sel: &str, want: &str) -> Json<serde_json::Value> {
    let el = match page.find_element(sel).await {
        Ok(el) => el,
        Err(e) => return err(format!("element not found for `{sel}`: {e:#}")),
    };
    if el.click().await.is_err() {
        return err("focus failed".into());
    }
    for c in want.chars().take(40) {
        let _ = el.press_key(c.to_string()).await;
    }
    let _ = el.press_key("Enter").await;
    tokio::time::sleep(Duration::from_millis(120)).await;
    ok(&format!("selected (via keyboard) {want:?}"))
}

/// Forward one raw keyboard event to the page (powers UI keyboard-capture mode).
async fn api_key(State(s): State<AppState>, Json(b): Json<KeyReq>) -> Json<serde_json::Value> {
    use chromiumoxide::cdp::browser_protocol::input::{DispatchKeyEventParams, DispatchKeyEventType};
    let is_up = b.event.eq_ignore_ascii_case("up");
    let is_down = b.event.eq_ignore_ascii_case("down") || b.event.eq_ignore_ascii_case("press");
    if !is_up && !is_down {
        return err("event must be \"down\" or \"up\"".into());
    }
    if b.key.is_empty() && b.text.is_empty() {
        return err("key or text required".into());
    }
    // Mirror what a real browser sends: printable chars as keyDown+text (which
    // inserts the char), everything else as rawKeyDown; keyUp needs no text.
    let ty = if is_up {
        DispatchKeyEventType::KeyUp
    } else if b.text.chars().count() == 1 {
        DispatchKeyEventType::KeyDown
    } else {
        DispatchKeyEventType::RawKeyDown
    };
    let mut cmd = DispatchKeyEventParams::builder()
        .r#type(ty)
        .modifiers(b.modifiers)
        .auto_repeat(b.repeat);
    if !b.key.is_empty() {
        cmd = cmd.key(b.key.clone());
    }
    if !b.code.is_empty() {
        cmd = cmd.code(b.code.clone());
    }
    if b.key_code != 0 {
        cmd = cmd
            .windows_virtual_key_code(b.key_code)
            .native_virtual_key_code(b.key_code);
    }
    if is_down && b.text.chars().count() == 1 {
        cmd = cmd.text(b.text.clone()).unmodified_text(b.text.clone());
    }
    let (_tab_id, page) = tab!(&s, b.tab);
    // Keystrokes are intentionally not logged: at ~2 events per key they'd
    // drown the action log. Failures still surface via the API response.
    match page.execute(cmd.build().unwrap()).await {
        Ok(_) => Json(serde_json::json!({ "ok": true })),
        Err(e) => err(format!("key event failed: {e:#}")),
    }
}

#[derive(Deserialize)]
struct ZoomReq {
    /// Tab id (default: active tab).
    #[serde(default)]
    tab: Option<u32>,
    /// "in", "out" or "reset"
    #[serde(default = "default_zoom_action")]
    action: String,
}
fn default_zoom_action() -> String {
    "in".to_string()
}
async fn api_zoom(State(s): State<AppState>, Json(b): Json<ZoomReq>) -> Json<serde_json::Value> {
    let (tab_id, page) = tab!(&s, b.tab);
    let action = b.action.to_lowercase();
    if action != "in" && action != "out" && action != "reset" {
        return err(format!("unknown zoom action: {:?} (use in, out or reset)", b.action));
    }
    let zoom = {
        let mut t = s.tabs.lock().await;
        let ts = t.map.get_mut(&tab_id).unwrap();
        ts.zoom = step_zoom(ts.zoom, action.as_str());
        ts.zoom
    };
    s.push_log("zoom", &format!("tab {tab_id} -> {zoom}"));
    apply_zoom(&page, zoom).await;
    tokio::time::sleep(Duration::from_millis(60)).await;
    Json(serde_json::json!({ "ok": true, "zoom": zoom, "message": format!("zoom {zoom}") }))
}

#[derive(Deserialize)]
struct InsertReq {
    /// Tab id (default: active tab).
    #[serde(default)]
    tab: Option<u32>,
    text: String,
}
/// Insert text at the cursor as if typed (single trusted `Input.insertText`).
/// Focus the target first (click it) — replaces any current selection, so it
/// doubles as fast paste into editors like Monaco/CodeMirror.
async fn api_insert(State(s): State<AppState>, Json(b): Json<InsertReq>) -> Json<serde_json::Value> {
    use chromiumoxide::cdp::browser_protocol::input::InsertTextParams;
    if b.text.is_empty() {
        return err("empty text".into());
    }
    let (_tab_id, page) = tab!(&s, b.tab);
    s.push_log("insert", &format!("{} chars", b.text.chars().count()));
    match tokio::time::timeout(CDP_TIMEOUT, page.execute(InsertTextParams::new(b.text))).await {
        Ok(Ok(_)) => {
            tokio::time::sleep(Duration::from_millis(60)).await;
            ok("inserted")
        }
        Ok(Err(e)) => err(friendly_cdp_error(s.port, "insert failed", e)),
        Err(_) => err(format!("insert timed out after {}s", CDP_TIMEOUT.as_secs())),
    }
}

#[derive(Deserialize)]
struct TypeRawReq {
    /// Tab id (default: active tab).
    #[serde(default)]
    tab: Option<u32>,
    text: String,
}
/// Type text as individual trusted keystrokes (one `Input.insertText` per char).
/// Slower than /api/insert but indistinguishable from real typing — use for
/// editors (Monaco/CodeMirror) that reformat or reject bulk pastes.
async fn api_type_raw(State(s): State<AppState>, Json(b): Json<TypeRawReq>) -> Json<serde_json::Value> {
    use chromiumoxide::cdp::browser_protocol::input::InsertTextParams;
    if b.text.is_empty() {
        return err("empty text".into());
    }
    let (_tab_id, page) = tab!(&s, b.tab);
    s.push_log("type-raw", &format!("{} chars", b.text.chars().count()));
    for c in b.text.chars() {
        let s_ = c.to_string();
        // CDP insertText needs at least one char; newlines are fine.
        if s_.is_empty() {
            continue;
        }
        if let Err(e) = page.execute(InsertTextParams::new(s_)).await {
            return err(format!("type-raw failed: {e:#}"));
        }
    }
    tokio::time::sleep(Duration::from_millis(60)).await;
    ok("typed")
}

#[derive(Deserialize)]
struct WaitReq {
    /// Tab id (default: active tab).
    #[serde(default)]
    tab: Option<u32>,
    /// Snapshot ref ("12"), `css=` selector or plain CSS to wait for.
    target: String,
    /// Poll interval seconds is fixed (0.5s); total wait budget:
    #[serde(default = "default_wait_timeout")]
    timeout: u64,
    /// When true, wait until the target *disappears* instead of appears.
    #[serde(default)]
    gone: bool,
}
fn default_wait_timeout() -> u64 {
    15
}
/// Block (server-side polling) until an element appears — or disappears with
/// `"gone": true`. Replaces agent sleep/retry loops and kills the
/// snapshot-right-after-navigation race.
async fn api_wait(State(s): State<AppState>, Json(b): Json<WaitReq>) -> Json<serde_json::Value> {
    let (tab_id, page) = tab!(&s, b.tab);
    let budget = b.timeout.clamp(1, 120);
    let probe_js = format!(
        "{WAIT_PROBE_PREFIX}({})",
        serde_json::to_string(&b.target).unwrap()
    );
    s.push_log(
        "wait",
        &format!("tab {tab_id} <- {:?}{}", b.target, if b.gone { " (gone)" } else { "" }),
    );
    // Tag refs first so waiting on a snapshot ref works right after navigation.
    let _ = tokio::time::timeout(CDP_TIMEOUT, page.evaluate(SNAPSHOT_JS)).await;
    let start = std::time::Instant::now();
    loop {
        let present = match tokio::time::timeout(CDP_TIMEOUT, page.evaluate(probe_js.as_str())).await {
            Ok(Ok(ev)) => ev.into_value::<bool>().unwrap_or(false),
            Ok(Err(e)) => {
                let msg = format!("{e:#}");
                if is_tab_gone_msg(&msg) {
                    return err(tab_gone_msg(s.port));
                }
                if is_browser_gone_msg(&msg) {
                    return Json(gone_err(s.port));
                }
                false
            }
            Err(_) => false,
        };
        if present != b.gone {
            let after = start.elapsed().as_secs_f64();
            return Json(serde_json::json!({ "ok": true, "tab": tab_id,
                "message": format!("wait satisfied after {after:.1}s ({} {})",
                    b.target, if b.gone { "gone" } else { "present" }) }));
        }
        if start.elapsed().as_secs() >= budget {
            return err(format!(
                "wait timed out after {budget}s: {:?} never {} (page may still be loading — raise --timeout or re-check `lb snapshot`)",
                b.target,
                if b.gone { "disappeared" } else { "appeared" }
            ));
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

#[derive(Deserialize)]
struct ContentReq {
    /// Tab id (default: active tab).
    #[serde(default)]
    tab: Option<u32>,
    /// Snapshot ref, `css=` selector or plain CSS. Default: whole page body.
    #[serde(default)]
    target: Option<String>,
    /// Max characters returned (default 8000, cap 100000).
    #[serde(default = "default_content_max")]
    max: usize,
}
fn default_content_max() -> usize {
    8000
}

/// Shared implementation for /api/text and /api/html.
async fn read_content(
    s: &AppState,
    tab: Option<u32>,
    target: &Option<String>,
    max: usize,
    kind: &str,
) -> Json<serde_json::Value> {
    let (tab_id, page) = tab!(s, tab);
    let max = max.clamp(1, 100_000);
    let prop = if kind == "text" { "\"innerText\"" } else { "\"outerHTML\"" };
    let js = match target {
        Some(t) => {
            let sel = target_to_selector(t);
            format!(
                "(() => {{ const el = document.querySelector({sel}); if (el) return el[{prop}]; \
                 const frames = document.querySelectorAll('iframe'); \
                 for (const f of frames) {{ try {{ const d = f.contentDocument; if (d) {{ const hit = d.querySelector({sel}); if (hit) return hit[{prop}]; }} }} catch (e) {{}} }} \
                 return null; }})()",
                sel = serde_json::to_string(&sel).unwrap()
            )
        }
        None => format!("document.body ? document.body[{prop}] : document.documentElement[{prop}]"),
    };
    match tokio::time::timeout(CDP_TIMEOUT, page.evaluate(js.as_str())).await {
        Ok(Ok(ev)) => match ev.into_value::<Option<String>>() {
            Ok(Some(content)) => {
                let truncated = content.len() > max;
                let clipped: String = content.chars().take(max).collect();
                Json(serde_json::json!({ "ok": true, "tab": tab_id,
                    "content": clipped, "truncated": truncated, "chars": content.len() }))
            }
            _ => err(format!(
                "no such element (run `lb snapshot` first){}",
                target.as_ref().map(|t| format!(": {t}")).unwrap_or_default()
            )),
        },
        Ok(Err(e)) => err(friendly_cdp_error(s.port, &format!("{kind} failed"), e)),
        Err(_) => err(format!("{kind} timed out after {}s", CDP_TIMEOUT.as_secs())),
    }
}

async fn api_text(State(s): State<AppState>, Json(b): Json<ContentReq>) -> Json<serde_json::Value> {
    s.push_log(
        "text",
        &format!("tab {} <- {}", b.tab.unwrap_or(0), b.target.as_deref().unwrap_or("body")),
    );
    read_content(&s, b.tab, &b.target, b.max, "text").await
}

async fn api_html(State(s): State<AppState>, Json(b): Json<ContentReq>) -> Json<serde_json::Value> {
    s.push_log(
        "html",
        &format!("tab {} <- {}", b.tab.unwrap_or(0), b.target.as_deref().unwrap_or("body")),
    );
    read_content(&s, b.tab, &b.target, b.max, "html").await
}

#[derive(Deserialize)]
struct UploadReq {
    /// Tab id (default: active tab).
    #[serde(default)]
    tab: Option<u32>,
    /// Snapshot ref or CSS selector of an `<input type=file>`.
    target: String,
    /// Local file path to attach (must exist on the machine running `lb web`).
    file: String,
}
/// Attach a local file to a file input via CDP `DOM.setFileInputFiles` —
/// the only automation-safe way (typing a path into the input does nothing).
async fn api_upload(State(s): State<AppState>, Json(b): Json<UploadReq>) -> Json<serde_json::Value> {
    use chromiumoxide::cdp::browser_protocol::dom::SetFileInputFilesParams;
    let sel = target_to_selector(&b.target);
    let (_tab_id, page) = tab!(&s, b.tab);
    let abs = match std::fs::canonicalize(&b.file) {
        Ok(p) => p,
        Err(_) => return err(format!("file not found: {:?} (path is resolved on the `lb web` machine)", b.file)),
    };
    if !abs.is_file() {
        return err(format!("not a file: {}", abs.display()));
    }
    s.push_log("upload", &format!("{sel} <- {}", abs.display()));
    let run = async {
        if !b.target.trim().starts_with("css=") && b.target.trim().parse::<u32>().is_ok() {
            let _ = tokio::time::timeout(CDP_TIMEOUT, page.evaluate(SNAPSHOT_JS)).await;
        }
        let el = page
            .find_element(&sel)
            .await
            .map_err(|_| format!("element not found for `{sel}` (run `lb snapshot` first)"))?;
        // Refuse non-file inputs with a clear message instead of a silent no-op.
        let kind_js = format!(
            "(() => {{ const el = document.querySelector({}); if (!el) return 'missing'; return (el.tagName + '/' + (el.getAttribute('type') || '')).toLowerCase(); }})()",
            serde_json::to_string(&sel).unwrap()
        );
        let kind: String = page
            .evaluate(kind_js.as_str())
            .await
            .map_err(|e| format!("upload probe failed: {e:#}"))?
            .into_value()
            .unwrap_or_default();
        if kind != "input/file" {
            return Err(format!("`{sel}` is {kind}, not a file input (need <input type=file>)"));
        }
        let params = SetFileInputFilesParams::builder()
            .files(vec![abs.to_string_lossy().to_string()])
            .backend_node_id(el.backend_node_id)
            .build()
            .map_err(|e| format!("upload failed to build request: {e:?}"))?;
        page.execute(params)
            .await
            .map(|_| ())
            .map_err(|e| format!("upload failed: {e:#}"))?;
        tokio::time::sleep(Duration::from_millis(150)).await;
        Ok(format!("attached {} to {sel}", abs.display()))
    };
    match tokio::time::timeout(CDP_TIMEOUT, run).await {
        Ok(Ok(msg)) => ok(&msg),
        Ok(Err(e)) => err(e),
        Err(_) => err(format!("upload timed out after {}s", CDP_TIMEOUT.as_secs())),
    }
}

/// Files downloaded through the browser (see download behavior set at
/// startup), newest first. Downloads previously fired and vanished with no
/// feedback; now they land here and are listable.
async fn api_downloads(State(s): State<AppState>) -> Json<serde_json::Value> {
    let dir = s.download_dir.clone();
    let mut files = vec![];
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for entry in rd.filter_map(|e| e.ok()) {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            // Skip in-progress Chromium downloads.
            if path.extension().and_then(|x| x.to_str()) == Some("crdownload") {
                continue;
            }
            let meta = entry.metadata().ok();
            let bytes = meta.as_ref().map(|m| m.len()).unwrap_or(0);
            let modified = meta
                .as_ref()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            files.push(serde_json::json!({
                "name": entry.file_name().to_string_lossy(),
                "bytes": bytes,
                "modified": modified,
                "path": path.to_string_lossy(),
            }));
        }
    }
    files.sort_by(|a, b| {
        b.get("modified").and_then(|x| x.as_u64()).cmp(&a.get("modified").and_then(|x| x.as_u64()))
    });
    Json(serde_json::json!({ "ok": true, "dir": dir, "files": files }))
}

#[derive(Deserialize)]
struct HoverReq {
    /// Tab id (default: active tab).
    #[serde(default)]
    tab: Option<u32>,
    #[serde(default)]
    x: Option<f64>,
    #[serde(default)]
    y: Option<f64>,
    /// Snapshot ref or CSS selector to hover instead of coordinates
    /// (resolves main-frame and same-origin iframe elements).
    #[serde(default)]
    target: Option<String>,
}
/// Move the mouse without clicking (drives :hover states, tooltips, menus).
/// Deliberately unlogged — hover fires continuously.
async fn api_hover(State(s): State<AppState>, Json(b): Json<HoverReq>) -> Json<serde_json::Value> {
    let (_tab_id, page) = tab!(&s, b.tab);
    let (x, y) = match (&b.target, b.x, b.y) {
        (Some(t), _, _) => {
            let sel = target_to_selector(t);
            // Ensure refs exist (snapshot tags elements) — same as click.
            if !t.trim().starts_with("css=") && t.trim().parse::<u32>().is_ok() {
                let _ = tokio::time::timeout(CDP_TIMEOUT, page.evaluate(SNAPSHOT_JS)).await;
            }
            // Main-frame fast path: bounding rect via JS.
            let rect_js = format!(
                "(() => {{ const el = document.querySelector({}); if (!el) return null; const r = el.getBoundingClientRect(); return [r.left + r.width / 2, r.top + r.height / 2]; }})()",
                serde_json::to_string(&sel).unwrap()
            );
            let direct: Option<(f64, f64)> =
                match tokio::time::timeout(CDP_TIMEOUT, page.evaluate(rect_js.as_str())).await {
                    Ok(Ok(ev)) => ev.into_value::<Vec<f64>>().ok().and_then(|pt| {
                        (pt.len() == 2 && pt[0].is_finite() && pt[1].is_finite()).then_some((pt[0], pt[1]))
                    }),
                    _ => None,
                };
            match direct.or(ref_coords(&page, t).await) {
                Some(pt) => pt,
                None => return err(format!("element not found for `{sel}` (run `lb snapshot` first)")),
            }
        }
        (None, Some(x), Some(y)) => (x, y),
        _ => return err("hover needs x+y coordinates or a target (see `lb hover --help`)".into()),
    };
    if !x.is_finite() || !y.is_finite() || x < 0.0 || y < 0.0 {
        return err("invalid coordinates".into());
    }
    match tokio::time::timeout(
        CDP_TIMEOUT,
        page.move_mouse(chromiumoxide::layout::Point::new(x, y)),
    )
    .await
    {
        Ok(Ok(_)) => Json(serde_json::json!({ "ok": true })),
        Ok(Err(e)) => err(friendly_cdp_error(s.port, "hover failed", e)),
        Err(_) => err(format!("hover timed out after {}s", CDP_TIMEOUT.as_secs())),
    }
}

#[derive(Deserialize)]
struct WheelReq {
    /// Tab id (default: active tab).
    #[serde(default)]
    tab: Option<u32>,
    x: f64,
    y: f64,
    #[serde(default)]
    dx: f64,
    #[serde(default)]
    dy: f64,
}
/// Trackpad-style wheel event at viewport coordinates: scrolls whatever is
/// under the cursor (inner containers included), unlike /api/scroll which
/// always scrolls the whole page.
async fn api_wheel(State(s): State<AppState>, Json(b): Json<WheelReq>) -> Json<serde_json::Value> {
    use chromiumoxide::cdp::browser_protocol::input::{DispatchMouseEventParams, DispatchMouseEventType};
    if ![b.x, b.y, b.dx, b.dy].iter().all(|v| v.is_finite()) || b.x < 0.0 || b.y < 0.0 {
        return err("invalid wheel params".into());
    }
    let (_tab_id, page) = tab!(&s, b.tab);
    let run = async {
        if let Err(e) = page.move_mouse(chromiumoxide::layout::Point::new(b.x, b.y)).await {
            return Err(format!("mouse move failed: {e:#}"));
        }
        let params = DispatchMouseEventParams::builder()
            .r#type(DispatchMouseEventType::MouseWheel)
            .x(b.x)
            .y(b.y)
            .delta_x(b.dx)
            .delta_y(b.dy)
            .build()
            .unwrap();
        page.execute(params).await.map(|_| ()).map_err(|e| format!("wheel failed: {e:#}"))
    };
    match tokio::time::timeout(CDP_TIMEOUT, run).await {
        Ok(Ok(_)) => {
            tokio::time::sleep(Duration::from_millis(60)).await;
            ok("wheeled")
        }
        Ok(Err(e)) => err(e),
        Err(_) => err(format!("wheel timed out after {}s", CDP_TIMEOUT.as_secs())),
    }
}

#[derive(Deserialize)]
struct DragReq {
    /// Tab id (default: active tab).
    #[serde(default)]
    tab: Option<u32>,
    x1: f64,
    y1: f64,
    x2: f64,
    y2: f64,
    #[serde(default = "default_drag_steps")]
    steps: u32,
    /// "human" when triggered from the web UI; anything else = agent.
    #[serde(default)]
    by: String,
}
fn default_drag_steps() -> u32 {
    10
}
/// Mouse drag from (x1,y1) to (x2,y2): text selection, sliders, resize
/// handles, drag-and-drop.
async fn api_drag(State(s): State<AppState>, Json(b): Json<DragReq>) -> Json<serde_json::Value> {
    use chromiumoxide::cdp::browser_protocol::input::{
        DispatchMouseEventParams, DispatchMouseEventType, MouseButton,
    };
    if ![b.x1, b.y1, b.x2, b.y2].iter().all(|v| v.is_finite()) {
        return err("invalid drag coords".into());
    }
    let steps = b.steps.clamp(2, 50) as usize;
    let (tab_id, page) = tab!(&s, b.tab);
    s.push_log("drag", &format!("({},{}) -> ({},{})", b.x1, b.y1, b.x2, b.y2));
    s.set_cursor(tab_id, b.x1, b.y1, "drag", &b.by).await;
    // Field recipe mirrors Puppeteer's Mouse exactly (verified against
    // puppeteer-core 25: moves carry button+buttons, press carries
    // buttons=1 AFTER engaging, release carries buttons=0 AFTER clearing).
    let run = async {
        page
            .execute(
                DispatchMouseEventParams::builder()
                    .r#type(DispatchMouseEventType::MouseMoved)
                    .x(b.x1)
                    .y(b.y1)
                    .modifiers(0)
                    .buttons(0)
                    .button(MouseButton::None)
                    .build()
                    .unwrap(),
            )
            .await?;
        // NB: no click_count on press — a real mousedown carries none, and
        // sending one suppresses drag initiation (clicks complete on release).
        page
            .execute(
                DispatchMouseEventParams::builder()
                    .r#type(DispatchMouseEventType::MousePressed)
                    .x(b.x1)
                    .y(b.y1)
                    .modifiers(0)
                    .button(MouseButton::Left)
                    .buttons(1)
                    .build()
                    .unwrap(),
            )
            .await?;
        for i in 1..=steps {
            let t = i as f64 / steps as f64;
            page
                .execute(
                    DispatchMouseEventParams::builder()
                        .r#type(DispatchMouseEventType::MouseMoved)
                        .x(b.x1 + (b.x2 - b.x1) * t)
                        .y(b.y1 + (b.y2 - b.y1) * t)
                        .modifiers(0)
                        .button(MouseButton::Left)
                        .buttons(1)
                        .build()
                        .unwrap(),
                )
                .await?;
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
        page
            .execute(
                DispatchMouseEventParams::builder()
                    .r#type(DispatchMouseEventType::MouseReleased)
                    .x(b.x2)
                    .y(b.y2)
                    .button(MouseButton::Left)
                    .click_count(1)
                    .build()
                    .unwrap(),
            )
            .await?;
        anyhow::Ok(())
    };
    match tokio::time::timeout(CDP_TIMEOUT + Duration::from_secs(10), run).await {
        Ok(Ok(())) => {
            s.set_cursor(tab_id, b.x2, b.y2, "drag", &b.by).await;
            tokio::time::sleep(Duration::from_millis(120)).await;
            ok("dragged")
        }
        Ok(Err(e)) => err(friendly_cdp_error(s.port, "drag failed", e)),
        Err(_) => err(format!("drag timed out after {}s", (CDP_TIMEOUT + Duration::from_secs(10)).as_secs())),
    }
}

/// Viewport metrics for mapping screenshot pixels → page coordinates.
async fn api_metrics(
    State(s): State<AppState>,
    Query(q): Query<TabQuery>,
) -> Json<serde_json::Value> {
    let (_tab_id, page) = tab!(&s, q.tab);
    const METRICS_JS: &str = "({w: window.innerWidth, h: window.innerHeight, dpr: window.devicePixelRatio || 1, sx: window.scrollX, sy: window.scrollY})";
    match tokio::time::timeout(CDP_TIMEOUT, page.evaluate(METRICS_JS)).await {
        Ok(Ok(r)) => match r.into_value::<serde_json::Value>() {
            Ok(m) => Json(serde_json::json!({ "ok": true, "metrics": m })),
            Err(e) => err(format!("decode metrics failed: {e:#}")),
        },
        Ok(Err(e)) => err(friendly_cdp_error(s.port, "metrics failed", e)),
        Err(_) => err(format!("metrics timed out after {}s", CDP_TIMEOUT.as_secs())),
    }
}

#[derive(Deserialize)]
struct HandoverReq {
    /// Tab id (default: active tab).
    #[serde(default)]
    tab: Option<u32>,
    message: String,
}
async fn api_handover_request(
    State(s): State<AppState>,
    Json(b): Json<HandoverReq>,
) -> Json<serde_json::Value> {
    let (tab_id, _page) = tab!(&s, b.tab);
    {
        let mut t = s.tabs.lock().await;
        let ho = &mut t.map.get_mut(&tab_id).unwrap().handover;
        ho.active = true;
        ho.message = b.message.clone();
        ho.claimed = false;
        ho.requested_at = Some(chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true));
    }
    s.push_log("handover/request", &format!("tab {tab_id}: {}", b.message));
    Json(serde_json::json!({ "ok": true, "tab": tab_id, "message": "handover requested — human should take over the browser window, then click Done in the web UI" }))
}

#[derive(Deserialize)]
struct ClaimReq {
    /// Tab id (default: active tab).
    #[serde(default)]
    tab: Option<u32>,
    #[serde(default)]
    message: String,
}
/// Human proactively claims control from the web UI (no agent request needed).
async fn api_handover_claim(
    State(s): State<AppState>,
    Json(b): Json<ClaimReq>,
) -> Json<serde_json::Value> {
    let (tab_id, _page) = tab!(&s, b.tab);
    let msg = if b.message.trim().is_empty() {
        "Human claimed control from the web UI.".to_string()
    } else {
        b.message.clone()
    };
    {
        let mut t = s.tabs.lock().await;
        let ho = &mut t.map.get_mut(&tab_id).unwrap().handover;
        ho.active = true;
        ho.message = msg.clone();
        ho.claimed = true;
        ho.requested_at = Some(chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true));
    }
    s.push_log("handover/claim", &format!("tab {tab_id}: {msg}"));
    Json(serde_json::json!({ "ok": true, "message": "control claimed — the agent should hold off until you click Done in the web UI" }))
}

async fn api_handover_get(
    State(s): State<AppState>,
    Query(q): Query<TabQuery>,
) -> Json<serde_json::Value> {
    let (tab_id, _page) = tab!(&s, q.tab);
    let ho = s.tabs.lock().await.map.get(&tab_id).unwrap().handover.clone();
    Json(serde_json::json!({ "ok": true, "tab": tab_id, "active": ho.active, "claimed": ho.claimed, "message": ho.message, "requested_at": ho.requested_at }))
}

async fn api_handover_done(
    State(s): State<AppState>,
    Query(q): Query<TabQuery>,
) -> Json<serde_json::Value> {
    let (tab_id, _page) = tab!(&s, q.tab);
    with_tab(&s, tab_id, |ts| ts.handover.active = false).await;
    s.push_log("handover/done", &format!("tab {tab_id}: human returned control to agent"));
    ok("handover complete — agent may continue (re-run `lb snapshot`)")
}

async fn api_log(State(s): State<AppState>) -> Json<serde_json::Value> {
    let entries = s.log.lock().await.clone();
    Json(serde_json::json!({ "ok": true, "entries": entries }))
}

/// List all tabs with basic info for quick overview.
///
/// Each tab is probed with a short timeout and falls back to its last known
/// URL: one wedged renderer (JS dialog, crashed target) must never hang the
/// whole listing — that used to brick `lb list`, `lb close` and `lb web link`
/// daemon-wide (stale tabs are flagged so callers can see it).
async fn api_tabs(State(s): State<AppState>) -> Json<serde_json::Value> {
    // Adopt page-opened tabs first so the tab strip shows target=_blank links.
    reconcile_tabs(&s).await;
    // Snapshot ids first so we don't hold the lock across CDP calls.
    let (ids, active) = {
        let t = s.tabs.lock().await;
        (t.map.keys().copied().collect::<Vec<_>>(), t.active)
    };
    let mut tabs = vec![];
    for id in ids {
        let (page, last_url, handover_active) = {
            let t = s.tabs.lock().await;
            match t.map.get(&id) {
                Some(ts) => (ts.page.clone(), ts.last_url.clone(), ts.handover.active),
                None => continue,
            }
        };
        let probe = async {
            let url = tokio::time::timeout(TAB_PROBE_TIMEOUT, page.url()).await.ok()?.ok()??;
            let title = tokio::time::timeout(TAB_PROBE_TIMEOUT, page.get_title()).await.ok()?.ok()?;
            Some((url, title))
        };
        let (url, title, stale) = match tokio::time::timeout(TAB_PROBE_TIMEOUT * 2, probe).await {
            Ok(Some((u, t))) => (u, t.unwrap_or_default(), false),
            _ => (last_url, String::new(), true),
        };
        tabs.push(serde_json::json!({
            "id": id,
            "active": id == active,
            "title": title,
            "url": url,
            "handover": handover_active,
            "stale": stale,
        }));
    }
    tabs.sort_by_key(|t| t["id"].as_u64().unwrap_or(0));
    Json(serde_json::json!({ "ok": true, "tabs": tabs }))
}

#[derive(Deserialize)]
struct CloseReq {
    /// Tab id (default: active tab).
    #[serde(default)]
    tab: Option<u32>,
}
async fn api_close(State(s): State<AppState>, Json(b): Json<CloseReq>) -> Json<serde_json::Value> {
    let (tab_id, _page) = tab!(&s, b.tab);
    // Drop the tab from our map first, then close the target in the
    // background with a timeout: a wedged renderer must not hang the close
    // (this used to make a poisoned tab unkillable).
    let removed = s.tabs.lock().await.map.remove(&tab_id);
    match removed {
        Some(ts) => {
            let url = if ts.last_url.is_empty() { "(unknown url)".to_string() } else { ts.last_url.clone() };
            tokio::spawn(async move {
                let _ = tokio::time::timeout(Duration::from_secs(10), ts.page.close()).await;
            });
            s.push_log("close", &format!("tab {tab_id} ({url})"));
            // Never leave the daemon tabless: open a fresh New-Tab page.
            let mut t = s.tabs.lock().await;
            if t.map.is_empty() {
                drop(t);
                let fresh = {
                    let browser = s.browser.lock().await;
                    tokio::time::timeout(CDP_TIMEOUT, browser.new_page(DEFAULT_START_URL)).await
                };
                match fresh {
                    Ok(Ok(fresh_page)) => {
                        let mut t = s.tabs.lock().await;
                        let id = t.next_id;
                        t.next_id += 1;
                        spawn_dialog_guard(fresh_page.clone(), s.log.clone(), id);
                        t.map.insert(id, TabState {
                            page: fresh_page,
                            handover: HandoverState { active: false, message: String::new(), claimed: false, requested_at: None },
                            zoom: 1.0,
                            cursor: CursorState::default(),
                            last_url: DEFAULT_START_URL.into(),
                        });
                        t.active = id;
                        return ok(&format!("closed tab {tab_id}; opened fresh tab {id}"));
                    }
                    Ok(Err(e)) => return err(friendly_cdp_error(s.port, &format!("closed tab {tab_id} but fresh tab failed"), e)),
                    Err(_) => return err(format!("closed tab {tab_id} but the fresh tab timed out — browser may be dead; restart with `lb web --port {} restart`", s.port)),
                }
            }
            if t.active == tab_id {
                t.active = *t.map.keys().max().unwrap();
            }
            ok(&format!("closed tab {tab_id} ({url})"))
        }
        None => err(format!("no such tab: {tab_id}")),
    }
}

#[derive(Deserialize)]
struct ActivateReq {
    tab: u32,
}
async fn api_activate(State(s): State<AppState>, Json(b): Json<ActivateReq>) -> Json<serde_json::Value> {
    let (tab_id, page) = tab!(&s, Some(b.tab));
    s.tabs.lock().await.active = tab_id;
    // Best-effort: bring to front so a headed window follows the switch.
    let _ = tokio::time::timeout(Duration::from_secs(5), page.bring_to_front()).await;
    s.push_log("activate", &format!("tab {tab_id}"));
    ok(&format!("tab {tab_id} active"))
}

// ---------------------------------------------------------------------------
// Browser manager (Settings → Browser): list / download / switch the Chrome
// binary without restarting the daemon process. Switching relaunches the
// browser in-process — open tabs are lost (same as a restart). Profiles are
// per browser kind (`<profile>` vs `<profile>-cloak`), so each kind keeps its
// own logins/cookies across switches.
// ---------------------------------------------------------------------------

/// Effective chrome path for `launch` (explicit `--chrome`, else auto-detect).
fn launch_chrome_path(launch: &LaunchConfig) -> Option<String> {
    if let Some(c) = launch.chrome.as_ref().filter(|c| !c.trim().is_empty()) {
        return Some(c.clone());
    }
    detect_chrome()
}

async fn api_browsers(State(s): State<AppState>) -> Json<serde_json::Value> {
    let launch = s.launch.lock().await.clone();
    let current_path = launch_chrome_path(&launch);
    let current_version = match &current_path {
        Some(p) => crate::cloak::binary_version(p).await,
        None => None,
    };
    let current_kind = match &current_path {
        Some(p) if p.contains(".cloakbrowser") || p.contains("cloak") => "cloak",
        Some(_) => "system",
        None => "unknown",
    };
    let system_path = detect_chrome();
    let system_version = match &system_path {
        Some(p) => crate::cloak::binary_version(p).await,
        None => None,
    };
    let cloak = crate::cloak::installed();
    // Latest free version (cached failure → None; UI falls back to the pin).
    let latest = tokio::time::timeout(
        Duration::from_secs(12),
        crate::cloak::latest_free_version(),
    )
    .await
    .ok()
    .flatten();
    let dl = s.cloak_dl.lock().await.clone();
    Json(serde_json::json!({
        "ok": true,
        "current": {"kind": current_kind, "path": current_path, "version": current_version},
        "system": {"path": system_path, "version": system_version},
        "cloak": cloak,
        "latest_free": latest,
        "fallback_free": crate::cloak::fallback_free_version(),
        "download": dl,
    }))
}

#[derive(Deserialize)]
struct BrowsersDownloadReq {
    /// Free-build version (default: newest discovered, pinned fallback offline).
    #[serde(default)]
    version: Option<String>,
}

async fn api_browsers_download(State(s): State<AppState>, Json(b): Json<BrowsersDownloadReq>) -> Json<serde_json::Value> {
    {
        let d = s.cloak_dl.lock().await;
        if matches!(
            d.phase,
            crate::cloak::DlPhase::Downloading | crate::cloak::DlPhase::Verifying | crate::cloak::DlPhase::Extracting
        ) {
            return err(format!("a download is already running ({} {})", format!("{:?}", d.phase).to_lowercase(), d.version));
        }
    }
    let version = b.version.clone();
    let dl = s.cloak_dl.clone();
    let log = s.log.clone();
    tokio::spawn(async move {
        match crate::cloak::download_free(version, dl).await {
            Ok(path) => {
                let entry = LogEntry {
                    at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                    action: "browser".into(),
                    detail: format!("CloakBrowser ready: {path}"),
                };
                let log = log.clone();
                tokio::spawn(async move {
                    let mut l = log.lock().await;
                    l.push(entry);
                });
            }
            Err(e) => tracing::warn!("cloak download failed: {e:#}"),
        }
    });
    s.push_log("browser", "CloakBrowser download started");
    ok("download started — poll the popup for progress")
}

async fn api_browsers_download_progress(State(s): State<AppState>) -> Json<serde_json::Value> {
    let d = s.cloak_dl.lock().await.clone();
    Json(serde_json::json!({ "ok": true, "download": d, "pct": d.pct() }))
}

#[derive(Deserialize)]
struct BrowsersUseReq {
    /// "system" or "cloak".
    kind: String,
    /// For cloak: installed version (default: newest installed).
    #[serde(default)]
    version: Option<String>,
}

/// Switch the running browser in-process: pre-validate the target (version +
/// headless smoke run) while the old browser is still up, then close the old
/// browser, launch the new one, and reset to a single New-Tab page. Bounded
/// throughout; ANY post-close failure rolls back to the previous binary, so
/// the daemon is never left browserless and the UI never claims a switch
/// that didn't happen.
///
/// Profiles are per browser kind (`<profile>` for system, `<profile>-cloak`
/// for CloakBrowser): the two builds are usually months apart (e.g. system
/// Chrome 155 vs free Cloak 145) and the older build trap-crashes on a profile
/// the newer one has touched, so sharing one dir makes system→cloak switches
/// fail. Each kind keeps its own logins across switches instead.
async fn api_browsers_use(State(s): State<AppState>, Json(b): Json<BrowsersUseReq>) -> Json<serde_json::Value> {
    let kind = b.kind.trim().to_lowercase();
    let target: String = match kind.as_str() {
        "system" => match detect_chrome() {
            Some(p) => p,
            None => return err("no system Chrome/Chromium found".into()),
        },
        "cloak" => {
            let all = crate::cloak::installed();
            if all.is_empty() {
                return err("no CloakBrowser binary installed — download it first".into());
            }
            match b.version.as_ref().map(|v| v.trim()).filter(|v| !v.is_empty()) {
                Some(v) => match all.iter().find(|c| c.version == v) {
                    Some(c) => c.path.clone(),
                    None => return err(format!("CloakBrowser {v} is not installed")),
                },
                None => all.into_iter().next().map(|c| c.path).unwrap(),
            }
        }
        _ => return err(format!("unknown browser kind {:?} (use system or cloak)", b.kind)),
    };
    // Validate before touching the running browser.
    if tokio::fs::metadata(&target).await.is_err() {
        return err(format!("browser binary not found: {target}"));
    }
    match crate::cloak::binary_version(&target).await {
        Some(v) => tracing::info!("switching browser to {target} ({v})"),
        None => return err(format!("{target} would not run (`--version` failed)")),
    }
    let no_sandbox = s.launch.lock().await.no_sandbox;
    if let Err(e) = crate::cloak::smoke_test_headless(&target, no_sandbox).await {
        return err(format!("{target} failed a headless smoke run, old browser untouched: {e:#}"));
    }
    s.push_log("browser", &format!("switching to {target}"));
    // Lock ordering (browser → launch → tabs → log) is exclusive to this
    // endpoint, so holding across the relaunch is deadlock-free.
    let mut browser = s.browser.lock().await;
    // Close the old browser first (releases the profile lock), then settle
    // briefly so slow platforms release it before the new launch.
    {
        let _ = tokio::time::timeout(Duration::from_secs(10), browser.close()).await;
        let _ = tokio::time::timeout(Duration::from_secs(10), browser.wait()).await;
        tokio::time::sleep(Duration::from_millis(1500)).await;
    }
    let mut launch = s.launch.lock().await.clone();
    let previous = launch.chrome.clone();
    let previous_profile = launch.profile.clone();
    launch.chrome = Some(target.clone());
    // Per-kind profile (see fn docs): deterministic so repeated switches are
    // stable (system <-> base, cloak <-> base-cloak). A first switch to Cloak
    // starts fresh there — log in again once, it persists afterwards.
    launch.profile = kind_profile(&launch.profile, &kind);
    let profile_fresh = !std::path::Path::new(&launch.profile).exists();
    std::fs::create_dir_all(&launch.profile).ok();
    match launch_with_page(&launch).await {
        Ok((nb, page)) => {
            install_fresh_browser(&s, &mut browser, launch.clone(), nb, page).await;
            s.push_log("browser", &format!("now using {target} (tabs reset)"));
            let tabs_note = "open tabs were reset";
            if profile_fresh {
                Json(serde_json::json!({ "ok": true, "message": format!("now using {target} ({tabs_note}; fresh {kind} profile — please log in again)") }))
            } else {
                Json(serde_json::json!({ "ok": true, "message": format!("now using {target} ({tabs_note})") }))
            }
        }
        Err(stage) => {
            // Roll back so the daemon is never left browserless.
            launch.chrome = previous;
            launch.profile = previous_profile;
            match launch_with_page(&launch).await {
                Ok((rb, page)) => {
                    let prev = launch.chrome.clone().unwrap_or_default();
                    install_fresh_browser(&s, &mut browser, launch.clone(), rb, page).await;
                    s.push_log("browser", &format!("switch failed ({stage}) — rolled back to {prev}"));
                    err(format!("switch failed ({stage}) — still on the previous browser (tabs were reset)"))
                }
                Err(e2) => {
                    mark_browser_dead();
                    err(format!("switch failed ({stage}) AND rollback failed ({e2}) — restart with `lb web restart`"))
                }
            }
        }
    }
}

/// Profile dir for a browser kind, derived from the daemon's base profile:
/// system uses the base dir itself, CloakBrowser a `-cloak` sibling. Stable
/// across repeated switches (system <-> base, cloak <-> base-cloak) and
/// respectful of an explicit `--profile *-cloak` dir.
fn kind_profile(base: &str, kind: &str) -> String {
    if kind == "cloak" {
        if base.ends_with("-cloak") {
            base.to_string()
        } else {
            format!("{base}-cloak")
        }
    } else if base.ends_with("-cloak") {
        base.trim_end_matches("-cloak").to_string()
    } else {
        base.to_string()
    }
}

/// Reuse Chrome's initial tab instead of opening a second one.
///
/// Chrome always opens one tab on launch (the New Tab page — or the
/// `-third-party` variant on ungoogled-chromium). Calling `new_page` on top
/// of that used to leave *two* tabs on every `lb web` start (one blank, one
/// NTP — or a restored `chrome://settings/help` when the profile had session
/// state), and `reconcile_tabs` then adopted the stray as a second lb tab.
///
/// This waits briefly for the initial target(s) to attach, navigates the
/// first one to `start_url` (unless already there), closes any extras, and
/// returns the single page. When no initial target appears it falls back to
/// `new_page` (retried — first launches can be slow on macOS).
async fn startup_page(browser: &Browser, start_url: &str) -> Result<Page, String> {
    // Wait for the initial tab(s) Chrome opens by itself.
    let mut initial: Vec<Page> = vec![];
    for _ in 0..25 {
        match tokio::time::timeout(Duration::from_millis(1000), browser.pages()).await {
            Ok(Ok(pages)) if !pages.is_empty() => {
                initial = pages;
                break;
            }
            _ => tokio::time::sleep(Duration::from_millis(200)).await,
        }
    }
    if initial.is_empty() {
        // No initial target (rare) — create one, retried like before.
        let mut last = String::new();
        for attempt in 1..=3 {
            match tokio::time::timeout(Duration::from_secs(20), browser.new_page(start_url)).await {
                Ok(Ok(p)) => return Ok(p),
                Ok(Err(e)) => last = format!("{e:#}"),
                Err(_) => last = "first tab timed out after 20s".into(),
            }
            tracing::warn!("browser (re)launch: first tab attempt {attempt} failed ({last})");
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        return Err(format!("first tab failed after 3 tries ({last})"));
    }
    let primary = initial.remove(0);
    // Close any extra initial/restored tabs (session restore, etc.) so
    // startup always leaves exactly one tab.
    for extra in initial {
        let _ = tokio::time::timeout(Duration::from_secs(5), extra.close()).await;
    }
    if start_url == "about:blank" {
        let _ = tokio::time::timeout(Duration::from_secs(5), primary.bring_to_front()).await;
        return Ok(primary);
    }
    // Navigate the initial tab to the wanted URL unless already there.
    // (`chrome://new-tab-page/` via `goto` works on both system Chrome and
    // ungoogled-chromium — verified, no redirect to `chrome://settings/help`.)
    let current = tokio::time::timeout(TAB_PROBE_TIMEOUT, primary.url())
        .await
        .ok()
        .and_then(|r| r.ok())
        .flatten()
        .unwrap_or_default();
    if current != start_url {
        match tokio::time::timeout(Duration::from_secs(25), primary.goto(start_url)).await {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => tracing::warn!("startup tab goto {start_url} failed ({e:#}) — keeping {current}"),
            Err(_) => tracing::warn!("startup tab goto {start_url} timed out — keeping {current}"),
        }
        // Let WebUI NTP settle; don't hard-fail startup on the wait.
        let _ = tokio::time::timeout(Duration::from_secs(3), primary.wait_for_navigation()).await;
    }
    let _ = tokio::time::timeout(Duration::from_secs(5), primary.bring_to_front()).await;
    Ok(primary)
}

/// Launch `launch` and open a first tab: the shared relaunch path for
/// switches and rollbacks. Spawns the generation-guarded watcher per launch.
/// Reuses Chrome's initial tab (see [`startup_page`); a half-dead browser
/// is dropped (child reaped) on failure.
async fn launch_with_page(launch: &LaunchConfig) -> Result<(Browser, Page), String> {
    let cfg = build_browser_config(launch).map_err(|e| format!("bad browser config: {e}"))?;
    let (mut nb, handler) = tokio::time::timeout(Duration::from_secs(75), Browser::launch(cfg))
        .await
        .map_err(|_| "launch timed out after 75s (first launch can be slow — retry)".to_string())?
        .map_err(|e| format!("launch failed: {e:#}"))?;
    spawn_browser_watcher(handler);
    match startup_page(&nb, DEFAULT_START_URL).await {
        Ok(p) => Ok((nb, p)),
        Err(last) => {
            let _ = tokio::time::timeout(Duration::from_secs(10), nb.close()).await;
            let _ = tokio::time::timeout(Duration::from_secs(10), nb.wait()).await;
            Err(format!("first tab failed after 3 tries ({last})"))
        }
    }
}

/// Install a freshly launched browser as current: single New-Tab page (like a
/// restart), dialog guard armed, launch config stored.
async fn install_fresh_browser(
    s: &AppState,
    browser_guard: &mut tokio::sync::MutexGuard<'_, Browser>,
    launch: LaunchConfig,
    nb: Browser,
    page: Page,
) {
    **browser_guard = nb;
    BROWSER_ALIVE.store(true, std::sync::atomic::Ordering::SeqCst);
    {
        let mut launch_guard = s.launch.lock().await;
        launch_guard.chrome = launch.chrome.clone();
        launch_guard.profile = launch.profile;
    }
    // Remember the switch so the next `lb web` (no --chrome flag) keeps it.
    save_persisted_chrome(launch.chrome.as_deref());
    // Record the actual URL (normally the New Tab page).
    let last_url = tokio::time::timeout(TAB_PROBE_TIMEOUT, page.url())
        .await
        .ok()
        .and_then(|r| r.ok())
        .flatten()
        .unwrap_or_else(|| DEFAULT_START_URL.into());
    {
        let mut t = s.tabs.lock().await;
        t.map.clear();
        t.map.insert(
            1u32,
            TabState {
                page: page.clone(),
                handover: HandoverState { active: false, message: String::new(), claimed: false, requested_at: None },
                zoom: 1.0,
                cursor: CursorState::default(),
                last_url,
            },
        );
        t.active = 1;
        t.next_id = 2;
    }
    spawn_dialog_guard(page, s.log.clone(), 1);
}

// ---------------------------------------------------------------------------
// Static UI
// ---------------------------------------------------------------------------

/// Serve the built Svelte app from `web/dist` (sibling of crate root).
/// Falls back to a diagnostic placeholder page when the UI isn't found —
/// it names the directory that was searched so a broken install (stale
/// binary on PATH, moved install dir) is obvious instead of mysterious.
async fn ui_index() -> Html<String> {
    let dir = dist_dir();
    let index = dir.join("index.html");
    if let Ok(html) = tokio::fs::read_to_string(&index).await {
        return Html(html);
    }
    Html(format!(
        r#"<!doctype html><html><body style="font-family:sans-serif;max-width:640px;margin:4rem auto">
<h1>Live Browser</h1>
<p>Daemon is running, but the web UI was not found.</p>
<p>Looked in: <code>{}</code></p>
<p>If you installed with the installer, it may be a stale <code>lb</code> on your PATH
(check <code>which -a lb</code> / <code>lb --version</code>) — re-run the installer.
If you built from source, build the UI first:</p>
<pre>cd web &amp;&amp; npm install &amp;&amp; npm run build</pre>
<p>Meanwhile the JSON API works: <a href="/api/status">/api/status</a></p>
</body></html>"#,
        dir.display()
    ))
}

/// Locate the `web/dist` directory (built Svelte UI), supporting both
/// `cargo run` (cwd = crate root) and running the binary from target/.
fn dist_dir() -> std::path::PathBuf {
    // Fast path: cwd-relative (covers `cargo run` and the daemon started
    // from the crate root).
    let cwd = std::path::PathBuf::from("web").join("dist");
    if cwd.join("index.html").exists() {
        return cwd;
    }
    // Walk up from the executable location (covers target/debug/lb etc.).
    // The exe path is canonicalized first: on macOS `current_exe()` returns
    // the *symlink* path (`~/.local/bin/lb`), whose ancestors lead nowhere —
    // only the resolved location (`~/.local/share/live-browser/lb`) sits
    // next to `web/dist`. (On Linux /proc/self/exe is already resolved.)
    if let Ok(exe) = std::env::current_exe() {
        let resolved = std::fs::canonicalize(&exe).unwrap_or_else(|_| exe.clone());
        for start in [resolved, exe] {
            if let Some(found) = find_dist_from(&start) {
                return found;
            }
        }
    }
    cwd
}

/// Walk up from `exe` looking for `web/dist` (or a crate root, whose
/// `web/dist` is returned even when not built yet). Split out for testing.
fn find_dist_from(exe: &std::path::Path) -> Option<std::path::PathBuf> {
    let mut dir = exe.parent().map(|p| p.to_path_buf());
    while let Some(d) = dir {
        let cand = d.join("web").join("dist");
        if cand.join("index.html").exists() {
            return Some(cand);
        }
        if d.join("Cargo.toml").exists() {
            return Some(d.join("web").join("dist"));
        }
        dir = d.parent().map(|p| p.to_path_buf());
    }
    None
}

// ---------------------------------------------------------------------------
// Daemon entrypoint
// ---------------------------------------------------------------------------

/// Default persistent browser-profile dir, per OS convention:
/// - Windows: `%LOCALAPPDATA%\live-browser\profile`
/// - macOS:   `~/Library/Caches/live-browser/profile`
/// - Linux:   `$XDG_CACHE_HOME/live-browser/profile` (else `~/.cache/…`)
// Base dir for live-browser state (profile + pidfile): %LOCALAPPDATA% on
// Windows, ~/Library/Caches on macOS, $XDG_CACHE_HOME (else ~/.cache)
// on Linux.
pub(crate) fn cache_base() -> std::path::PathBuf {
    #[cfg(windows)]
    {
        if let Ok(local) = std::env::var("LOCALAPPDATA") {
            if !local.is_empty() {
                return std::path::PathBuf::from(local);
            }
        }
        // Fallback: %USERPROFILE%\AppData\Local\…
        if let Ok(home) = std::env::var("USERPROFILE") {
            if !home.is_empty() {
                return std::path::PathBuf::from(home)
                    .join("AppData")
                    .join("Local");
            }
        }
        return std::env::temp_dir();
    }
    #[cfg(target_os = "macos")]
    {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
        return std::path::PathBuf::from(home).join("Library").join("Caches");
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        std::env::var("XDG_CACHE_HOME")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| {
                let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
                std::path::PathBuf::from(home).join(".cache")
            })
    }
}

/// Pidfile for the daemon serving `port` (one daemon per port).
/// Written on startup, removed on clean shutdown; `lb web stop` uses it.
pub(crate) fn pid_file(port: u16) -> std::path::PathBuf {
    cache_base()
        .join("live-browser")
        .join(format!("daemon-{port}.pid"))
}

pub(crate) fn default_profile_dir() -> String {
    let base = cache_base();
    #[cfg(windows)]
    {
        let dir = base.join("live-browser").join("profile");
        if dir.exists() {
            return dir.to_string_lossy().to_string();
        }
        // Fallback when LOCALAPPDATA/USERPROFILE are both missing (service
        // accounts): a profile dir next to the pidfile fallback.
        if base == std::env::temp_dir() {
            return base.join("live-browser-profile").to_string_lossy().to_string();
        }
        return dir.to_string_lossy().to_string();
    }
    #[cfg(target_os = "macos")]
    {
        let dir = base.join("live-browser").join("profile");
        // Migrate a profile created by an older version that used ~/.cache.
        let legacy = std::path::PathBuf::from(
            std::env::var("HOME").unwrap_or_else(|_| "/tmp".into()),
        )
        .join(".cache")
        .join("live-browser")
        .join("profile");
        if !dir.exists() && legacy.exists() {
            std::fs::create_dir_all(dir.parent().unwrap()).ok();
            if std::fs::rename(&legacy, &dir).is_ok() {
                tracing::info!("migrated browser profile from {}", legacy.display());
            }
        }
        return dir.to_string_lossy().to_string();
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let dir = base.join("live-browser").join("profile");
        // One-time migration from the old `simple-browser` profile location.
        let legacy = base.join("simple-browser").join("profile");
        if !dir.exists() && legacy.exists() && std::fs::rename(&legacy, &dir).is_ok() {
            tracing::info!("migrated browser profile from {}", legacy.display());
        }
        dir.to_string_lossy().to_string()
    }
}

/// Best-effort Chrome/Chromium auto-detect for `--chrome` default.
/// `chromiumoxide` already searches PATH + standard locations, so this only
/// pins well-known absolute paths that are *not* on PATH.
fn detect_chrome() -> Option<String> {
    let candidates: &[&str] = if cfg!(windows) {
        &[
            r"C:\Program Files\Google\Chrome\Application\chrome.exe",
            r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe",
        ]
    } else if cfg!(target_os = "macos") {
        &[
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
            "/Applications/Chromium.app/Contents/MacOS/Chromium",
        ]
    } else {
        &[
            "/usr/bin/google-chrome-stable",
            "/usr/bin/google-chrome",
            "/usr/bin/chromium",
            "/usr/bin/chromium-browser",
        ]
    };
    for c in candidates {
        if std::path::Path::new(c).exists() {
            return Some(c.to_string());
        }
    }
    // Windows per-user install (%LOCALAPPDATA%) — path depends on env.
    #[cfg(windows)]
    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        let p = std::path::PathBuf::from(local)
            .join("Google")
            .join("Chrome")
            .join("Application")
            .join("chrome.exe");
        if p.exists() {
            return Some(p.to_string_lossy().to_string());
        }
    }
    // macOS per-user ~/Applications install.
    #[cfg(target_os = "macos")]
    if let Ok(home) = std::env::var("HOME") {
        for app in [
            "Google Chrome.app/Contents/MacOS/Google Chrome",
            "Chromium.app/Contents/MacOS/Chromium",
        ] {
            let p = std::path::PathBuf::from(&home).join("Applications").join(app);
            if p.exists() {
                return Some(p.to_string_lossy().to_string());
            }
        }
    }
    None
}

/// True when `ip` is a Tailscale address: the 100.64.0.0/10 CGNAT range
/// (first octet 100, second 64–127). Used to spot the tailnet address among
/// local interface addresses.
fn is_tailscale_ip(ip: &str) -> bool {
    let ip = ip.trim().split('/').next().unwrap_or("").trim();
    let parts: Vec<&str> = ip.split('.').collect();
    if parts.len() != 4 {
        return false;
    }
    let mut nums = [0u8; 4];
    for (i, p) in parts.iter().enumerate() {
        match p.parse::<u8>() {
            // Reject leading-zero / empty weirdness only implicitly: parse ok.
            Ok(n) if !p.is_empty() => nums[i] = n,
            _ => return false,
        }
    }
    nums[0] == 100 && (64..=127).contains(&nums[1])
}

/// Pull every Tailscale-range IPv4 out of free-form command output
/// (`ip addr`, `ifconfig`, `ipconfig`, `hostname -I`, …). Tokens may carry
/// CIDR suffixes (`100.x.y.z/32`) or trailing punctuation — strip those.
fn scrape_tailscale_ips(text: &str) -> Vec<String> {
    let mut out = vec![];
    for raw in text.split(|c: char| !(c.is_ascii_alphanumeric() || c == '.' || c == '/')) {
        if raw.is_empty() {
            continue;
        }
        let cand = raw.split('/').next().unwrap_or("").trim_matches('.');
        if is_tailscale_ip(cand) && !out.contains(&cand.to_string()) {
            out.push(cand.to_string());
        }
    }
    out
}

/// This machine's Tailscale IPv4 (e.g. `100.90.12.34`), if Tailscale is up.
/// Best-effort and fast: `tailscale ip -4` first, then scrape local interface
/// listings for the 100.64.0.0/10 range. Returns None when Tailscale is
/// absent/down. Every step is a short-lived local command; failures are
/// silently ignored (no tailnet → no extra URL line, nothing else changes).
fn tailscale_ipv4() -> Option<String> {
    // 1. Exact: `tailscale ip -4` prints our address (one per line).
    if let Ok(out) = std::process::Command::new("tailscale").args(["ip", "-4"]).output() {
        if out.status.success() {
            let s = String::from_utf8_lossy(&out.stdout);
            for line in s.lines() {
                let ip = line.trim();
                if is_tailscale_ip(ip) {
                    return Some(ip.to_string());
                }
            }
            // `tailscale ip` with no args also works on old CLIs — but the
            // scrape below covers us anyway.
        }
    }
    // 2. Fallback: scrape interface addresses (works even when the CLI is
    // missing but the tailnet interface is up, e.g. via the GUI client).
    #[cfg(windows)]
    {
        if let Ok(out) = std::process::Command::new("ipconfig").output() {
            if out.status.success() {
                let s = String::from_utf8_lossy(&out.stdout);
                if let Some(ip) = scrape_tailscale_ips(&s).into_iter().next() {
                    return Some(ip);
                }
            }
        }
        return None;
    }
    #[cfg(not(windows))]
    {
        for argv in [
            ["ip", "-4", "-o", "addr", "show"].as_slice(),
            ["ifconfig"].as_slice(),
            ["hostname", "-I"].as_slice(),
        ] {
            let (prog, args) = argv.split_first().unwrap();
            if let Ok(out) = std::process::Command::new(prog).args(args).output() {
                if out.status.success() {
                    let s = String::from_utf8_lossy(&out.stdout);
                    if let Some(ip) = scrape_tailscale_ips(&s).into_iter().next() {
                        return Some(ip);
                    }
                }
            }
        }
        None
    }
}

/// Launch the startup browser and open the first page. On a first-page
/// failure the half-launched browser is closed (releasing the profile lock)
/// so a fallback launch starts clean.
///
/// NOTE: the handler must be driven *before* any page operation — page
/// operations send via the handler task, so awaiting them with an undriven
/// handler deadlocks forever (every `lb web` start hung at "launching
/// chrome …" with the browser listening but the daemon silent).
async fn startup_browser(launch: &LaunchConfig, start_url: &str) -> Result<(Browser, Page)> {
    let cfg = build_browser_config(launch)
        .map_err(|e| anyhow::anyhow!("invalid browser config: {e}"))?;
    tracing::info!(
        "launching chrome (profile: {}, {}) …",
        launch.profile,
        if launch.headed { "headed" } else { "headless" }
    );
    let (mut browser, handler) = Browser::launch(cfg)
        .await
        .context("failed to launch Chrome — try --chrome /path/to/chrome or --no-sandbox")?;
    // Drive the handler before any page operation (see NOTE above).
    BROWSER_ALIVE.store(true, std::sync::atomic::Ordering::SeqCst);
    spawn_browser_watcher(handler);
    // Reuse Chrome's initial tab (single-tab startup) instead of opening a
    // second one. Bound the whole step: first launches can be slow (macOS
    // verifies the bundle), but a wedged renderer must never hang startup.
    match tokio::time::timeout(Duration::from_secs(60), startup_page(&browser, start_url)).await
    {
        Ok(Ok(page)) => Ok((browser, page)),
        Ok(Err(e)) => {
            let _ = tokio::time::timeout(Duration::from_secs(10), browser.close()).await;
            let _ = tokio::time::timeout(Duration::from_secs(10), browser.wait()).await;
            Err(anyhow::anyhow!("failed to open browser page: {e}"))
        }
        Err(_) => {
            let _ = tokio::time::timeout(Duration::from_secs(10), browser.close()).await;
            let _ = tokio::time::timeout(Duration::from_secs(10), browser.wait()).await;
            Err(anyhow::anyhow!(
                "failed to open browser page: first tab timed out after 60s"
            ))
        }
    }
}

pub async fn run_daemon(args: DaemonArgs, passcode: Option<String>) -> Result<()> {
    // Normalize: empty string = no passcode. Fall back to LB_PASSWORD for
    // folks coming from OPENCODE_SERVER_PASSWORD-style setups.
    let passcode = passcode
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            std::env::var("LB_PASSWORD")
                .ok()
                .filter(|s| !s.trim().is_empty())
        });
    if passcode.as_deref().map(|s| s.trim().is_empty()).unwrap_or(true) {
        // Mirror `opencode web`: loud opt-in warning, not a hard failure.
        eprintln!("\x1b[1;33m!\tLB_PASSCODE is not set; server is unsecured.\x1b[0m");
        eprintln!("\x1b[33m\tSet a passcode with --passcode <code> or LB_PASSCODE=<code> lb web.\x1b[0m");
        tracing::warn!("LB_PASSCODE is not set; server is unsecured");
    } else {
        tracing::info!("passcode protection enabled");
    }
    let mut launch = LaunchConfig::from_args(&args);
    std::fs::create_dir_all(&launch.profile).ok();
    // Say which binary is in play: without this a restart that silently falls
    // back to system Chrome (stale persisted path, missing --chrome) is
    // mysterious — the Settings → Browser choice should be visible here.
    match launch_chrome_path(&launch) {
        Some(p) => tracing::info!("browser binary: {p}"),
        None => tracing::info!("browser binary: auto-detect (no --chrome, no saved choice)"),
    }

    let start_url = args.url.clone().unwrap_or_else(|| DEFAULT_START_URL.into());
    // An explicit --chrome failure is the user's typo to fix (fail fast); a
    // *persisted* choice failing must never wedge startup — fall back to
    // system Chrome so the daemon (and the UI switch) still comes up.
    let explicit_chrome = args.chrome.as_ref().is_some_and(|c| !c.trim().is_empty());
    let (browser, page) = match startup_browser(&launch, &start_url).await {
        Ok(x) => x,
        Err(e) if !explicit_chrome && launch.chrome.is_some() => {
            let bad = launch.chrome.clone().unwrap_or_default();
            tracing::warn!("saved browser {bad} failed to start ({e:#}) — falling back to system Chrome and clearing the saved choice");
            eprintln!("\x1b[1;33m!\tsaved browser failed to start ({e:#}).\x1b[0m");
            eprintln!("\x1b[33m\tFalling back to system Chrome and clearing the saved choice — switch back in the UI (gear icon → Browser) once it is up.\x1b[0m");
            save_persisted_chrome(None);
            launch.chrome = None;
            launch.profile = launch_profile_for(
                &args.profile.clone().unwrap_or_else(default_profile_dir),
                None,
            );
            std::fs::create_dir_all(&launch.profile).ok();
            startup_browser(&launch, &start_url).await.map_err(|e2| {
                anyhow::anyhow!("browser failed to start with the saved choice ({e:#}) and with auto-detected system Chrome ({e2:#}) — try `lb web --chrome /path/to/chrome`")
            })?
        }
        Err(e) => {
            return Err(e.context(format!(
                "browser failed to start (binary: {})",
                launch_chrome_path(&launch).unwrap_or_else(|| "auto-detect".into())
            )));
        }
    };
    // The binary proved it launches: only now remember an explicit --chrome.
    // A bad flag value must never be persisted (it would wedge later starts).
    if explicit_chrome {
        save_persisted_chrome(launch.chrome.as_deref());
    }

    // The startup browser's handler is already driven (spawned inside
    // `startup_browser` before the first tab — page ops need it). Just
    // ensure the alive flag is set for the CDP error mapping below.
    BROWSER_ALIVE.store(true, std::sync::atomic::Ordering::SeqCst);

    let mut tabs_map = std::collections::HashMap::new();
    tabs_map.insert(
        1u32,
        TabState {
            page: page.clone(),
            handover: HandoverState {
                active: false,
                message: String::new(),
                claimed: false,
                requested_at: None,
            },
            zoom: 1.0,
            cursor: CursorState::default(),
            last_url: start_url.clone(),
        },
    );
    let log: Arc<Mutex<Vec<LogEntry>>> = Arc::new(Mutex::new(vec![LogEntry {
        at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        action: "daemon".into(),
        detail: "browser launched".into(),
    }]));
    // Auto-dismiss JS dialogs on the first tab (alert/confirm/prompt stall
    // the renderer and used to wedge the whole daemon — see spawn_dialog_guard).
    spawn_dialog_guard(page, log.clone(), 1);
    // Route browser downloads into a visible dir instead of the void.
    let download_dir = std::path::PathBuf::from(&launch.profile).join("downloads").to_string_lossy().to_string();
    std::fs::create_dir_all(&download_dir).ok();
    {
        use chromiumoxide::cdp::browser_protocol::browser::SetDownloadBehaviorParams;
        use chromiumoxide::cdp::browser_protocol::browser::SetDownloadBehaviorBehavior;
        let params = SetDownloadBehaviorParams::builder()
            .behavior(SetDownloadBehaviorBehavior::Allow)
            .download_path(download_dir.clone())
            .events_enabled(true)
            .build()
            .unwrap();
        // Best-effort: downloads still work with default behavior if this fails.
        let _ = browser.execute(params).await;
    }
    let state = AppState {
        tabs: Arc::new(Mutex::new(Tabs { map: tabs_map, active: 1, next_id: 2 })),
        browser: Arc::new(Mutex::new(browser)),
        log,
        passcode: passcode.clone(),
        port: args.port,
        download_dir,
        launch: Arc::new(Mutex::new(launch)),
        cloak_dl: Arc::new(Mutex::new(crate::cloak::DownloadState::default())),
    };
    // Kept for shutdown: closing the browser releases the profile lock so
    // the next `lb web` starts clean (no orphaned Chrome).
    let browser_handle = state.browser.clone();

    // `/api/*` goes behind the passcode gate (when configured); the static
    // UI itself stays public so it can render its own login form.
    let app = Router::new()
        .route("/", get(ui_index))
        .route("/api/status", get(api_status))
        .route("/api/open", post(api_open))
        .route("/api/snapshot", get(api_snapshot))
        .route("/api/screenshot", get(api_screenshot))
        .route("/api/click", post(api_click))
        .route("/api/click-at", post(api_click_at))
        .route("/api/scroll", post(api_scroll))
        .route("/api/select", post(api_select))
        .route("/api/metrics", get(api_metrics))
        .route("/api/key", post(api_key))
        .route("/api/hover", post(api_hover))
        .route("/api/insert", post(api_insert))
        .route("/api/type-raw", post(api_type_raw))
        .route("/api/wheel", post(api_wheel))
        .route("/api/drag", post(api_drag))
        .route("/api/zoom", post(api_zoom))
        .route("/api/type", post(api_type))
        .route("/api/press", post(api_press))
        .route("/api/eval", post(api_eval))
        .route("/api/nav", post(api_nav))
        .route("/api/wait", post(api_wait))
        .route("/api/text", post(api_text))
        .route("/api/html", post(api_html))
        .route("/api/upload", post(api_upload))
        .route("/api/downloads", get(api_downloads))
        .route("/api/handover", get(api_handover_get))
        .route("/api/handover/request", post(api_handover_request))
        .route("/api/handover/claim", post(api_handover_claim))
        .route("/api/handover/done", post(api_handover_done))
        .route("/api/log", get(api_log))
        .route("/api/tabs", get(api_tabs))
        .route("/api/close", post(api_close))
        .route("/api/activate", post(api_activate))
        .route("/api/browsers", get(api_browsers))
        .route("/api/browsers/download", post(api_browsers_download).get(api_browsers_download_progress))
        .route("/api/browsers/use", post(api_browsers_use))
        .layer(axum::middleware::from_fn_with_state(state.clone(), require_passcode))
        // Live tool: nothing is cacheable (stale tabs/snapshots caused real
        // confusion — the UI once needed a manual refresh to show new tabs).
        .layer(tower_http::set_header::SetResponseHeaderLayer::if_not_present(
            header::CACHE_CONTROL,
            axum::http::HeaderValue::from_static("no-store"),
        ))
        .fallback_service(tower_http::services::ServeDir::new(dist_dir()))
        .with_state(state);

    // Tailscale: when this machine is on a tailnet, listen on all interfaces
    // so the UI is reachable via the tailnet address (127.0.0.1 alone would
    // refuse tailnet connections), and print that URL next to localhost.
    // Without Tailscale the bind stays loopback-only (no LAN exposure).
    let ts_ip = tailscale_ipv4();
    let bind_host = if ts_ip.is_some() { "0.0.0.0" } else { "127.0.0.1" };
    let addr = format!("{bind_host}:{}", args.port);
    let local_addr = format!("127.0.0.1:{}", args.port);
    let listener = tokio::net::TcpListener::bind(&addr).await.map_err(|e| {
        anyhow::anyhow!(
            "cannot bind {addr}: {e:#} — is another `lb web` already running? (`lb web stop` stops it)"
        )
    })?;
    if ts_ip.is_some() {
        tracing::info!("tailscale detected — listening on all interfaces ({bind_host})");
        if passcode.is_none() {
            eprintln!("\x1b[1;33m!\tListening on your tailnet without a passcode — any device on your tailnet can drive the browser. Set LB_PASSCODE.\x1b[0m");
        }
    }
    // Record our pid so `lb web stop` / `lb web restart` can find us.
    // A stale file from a crashed daemon is tolerated: stop verifies
    // liveness before trusting it.
    let pid = pid_file(args.port);
    if let Some(parent) = pid.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    if let Err(e) = std::fs::write(&pid, std::process::id().to_string()) {
        tracing::warn!("could not write pid file {}: {e:#}", pid.display());
    }
    // Say exactly which binary and UI dir are in play: a stale `lb` earlier
    // on PATH (or a moved install dir) otherwise shows up only as a
    // mysterious missing UI in the browser.
    match std::env::current_exe() {
        Ok(exe) => tracing::info!("binary: {}", exe.display()),
        Err(e) => tracing::warn!("could not determine binary path: {e:#}"),
    }
    let ui_dir = dist_dir();
    if ui_dir.join("index.html").exists() {
        tracing::info!("serving web UI from {}", ui_dir.display());
    } else {
        tracing::warn!(
            "web UI not found in {} — the root page will show a diagnostic. \
             Installed via script? Re-run it (check `which -a lb` for a stale binary). \
             Built from source? Run `cd web && npm install && npm run build` first.",
            ui_dir.display()
        );
    }
    println!("live-browser daemon ready");
    println!("  UI:  http://{local_addr}/");
    if let Some(ip) = &ts_ip {
        println!("  UI (tailscale):  http://{ip}:{}/", args.port);
    }
    println!("  API: http://{local_addr}/api/status");
    if passcode.is_some() {
        println!("  auth: passcode protection enabled");
    }
    println!("CLI example:");
    if passcode.is_some() {
        println!("  lb --server http://{local_addr} --passcode <code> open https://example.com");
    } else {
        println!("  lb --server http://{local_addr} open https://example.com");
    }
    println!("  lb --server http://{local_addr} snapshot");
    println!("  lb --server http://{local_addr} handover \"please log in, then click Done in the UI\"");
    println!("  (stop this server with: lb web --port {} stop)", args.port);
    if !args.no_open {
        open_in_default_browser(&format!("http://{local_addr}/"));
    }
    // Ctrl-C / SIGTERM: shut down the HTTP server, then close the browser
    // (releasing the profile lock) and remove the pidfile, so `lb web stop`
    // / `restart` and the next start find a clean slate. SIGKILL can't be
    // caught — `stop` reaps those leftovers by profile dir.
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    {
        let mut b = browser_handle.lock().await;
        tracing::info!("closing browser …");
        let _ = tokio::time::timeout(Duration::from_secs(10), b.close()).await;
        let _ = tokio::time::timeout(Duration::from_secs(10), b.wait()).await;
        tracing::info!("browser closed");
    }
    std::fs::remove_file(pid_file(args.port)).ok();
    Ok(())
}

/// Open `url` in the OS default browser. Best-effort: headless servers,
/// containers and SSH sessions have no browser to open — that must never
/// fail the daemon startup.
fn open_in_default_browser(url: &str) {
    let attempt = if cfg!(target_os = "macos") {
        std::process::Command::new("open").arg(url).status()
    } else if cfg!(windows) {
        // `start` is a cmd builtin; the empty title arg keeps quoted URLs working.
        std::process::Command::new("cmd")
            .args(["/c", "start", "", url])
            .status()
    } else if let Ok(browser) = std::env::var("BROWSER") {
        // Respect $BROWSER when set (common on Linux).
        let mut parts = browser.split_whitespace();
        match parts.next() {
            Some(bin) if !bin.is_empty() => {
                let mut cmd = std::process::Command::new(bin);
                cmd.args(parts).arg(url);
                cmd.status()
            }
            _ => std::process::Command::new("xdg-open").arg(url).status(),
        }
    } else {
        std::process::Command::new("xdg-open").arg(url).status()
    };
    match attempt {
        Ok(s) if s.success() => tracing::info!("opened web UI in default browser"),
        Ok(s) => tracing::warn!("default browser exited with {s} — open the UI manually"),
        Err(e) => tracing::warn!("could not open a browser ({e:#}) — open the UI manually"),
    }
}

/// Fires on Ctrl-C everywhere, and on SIGTERM on Unix (that's what
/// `lb web stop` and most supervisors send).
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("SIGTERM handler");
        let mut int = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
            .expect("SIGINT handler");
        tokio::select! {
            _ = term.recv() => {},
            _ = int.recv() => {},
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression test for the macOS install: the daemon runs via a symlink
    /// (`~/.local/bin/lb` → `~/.local/share/live-browser/lb`) and
    /// `current_exe()` there returns the *unresolved* symlink path, whose
    /// ancestors contain no `web/dist`. Only the canonicalized path does.
    /// (Unix-only: Windows `current_exe()` already resolves symlinks.)
    #[test]
    #[cfg(unix)]
    fn dist_dir_resolves_symlinked_install() {
        let root = std::env::temp_dir().join(format!("lb-dist-test-{}", std::process::id()));
        let home = root.join("share").join("live-browser");
        let bin = root.join("bin");
        std::fs::create_dir_all(home.join("web").join("dist")).unwrap();
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(home.join("web").join("dist").join("index.html"), "<html>").unwrap();
        std::fs::write(home.join("lb"), "fake").unwrap();
        std::os::unix::fs::symlink(home.join("lb"), bin.join("lb")).unwrap();

        // Unresolved symlink path (what macOS `current_exe()` returns):
        // ancestors (bin/, root/, tmp…) hold no UI.
        assert_eq!(find_dist_from(&bin.join("lb")), None);
        // Canonicalized (real) path: UI found next to the binary.
        let resolved = std::fs::canonicalize(bin.join("lb")).unwrap();
        assert_eq!(
            find_dist_from(&resolved),
            Some(home.join("web").join("dist"))
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// Per-kind profiles are deterministic: repeated system<->cloak switches
    /// land on the same two dirs, so each kind keeps its own logins.
    #[test]
    fn kind_profile_is_stable_across_switches() {
        let base = "/tmp/lb-profile";
        assert_eq!(kind_profile(base, "system"), base);
        assert_eq!(kind_profile(base, "cloak"), "/tmp/lb-profile-cloak");
        // Back and forth stays on the same pair (no `-cloak-cloak` spiral).
        assert_eq!(kind_profile("/tmp/lb-profile-cloak", "cloak"), "/tmp/lb-profile-cloak");
        assert_eq!(kind_profile("/tmp/lb-profile-cloak", "system"), base);
    }

    fn scratch_prefs(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "lb-browser-prefs-test-{}-{name}",
            std::process::id()
        ));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("browser.json")
    }

    #[test]
    fn browser_prefs_roundtrip() {
        let path = scratch_prefs("roundtrip");
        // Missing file -> None (auto-detect).
        assert_eq!(load_persisted_chrome_from(&path), None);
        // Save + load an existing binary path.
        let bin = std::env::temp_dir().join("lb-prefs-fake-chrome");
        std::fs::write(&bin, "fake").unwrap();
        let bin_str = bin.to_string_lossy().to_string();
        save_persisted_chrome_to(&path, Some(&bin_str));
        assert_eq!(load_persisted_chrome_from(&path), Some(bin_str));
        // Clear -> None again and file removed.
        save_persisted_chrome_to(&path, None);
        assert_eq!(load_persisted_chrome_from(&path), None);
        assert!(!path.exists());
        std::fs::remove_file(&bin).ok();
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn browser_prefs_ignores_stale_binary() {
        let path = scratch_prefs("stale");
        let missing = path.parent().unwrap().join("no-such-chrome");
        save_persisted_chrome_to(&path, Some(&missing.to_string_lossy()));
        // File exists but binary doesn't -> fall back to auto-detect.
        assert_eq!(load_persisted_chrome_from(&path), None);
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn browser_prefs_ignores_corrupt_file() {
        let path = scratch_prefs("corrupt");
        std::fs::write(&path, "not json{{{").unwrap();
        assert_eq!(load_persisted_chrome_from(&path), None);
        std::fs::write(&path, "{\"chrome\":\"\"}").unwrap();
        assert_eq!(load_persisted_chrome_from(&path), None);
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn resolve_chrome_path_plain_and_home() {
        assert_eq!(resolve_chrome_path("  /usr/bin/chrome  "), "/usr/bin/chrome");
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
        assert_eq!(resolve_chrome_path("~/bin/chrome"), format!("{home}/bin/chrome"));
        assert_eq!(resolve_chrome_path(""), "");
    }

    #[test]
    fn resolve_chrome_path_glob_picks_newest() {
        let root = std::env::temp_dir().join(format!("lb-glob-test-{}", std::process::id()));
        std::fs::remove_dir_all(&root).ok();
        std::fs::create_dir_all(root.join("chromium-1")).unwrap();
        std::fs::create_dir_all(root.join("chromium-2")).unwrap();
        std::fs::write(root.join("chromium-1").join("chrome"), "a").unwrap();
        std::fs::write(root.join("chromium-2").join("chrome"), "b").unwrap();
        let pattern = format!("{}/chromium-*/chrome", root.display());
        assert_eq!(resolve_chrome_path(&pattern), format!("{}/chromium-2/chrome", root.display()));
        let nomatch = format!("{}/nomatch-*/chrome", root.display());
        assert_eq!(resolve_chrome_path(&nomatch), nomatch);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn chrome_kind_detects_cloak_paths() {
        assert_eq!(chrome_kind(None), "system");
        assert_eq!(chrome_kind(Some("/usr/bin/google-chrome")), "system");
        assert_eq!(chrome_kind(Some("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome")), "system");
        assert_eq!(
            chrome_kind(Some(
                "/Users/u/.cloakbrowser/chromium-145.0.7632.109.2/Chromium.app/Contents/MacOS/Chromium"
            )),
            "cloak"
        );
        assert_eq!(chrome_kind(Some("/opt/cloak/chrome")), "cloak");
    }

    /// Startup must land on the same per-kind profile the Settings → Browser
    /// switch uses — otherwise a cloak restart reuses the system profile dir
    /// and the older build trap-crashes on it (wedged daemon, no UI to
    /// switch back). Regression test for exactly that.
    #[test]
    fn launch_profile_follows_browser_kind() {
        let base = "/tmp/lb-profile";
        assert_eq!(launch_profile_for(base, None), base);
        assert_eq!(launch_profile_for(base, Some("/usr/bin/chrome")), base);
        assert_eq!(
            launch_profile_for(base, Some("/Users/u/.cloakbrowser/c/Chromium")),
            "/tmp/lb-profile-cloak"
        );
        // Stable: an already-suffixed base is not doubled.
        assert_eq!(
            launch_profile_for("/tmp/lb-profile-cloak", Some("/x/cloak")),
            "/tmp/lb-profile-cloak"
        );
    }

    #[test]
    fn segment_match_basics() {
        assert!(segment_matches("chromium-*", "chromium-146.0"));
        assert!(!segment_matches("chromium-*", "chrome-146.0"));
        assert!(segment_matches("?", "a"));
        assert!(!segment_matches("?", ""));
        assert!(segment_matches("a*b", "aXYZb"));
    }

    #[test]
    fn tailscale_ip_range() {
        // Inside 100.64.0.0/10.
        assert!(is_tailscale_ip("100.64.0.1"));
        assert!(is_tailscale_ip("100.127.255.254"));
        assert!(is_tailscale_ip("100.90.12.34"));
        assert!(is_tailscale_ip("  100.100.100.100\n"));
        assert!(is_tailscale_ip("100.72.0.5/32")); // CIDR suffix tolerated
        // Outside.
        assert!(!is_tailscale_ip("100.63.255.255"));
        assert!(!is_tailscale_ip("100.128.0.1"));
        assert!(!is_tailscale_ip("127.0.0.1"));
        assert!(!is_tailscale_ip("192.168.1.1"));
        assert!(!is_tailscale_ip("10.0.0.1"));
        assert!(!is_tailscale_ip("not-an-ip"));
        assert!(!is_tailscale_ip(""));
        assert!(!is_tailscale_ip("100.90.12"));
    }

    #[test]
    fn tailscale_scrape_finds_first() {
        let ip_out = "2: tailscale0    inet 100.90.12.34/32 scope global tailscale0\n3: eth0 inet 192.168.1.5/24 brd";
        assert_eq!(scrape_tailscale_ips(ip_out), vec!["100.90.12.34".to_string()]);
        let ipcfg = "   IPv4 Address. . . . . . . . . . . : 100.120.7.9(Preferred)";
        assert_eq!(scrape_tailscale_ips(ipcfg), vec!["100.120.7.9".to_string()]);
        assert!(scrape_tailscale_ips("inet 192.168.1.5/24").is_empty());
    }
}
