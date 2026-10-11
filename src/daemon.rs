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
// Snapshot / interaction JS
// ---------------------------------------------------------------------------

const SNAPSHOT_JS: &str = r#"(() => {
  document.querySelectorAll('[data-lb-ref]').forEach(e => e.removeAttribute('data-lb-ref'));
  const els = document.querySelectorAll('a, button, input, select, textarea, [role="button"], [role="link"], [role="textbox"], [onclick], summary');
  const out = [];
  let ref = 1;
  for (const el of els) {
    if (out.length >= 200) break;
    try {
      const rect = el.getBoundingClientRect();
      const style = window.getComputedStyle(el);
      if (style.display === 'none' || style.visibility === 'hidden') continue;
      if (rect.width === 0 && rect.height === 0 && rect.top === 0 && rect.left === 0) continue;
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
        options: (el.tagName === 'SELECT'
          ? Array.from(el.options).map(o => ({
              value: o.value,
              text: String(o.text || '').trim().replace(/\s+/g, ' ').slice(0, 40),
              selected: o.selected
            }))
          : [])
      });
      ref++;
    } catch (e) { continue; }
  }
  return out;
})()"#;

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
    let (tab_id, page) = tab!(&s, q.tab);
    let url = page_url(&page).await;
    let title = page_title(&page).await;
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
}
/// Omnibox-style normalization: full URLs pass through, bare domains get
/// https://, and anything else becomes a Google search.
fn normalize_open_url(input: &str) -> String {
    let t = input.trim();
    if t.contains("://") || t.starts_with("about:") || t.starts_with("data:") || t.starts_with("file:") {
        return t.to_string();
    }
    let single_token = !t.chars().any(|c| c.is_whitespace());
    if single_token && (t.contains('.') || t.starts_with("localhost") || t.contains(':')) {
        return format!("https://{t}");
    }
    format!("https://www.google.com/search?q={}", urlencode_query(t))
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
    let url = normalize_open_url(&input);
    // With an explicit tab: navigate it in place. Without: open a new tab.
    let (tab_id, page) = match b.tab {
        Some(id) => tab!(&s, Some(id)),
        None => {
            let page = match s.browser.lock().await.new_page(&url).await {
                Ok(p) => p,
                Err(e) => return err(format!("new tab failed: {e:#}")),
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
                        page: page.clone(),
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
            return open_navigate(&s, id, page, &url).await;
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
    match page.evaluate(SNAPSHOT_JS).await {
        Ok(eval) => match eval.into_value::<serde_json::Value>() {
            Ok(elements) => Json(serde_json::json!({
                "ok": true, "tab": tab_id, "url": url, "title": title, "elements": elements,
                "hint": "click with `lb click <ref>`, type with `lb type <ref> <text>`. Re-run snapshot after navigation."
            })),
            Err(e) => err(format!("decode snapshot failed: {e:#}")),
        },
        Err(e) => err(format!("snapshot failed (page still loading?): {e:#}")),
    }
}

async fn api_screenshot(
    State(s): State<AppState>,
    Query(q): Query<TabQuery>,
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
        Ok(Ok(bytes)) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "image/jpeg"), (header::CACHE_CONTROL, "no-store")],
            bytes,
        )
            .into_response(),
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
}
async fn api_click(State(s): State<AppState>, Json(b): Json<ClickReq>) -> Json<serde_json::Value> {
    let sel = target_to_selector(&b.target);
    let (tab_id, page) = tab!(&s, b.tab);
    s.push_log("click", &format!("tab {tab_id} <- {sel}"));
    // Ensure refs exist (snapshot tags elements) — but only for numeric refs.
    // Raw `css=` selectors don't need DOM re-tagging; skipping the full
    // SNAPSHOT_JS scan here saves a costly layout-forcing evaluate.
    if !b.target.trim().starts_with("css=") && b.target.trim().parse::<u32>().is_ok() {
        let _ = page.evaluate(SNAPSHOT_JS).await;
    }
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
            match el.click().await {
                Ok(_) => {
                    // Brief settle so SPAs commit DOM updates; the caller
                    // (UI poll / next snapshot) observes the rest. Kept short
                    // on purpose — this sleep is on the latency hot path.
                    tokio::time::sleep(Duration::from_millis(150)).await;
                    ok(&format!("clicked {sel}"))
                }
                Err(e) => err(format!("click failed: {e:#}")),
            }
        }
        Err(e) => err(format!("element not found for `{sel}` (run `lb snapshot` first): {e:#}")),
    }
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
}
async fn api_type(State(s): State<AppState>, Json(b): Json<TypeReq>) -> Json<serde_json::Value> {
    let sel = target_to_selector(&b.target);
    let (_tab_id, page) = tab!(&s, b.tab);
    s.push_log("type", &format!("{sel} <- {:?}", b.text));
    if !b.target.trim().starts_with("css=") && b.target.trim().parse::<u32>().is_ok() {
        let _ = page.evaluate(SNAPSHOT_JS).await;
    }
    let el = match page.find_element(&sel).await {
        Ok(el) => el,
        Err(e) => return err(format!("element not found for `{sel}`: {e:#}")),
    };
    if let Err(e) = el.click().await {
        return err(format!("focus failed: {e:#}"));
    }
    // Clear existing content via JS, then type (typing triggers real key events).
    let clear_js = format!(
        "(() => {{ const el = document.querySelector({}); if (el) {{ el.focus(); if ('value' in el) {{ el.value=''; el.dispatchEvent(new Event('input', {{bubbles:true}})); }} }} }})()",
        serde_json::to_string(&sel).unwrap()
    );
    let _ = page.evaluate(clear_js.as_str()).await;
    // Type line-by-line: '\n' has no key definition, so press Enter between
    // lines. Leading whitespace is stripped and left to the editor's
    // auto-indent (typing it verbatim would double-indent); electric
    // characters like `}` re-indent themselves in code editors.
    // Single-line text keeps its exact spacing.
    if !b.text.contains('\n') {
        if let Err(e) = el.type_str(&b.text).await {
            return err(format!("type failed: {e:#}"));
        }
    } else {
        let mut first = true;
        for line in b.text.replace('\t', "    ").split('\n') {
            if !first {
                if let Err(e) = el.press_key("Enter").await {
                    return err(format!("newline failed: {e:#}"));
                }
            }
            first = false;
            let content = line.trim_start();
            if !content.is_empty() {
                if let Err(e) = el.type_str(content).await {
                    return err(format!("type failed: {e:#}"));
                }
            }
        }
    }
    if b.submit {
        if let Err(e) = el.press_key("Enter").await {
            return err(format!("submit (Enter) failed: {e:#}"));
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    ok(&format!("typed into {sel}"))
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
    match page.find_element("body").await {
        Ok(el) => match el.press_key(b.key.as_str()).await {
            Ok(_) => {
                tokio::time::sleep(Duration::from_millis(80)).await;
                ok(&format!("pressed {}", b.key))
            }
            Err(e) => err(format!("press failed: {e:#}")),
        },
        Err(e) => err(format!("press failed (no body?): {e:#}")),
    }
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
    match page.evaluate(b.js.as_str()).await {
        Ok(r) => match r.into_value::<serde_json::Value>() {
            Ok(v) => Json(serde_json::json!({ "ok": true, "tab": tab_id, "result": v })),
            Err(_) => Json(serde_json::json!({ "ok": true, "result": "(non-JSON value — side effects applied)" })),
        },
        Err(e) => err(format!("eval failed: {e:#}")),
    }
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
        other => return err(format!("unknown nav action: {other}")),
    };
    let _ = page.evaluate(js).await;
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
}
/// Click at viewport CSS-pixel coordinates (used by the clickable screenshot).
async fn api_click_at(State(s): State<AppState>, Json(b): Json<ClickAtReq>) -> Json<serde_json::Value> {
    if !b.x.is_finite() || !b.y.is_finite() || b.x < 0.0 || b.y < 0.0 {
        return err("invalid coordinates".into());
    }
    let (tab_id, page) = tab!(&s, b.tab);
    s.push_log("click-at", &format!("{}, {}", b.x.round(), b.y.round()));
    s.set_cursor(tab_id, b.x, b.y, "click", &b.by).await;
    let point = chromiumoxide::layout::Point::new(b.x, b.y);
    match page.click(point).await {
        Ok(_) => {
            tokio::time::sleep(Duration::from_millis(120)).await;
            ok("clicked")
        }
        Err(e) => err(format!("click failed: {e:#}")),
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
    match page.evaluate(js.as_str()).await {
        Ok(_) => {
            tokio::time::sleep(Duration::from_millis(60)).await;
            ok("scrolled")
        }
        Err(e) => err(format!("scroll failed: {e:#}")),
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
    match page.evaluate(call.as_str()).await {
        Ok(r) => match r.into_value::<serde_json::Value>() {
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
        Err(e) => err(format!("select failed: {e:#}")),
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
    let zoom = {
        let mut t = s.tabs.lock().await;
        let ts = t.map.get_mut(&tab_id).unwrap();
        ts.zoom = step_zoom(ts.zoom, b.action.as_str());
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
    match page.execute(InsertTextParams::new(b.text)).await {
        Ok(_) => {
            tokio::time::sleep(Duration::from_millis(60)).await;
            ok("inserted")
        }
        Err(e) => err(format!("insert failed: {e:#}")),
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
struct HoverReq {
    /// Tab id (default: active tab).
    #[serde(default)]
    tab: Option<u32>,
    x: f64,
    y: f64,
}
/// Move the mouse without clicking (drives :hover states, tooltips, menus).
/// Deliberately unlogged — hover fires continuously.
async fn api_hover(State(s): State<AppState>, Json(b): Json<HoverReq>) -> Json<serde_json::Value> {
    if !b.x.is_finite() || !b.y.is_finite() || b.x < 0.0 || b.y < 0.0 {
        return err("invalid coordinates".into());
    }
    let (_tab_id, page) = tab!(&s, b.tab);
    match page
        .move_mouse(chromiumoxide::layout::Point::new(b.x, b.y))
        .await
    {
        Ok(_) => Json(serde_json::json!({ "ok": true })),
        Err(e) => err(format!("hover failed: {e:#}")),
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
    if let Err(e) = page.move_mouse(chromiumoxide::layout::Point::new(b.x, b.y)).await {
        return err(format!("mouse move failed: {e:#}"));
    }
    let params = DispatchMouseEventParams::builder()
        .r#type(DispatchMouseEventType::MouseWheel)
        .x(b.x)
        .y(b.y)
        .delta_x(b.dx)
        .delta_y(b.dy)
        .build()
        .unwrap();
    match page.execute(params).await {
        Ok(_) => {
            tokio::time::sleep(Duration::from_millis(60)).await;
            ok("wheeled")
        }
        Err(e) => err(format!("wheel failed: {e:#}")),
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
    match run.await {
        Ok(()) => {
            s.set_cursor(tab_id, b.x2, b.y2, "drag", &b.by).await;
            tokio::time::sleep(Duration::from_millis(120)).await;
            ok("dragged")
        }
        Err(e) => err(format!("drag failed: {e:#}")),
    }
}

/// Viewport metrics for mapping screenshot pixels → page coordinates.
async fn api_metrics(
    State(s): State<AppState>,
    Query(q): Query<TabQuery>,
) -> Json<serde_json::Value> {
    let (_tab_id, page) = tab!(&s, q.tab);
    const METRICS_JS: &str = "({w: window.innerWidth, h: window.innerHeight, dpr: window.devicePixelRatio || 1, sx: window.scrollX, sy: window.scrollY})";
    match page.evaluate(METRICS_JS).await {
        Ok(r) => match r.into_value::<serde_json::Value>() {
            Ok(m) => Json(serde_json::json!({ "ok": true, "metrics": m })),
            Err(e) => err(format!("decode metrics failed: {e:#}")),
        },
        Err(e) => err(format!("metrics failed: {e:#}")),
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
async fn api_tabs(State(s): State<AppState>) -> Json<serde_json::Value> {
    // Snapshot ids first so we don't hold the lock across CDP calls.
    let (ids, active) = {
        let t = s.tabs.lock().await;
        (t.map.keys().copied().collect::<Vec<_>>(), t.active)
    };
    let mut tabs = vec![];
    for id in ids {
        let page = match s.tabs.lock().await.map.get(&id).map(|ts| ts.page.clone()) {
            Some(p) => p,
            None => continue,
        };
        let url = page_url(&page).await;
        let title = page_title(&page).await;
        let t = s.tabs.lock().await;
        let ts = t.map.get(&id).unwrap();
        tabs.push(serde_json::json!({
            "id": id,
            "active": id == active,
            "title": title,
            "url": url,
            "handover": ts.handover.active,
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
    let removed = s.tabs.lock().await.map.remove(&tab_id);
    match removed {
        Some(ts) => {
            let url = page_url(&ts.page).await;
            let _ = ts.page.close().await;
            s.push_log("close", &format!("tab {tab_id} ({url})"));
            // Never leave the daemon tabless: open a fresh blank tab.
            let mut t = s.tabs.lock().await;
            if t.map.is_empty() {
                drop(t);
                match s.browser.lock().await.new_page("about:blank").await {
                    Ok(page) => {
                        let mut t = s.tabs.lock().await;
                        let id = t.next_id;
                        t.next_id += 1;
                        t.map.insert(id, TabState {
                            page,
                            handover: HandoverState { active: false, message: String::new(), claimed: false, requested_at: None },
                            zoom: 1.0,
                            cursor: CursorState::default(),
                            last_url: String::new(),
                        });
                        t.active = id;
                        return ok(&format!("closed tab {tab_id}; opened fresh tab {id}"));
                    }
                    Err(e) => return err(format!("closed tab {tab_id} but fresh tab failed: {e:#}")),
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
    let _ = page.bring_to_front().await;
    s.push_log("activate", &format!("tab {tab_id}"));
    ok(&format!("tab {tab_id} active"))
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
    let profile = args.profile.clone().unwrap_or_else(default_profile_dir);
    std::fs::create_dir_all(&profile).ok();

    let mut cfg = BrowserConfig::builder()
        .user_data_dir(std::path::PathBuf::from(&profile))
        .window_size(1280, 900)
        .launch_timeout(Duration::from_secs(60))
        .request_timeout(Duration::from_secs(20));

    if let Some(chrome) = &args.chrome {
        cfg = cfg.chrome_executable(chrome.as_str());
    } else if let Some(detected) = detect_chrome() {
        cfg = cfg.chrome_executable(detected);
    }

    if args.headless_mode() {
        // Headless is the default (no visible window); --headed opts out.
    } else {
        cfg = cfg.with_head();
    }
    if args.no_sandbox {
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
    ]);

    tracing::info!(
        "launching chrome (profile: {profile}, {}) …",
        if args.headless_mode() { "headless" } else { "headed" }
    );
    let (browser, mut handler) = Browser::launch(cfg.build().map_err(|e| anyhow::anyhow!(e))?)
        .await
        .context("failed to launch Chrome — try --chrome /path/to/chrome or --no-sandbox")?;

    // Drive the browser handler in the background.
    tokio::spawn(async move {
        while let Some(h) = handler.next().await {
            if h.is_err() {
                break;
            }
        }
    });

    let start_url = args.url.clone().unwrap_or_else(|| "about:blank".into());
    let page = browser
        .new_page(start_url.as_str())
        .await
        .context("failed to open browser page")?;

    let mut tabs_map = std::collections::HashMap::new();
    tabs_map.insert(
        1u32,
        TabState {
            page,
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
    let state = AppState {
        tabs: Arc::new(Mutex::new(Tabs { map: tabs_map, active: 1, next_id: 2 })),
        browser: Arc::new(Mutex::new(browser)),
        log: Arc::new(Mutex::new(vec![LogEntry {
            at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            action: "daemon".into(),
            detail: "browser launched".into(),
        }])),
        passcode: passcode.clone(),
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
        .route("/api/handover", get(api_handover_get))
        .route("/api/handover/request", post(api_handover_request))
        .route("/api/handover/claim", post(api_handover_claim))
        .route("/api/handover/done", post(api_handover_done))
        .route("/api/log", get(api_log))
        .route("/api/tabs", get(api_tabs))
        .route("/api/close", post(api_close))
        .route("/api/activate", post(api_activate))
        .layer(axum::middleware::from_fn_with_state(state.clone(), require_passcode))
        // Live tool: nothing is cacheable (stale tabs/snapshots caused real
        // confusion — the UI once needed a manual refresh to show new tabs).
        .layer(tower_http::set_header::SetResponseHeaderLayer::if_not_present(
            header::CACHE_CONTROL,
            axum::http::HeaderValue::from_static("no-store"),
        ))
        .fallback_service(tower_http::services::ServeDir::new(dist_dir()))
        .with_state(state);

    let addr = format!("127.0.0.1:{}", args.port);
    let listener = tokio::net::TcpListener::bind(&addr).await.map_err(|e| {
        anyhow::anyhow!(
            "cannot bind {addr}: {e:#} — is another `lb web` already running? (`lb web stop` stops it)"
        )
    })?;
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
    println!("  UI:  http://{addr}/");
    println!("  API: http://{addr}/api/status");
    if passcode.is_some() {
        println!("  auth: passcode protection enabled");
    }
    println!("CLI example:");
    if passcode.is_some() {
        println!("  lb --server http://{addr} --passcode <code> open https://example.com");
    } else {
        println!("  lb --server http://{addr} open https://example.com");
    }
    println!("  lb --server http://{addr} snapshot");
    println!("  lb --server http://{addr} handover \"please log in, then click Done in the UI\"");
    println!("  (stop this server with: lb web --port {} stop)", args.port);
    if !args.no_open {
        open_in_default_browser(&format!("http://{addr}/"));
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
}
