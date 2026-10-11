use anyhow::{Context, Result};
use axum::{
    Json, Router,
    extract::State,
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

#[derive(Clone)]
struct AppState {
    page: Page,
    handover: Arc<Mutex<HandoverState>>,
    log: Arc<Mutex<Vec<LogEntry>>>,
    /// Page zoom factor (CSS `body.style.zoom`), re-applied after navigations.
    zoom: Arc<Mutex<f64>>,
    /// Last pointer action, rendered as a fake cursor in the web UI.
    cursor: Arc<Mutex<CursorState>>,
    /// Last healthy page URL — reload recovers here from error pages.
    last_url: Arc<Mutex<String>>,
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

    async fn set_cursor(&self, x: f64, y: f64, kind: &str, by: &str) {
        let mut c = self.cursor.lock().await;
        c.x = x;
        c.y = y;
        c.kind = kind.to_string();
        c.by = if by == "human" { "human".into() } else { "agent".into() };
        c.at = chrono::Utc::now().timestamp_millis();
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

async fn api_status(State(s): State<AppState>) -> Json<serde_json::Value> {
    let url = page_url(&s.page).await;
    let title = page_title(&s.page).await;
    let ho = s.handover.lock().await.clone();
    let zoom = *s.zoom.lock().await;
    let cursor = s.cursor.lock().await.clone();
    Json(serde_json::json!({
        "ok": true,
        "url": url,
        "title": title,
        "handover": ho,
        "zoom": zoom,
        "cursor": cursor,
    }))
}

#[derive(Deserialize)]
struct OpenReq {
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
    s.push_log("open", &url);
    // The network here can flap (VPN/tailscale), so retry transient failures.
    let mut last_err = String::new();
    for attempt in 1..=3 {
        let goto = s.page.goto(&url).await;
        // goto may report transient network errors (e.g. ERR_NETWORK_CHANGED)
        // while the navigation still succeeds — verify by current URL.
        tokio::time::sleep(Duration::from_millis(1500)).await;
        let current = page_url(&s.page).await;
        if goto.is_ok() || !current.starts_with("chrome-error") {
            // Give the page a moment to keep loading; don't hard-fail on waits.
            let _ = tokio::time::timeout(Duration::from_secs(10), s.page.wait_for_navigation()).await;
            let landed = page_url(&s.page).await;
            // Remember the last healthy URL so reload can recover from errors.
            if !landed.starts_with("chrome-error") && !landed.is_empty() {
                *s.last_url.lock().await = landed.clone();
            }
            // Navigations reset document zoom — re-apply the stored level.
            apply_zoom(&s.page, *s.zoom.lock().await).await;
            let note = if attempt > 1 { format!(" (after {attempt} attempts)") } else { String::new() };
            return ok(&format!("opened {url} (now at {landed}){note}"));
        }
        last_err = format!("{:#}", goto.unwrap_err());
        s.push_log("open/retry", &format!("attempt {attempt}: {last_err}"));
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    err(format!("goto failed after 3 attempts (network flapping?): {last_err}"))
}

async fn api_snapshot(State(s): State<AppState>) -> Json<serde_json::Value> {
    let url = page_url(&s.page).await;
    let title = page_title(&s.page).await;
    match s.page.evaluate(SNAPSHOT_JS).await {
        Ok(eval) => match eval.into_value::<serde_json::Value>() {
            Ok(elements) => Json(serde_json::json!({
                "ok": true, "url": url, "title": title, "elements": elements,
                "hint": "click with `lb click <ref>`, type with `lb type <ref> <text>`. Re-run snapshot after navigation."
            })),
            Err(e) => err(format!("decode snapshot failed: {e:#}")),
        },
        Err(e) => err(format!("snapshot failed (page still loading?): {e:#}")),
    }
}

async fn api_screenshot(State(s): State<AppState>) -> impl IntoResponse {
    // Never let a slow screenshot pile up: the UI polls every ~2s, so fail
    // fast (the UI just retries on the next tick).
    let params = chromiumoxide::page::ScreenshotParams::builder()
        .format(chromiumoxide::cdp::browser_protocol::page::CaptureScreenshotFormat::Jpeg)
        .quality(70)
        .build();
    let shot = tokio::time::timeout(Duration::from_secs(10), s.page.screenshot(params)).await;
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
    target: String,
    /// "human" when triggered from the web UI; anything else = agent.
    #[serde(default)]
    by: String,
}
async fn api_click(State(s): State<AppState>, Json(b): Json<ClickReq>) -> Json<serde_json::Value> {
    let sel = target_to_selector(&b.target);
    s.push_log("click", &sel);
    // Ensure refs exist (snapshot tags elements). Best-effort.
    let _ = s.page.evaluate(SNAPSHOT_JS).await;
    match s.page.find_element(&sel).await {
        Ok(el) => {
            // Report the click point for the UI's fake cursor.
            let center_js = format!(
                "(() => {{ const el = document.querySelector({}); if (!el) return null; const r = el.getBoundingClientRect(); return [r.left + r.width / 2, r.top + r.height / 2]; }})()",
                serde_json::to_string(&sel).unwrap()
            );
            if let Ok(ev) = s.page.evaluate(center_js.as_str()).await {
                if let Ok(pt) = ev.into_value::<Vec<f64>>() {
                    if pt.len() == 2 {
                        s.set_cursor(pt[0], pt[1], "click", &b.by).await;
                    }
                }
            }
            match el.click().await {
                Ok(_) => {
                    tokio::time::sleep(Duration::from_millis(800)).await;
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
    target: String,
    text: String,
    #[serde(default)]
    submit: bool,
}
async fn api_type(State(s): State<AppState>, Json(b): Json<TypeReq>) -> Json<serde_json::Value> {
    let sel = target_to_selector(&b.target);
    s.push_log("type", &format!("{sel} <- {:?}", b.text));
    let _ = s.page.evaluate(SNAPSHOT_JS).await;
    let el = match s.page.find_element(&sel).await {
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
    let _ = s.page.evaluate(clear_js.as_str()).await;
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
        tokio::time::sleep(Duration::from_millis(800)).await;
    }
    ok(&format!("typed into {sel}"))
}

#[derive(Deserialize)]
struct PressReq {
    key: String,
}
async fn api_press(State(s): State<AppState>, Json(b): Json<PressReq>) -> Json<serde_json::Value> {
    s.push_log("press", &b.key);
    match s.page.find_element("body").await {
        Ok(el) => match el.press_key(b.key.as_str()).await {
            Ok(_) => {
                tokio::time::sleep(Duration::from_millis(400)).await;
                ok(&format!("pressed {}", b.key))
            }
            Err(e) => err(format!("press failed: {e:#}")),
        },
        Err(e) => err(format!("press failed (no body?): {e:#}")),
    }
}

#[derive(Deserialize)]
struct EvalReq {
    js: String,
}
async fn api_eval(State(s): State<AppState>, Json(b): Json<EvalReq>) -> Json<serde_json::Value> {
    s.push_log("eval", &b.js);
    match s.page.evaluate(b.js.as_str()).await {
        Ok(r) => match r.into_value::<serde_json::Value>() {
            Ok(v) => Json(serde_json::json!({ "ok": true, "result": v })),
            Err(_) => Json(serde_json::json!({ "ok": true, "result": "(non-JSON value — side effects applied)" })),
        },
        Err(e) => err(format!("eval failed: {e:#}")),
    }
}

#[derive(Deserialize)]
struct NavReq {
    action: String,
}
async fn api_nav(State(s): State<AppState>, Json(b): Json<NavReq>) -> Json<serde_json::Value> {
    s.push_log(&b.action, "");
    // Reloading a chrome-error page just redisplays the error — recover by
    // re-opening the last healthy URL instead.
    if b.action == "reload" {
        let current = page_url(&s.page).await;
        if current.starts_with("chrome-error") {
            let last = s.last_url.lock().await.clone();
            if last.is_empty() {
                return err("error page with no previous URL — use open instead".into());
            }
            s.push_log("reload/recover", &last);
            let goto = s.page.goto(&last).await;
            tokio::time::sleep(Duration::from_millis(1500)).await;
            let landed = page_url(&s.page).await;
            if goto.is_ok() || !landed.starts_with("chrome-error") {
                if !landed.starts_with("chrome-error") && !landed.is_empty() {
                    *s.last_url.lock().await = landed.clone();
                }
                apply_zoom(&s.page, *s.zoom.lock().await).await;
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
    let _ = s.page.evaluate(js).await;
    tokio::time::sleep(Duration::from_millis(800)).await;
    apply_zoom(&s.page, *s.zoom.lock().await).await;
    ok(&format!("navigated {}", b.action))
}

#[derive(Deserialize)]
struct ClickAtReq {
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
    s.push_log("click-at", &format!("{}, {}", b.x.round(), b.y.round()));
    s.set_cursor(b.x, b.y, "click", &b.by).await;
    let point = chromiumoxide::layout::Point::new(b.x, b.y);
    match s.page.click(point).await {
        Ok(_) => {
            tokio::time::sleep(Duration::from_millis(700)).await;
            ok("clicked")
        }
        Err(e) => err(format!("click failed: {e:#}")),
    }
}

#[derive(Deserialize)]
struct ScrollReq {
    #[serde(default)]
    dx: f64,
    #[serde(default)]
    dy: f64,
}
async fn api_scroll(State(s): State<AppState>, Json(b): Json<ScrollReq>) -> Json<serde_json::Value> {
    if !b.dx.is_finite() || !b.dy.is_finite() {
        return err("invalid scroll delta".into());
    }
    s.push_log("scroll", &format!("{}, {}", b.dx, b.dy));
    let js = format!("window.scrollBy({}, {})", b.dx, b.dy);
    match s.page.evaluate(js.as_str()).await {
        Ok(_) => {
            tokio::time::sleep(Duration::from_millis(400)).await;
            ok("scrolled")
        }
        Err(e) => err(format!("scroll failed: {e:#}")),
    }
}

#[derive(Deserialize)]
struct KeyReq {
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
    target: String,
    /// Option value (or visible text — matched case-insensitively as fallback).
    value: String,
}
/// Set a <select> option by value (or visible text). Native dropdown popups
/// don't work through CDP clicks, so this drives the element directly and
/// fires the same input/change events a real choice would.
async fn api_select(State(s): State<AppState>, Json(b): Json<SelectReq>) -> Json<serde_json::Value> {
    let sel = target_to_selector(&b.target);
    s.push_log("select", &format!("{sel} <- {:?}", b.value));
    let _ = s.page.evaluate(SNAPSHOT_JS).await;
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
    match s.page.evaluate(call.as_str()).await {
        Ok(r) => match r.into_value::<serde_json::Value>() {
            Ok(v) => {
                if v.get("error").and_then(|x| x.as_str()).is_some() {
                    // Fall back to keyboard-driving the native control.
                    return select_via_keyboard(&s, &sel, &b.value).await;
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
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
async fn select_via_keyboard(s: &AppState, sel: &str, want: &str) -> Json<serde_json::Value> {
    let el = match s.page.find_element(sel).await {
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
    tokio::time::sleep(Duration::from_millis(500)).await;
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
    // Keystrokes are intentionally not logged: at ~2 events per key they'd
    // drown the action log. Failures still surface via the API response.
    match s.page.execute(cmd.build().unwrap()).await {
        Ok(_) => Json(serde_json::json!({ "ok": true })),
        Err(e) => err(format!("key event failed: {e:#}")),
    }
}

#[derive(Deserialize)]
struct ZoomReq {
    /// "in", "out" or "reset"
    #[serde(default = "default_zoom_action")]
    action: String,
}
fn default_zoom_action() -> String {
    "in".to_string()
}
async fn api_zoom(State(s): State<AppState>, Json(b): Json<ZoomReq>) -> Json<serde_json::Value> {
    let mut z = s.zoom.lock().await;
    *z = step_zoom(*z, b.action.as_str());
    let zoom = *z;
    drop(z);
    s.push_log("zoom", &format!("{zoom}"));
    apply_zoom(&s.page, zoom).await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    Json(serde_json::json!({ "ok": true, "zoom": zoom, "message": format!("zoom {zoom}") }))
}

#[derive(Deserialize)]
struct InsertReq {
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
    s.push_log("insert", &format!("{} chars", b.text.chars().count()));
    match s.page.execute(InsertTextParams::new(b.text)).await {
        Ok(_) => {
            tokio::time::sleep(Duration::from_millis(400)).await;
            ok("inserted")
        }
        Err(e) => err(format!("insert failed: {e:#}")),
    }
}

#[derive(Deserialize)]
struct TypeRawReq {
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
    s.push_log("type-raw", &format!("{} chars", b.text.chars().count()));
    for c in b.text.chars() {
        let s_ = c.to_string();
        // CDP insertText needs at least one char; newlines are fine.
        if s_.is_empty() {
            continue;
        }
        if let Err(e) = s.page.execute(InsertTextParams::new(s_)).await {
            return err(format!("type-raw failed: {e:#}"));
        }
    }
    tokio::time::sleep(Duration::from_millis(400)).await;
    ok("typed")
}

#[derive(Deserialize)]
struct HoverReq {
    x: f64,
    y: f64,
}
/// Move the mouse without clicking (drives :hover states, tooltips, menus).
/// Deliberately unlogged — hover fires continuously.
async fn api_hover(State(s): State<AppState>, Json(b): Json<HoverReq>) -> Json<serde_json::Value> {
    if !b.x.is_finite() || !b.y.is_finite() || b.x < 0.0 || b.y < 0.0 {
        return err("invalid coordinates".into());
    }
    match s
        .page
        .move_mouse(chromiumoxide::layout::Point::new(b.x, b.y))
        .await
    {
        Ok(_) => Json(serde_json::json!({ "ok": true })),
        Err(e) => err(format!("hover failed: {e:#}")),
    }
}

#[derive(Deserialize)]
struct WheelReq {
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
    if let Err(e) = s.page.move_mouse(chromiumoxide::layout::Point::new(b.x, b.y)).await {
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
    match s.page.execute(params).await {
        Ok(_) => {
            tokio::time::sleep(Duration::from_millis(300)).await;
            ok("wheeled")
        }
        Err(e) => err(format!("wheel failed: {e:#}")),
    }
}

#[derive(Deserialize)]
struct DragReq {
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
    s.push_log("drag", &format!("({},{}) -> ({},{})", b.x1, b.y1, b.x2, b.y2));
    s.set_cursor(b.x1, b.y1, "drag", &b.by).await;
    // Field recipe mirrors Puppeteer's Mouse exactly (verified against
    // puppeteer-core 25: moves carry button+buttons, press carries
    // buttons=1 AFTER engaging, release carries buttons=0 AFTER clearing).
    let run = async {
        s.page
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
        s.page
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
            s.page
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
        s.page
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
            s.set_cursor(b.x2, b.y2, "drag", &b.by).await;
            tokio::time::sleep(Duration::from_millis(500)).await;
            ok("dragged")
        }
        Err(e) => err(format!("drag failed: {e:#}")),
    }
}

/// Viewport metrics for mapping screenshot pixels → page coordinates.
async fn api_metrics(State(s): State<AppState>) -> Json<serde_json::Value> {
    const METRICS_JS: &str = "({w: window.innerWidth, h: window.innerHeight, dpr: window.devicePixelRatio || 1, sx: window.scrollX, sy: window.scrollY})";
    match s.page.evaluate(METRICS_JS).await {
        Ok(r) => match r.into_value::<serde_json::Value>() {
            Ok(m) => Json(serde_json::json!({ "ok": true, "metrics": m })),
            Err(e) => err(format!("decode metrics failed: {e:#}")),
        },
        Err(e) => err(format!("metrics failed: {e:#}")),
    }
}

#[derive(Deserialize)]
struct HandoverReq {
    message: String,
}
async fn api_handover_request(
    State(s): State<AppState>,
    Json(b): Json<HandoverReq>,
) -> Json<serde_json::Value> {
    let mut ho = s.handover.lock().await;
    ho.active = true;
    ho.message = b.message.clone();
    ho.claimed = false;
    ho.requested_at = Some(chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true));
    s.push_log("handover/request", &b.message);
    Json(serde_json::json!({ "ok": true, "message": "handover requested — human should take over the browser window, then click Done in the web UI" }))
}

#[derive(Deserialize)]
struct ClaimReq {
    #[serde(default)]
    message: String,
}
/// Human proactively claims control from the web UI (no agent request needed).
async fn api_handover_claim(
    State(s): State<AppState>,
    Json(b): Json<ClaimReq>,
) -> Json<serde_json::Value> {
    let mut ho = s.handover.lock().await;
    ho.active = true;
    ho.message = if b.message.trim().is_empty() {
        "Human claimed control from the web UI.".to_string()
    } else {
        b.message.clone()
    };
    ho.claimed = true;
    ho.requested_at = Some(chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true));
    s.push_log("handover/claim", &ho.message.clone());
    Json(serde_json::json!({ "ok": true, "message": "control claimed — the agent should hold off until you click Done in the web UI" }))
}

async fn api_handover_get(State(s): State<AppState>) -> Json<serde_json::Value> {
    let ho = s.handover.lock().await.clone();
    Json(serde_json::json!({ "ok": true, "active": ho.active, "claimed": ho.claimed, "message": ho.message, "requested_at": ho.requested_at }))
}

async fn api_handover_done(State(s): State<AppState>) -> Json<serde_json::Value> {
    let mut ho = s.handover.lock().await;
    ho.active = false;
    s.push_log("handover/done", "human returned control to agent");
    ok("handover complete — agent may continue (re-run `lb snapshot`)")
}

async fn api_log(State(s): State<AppState>) -> Json<serde_json::Value> {
    let entries = s.log.lock().await.clone();
    Json(serde_json::json!({ "ok": true, "entries": entries }))
}

// ---------------------------------------------------------------------------
// Static UI
// ---------------------------------------------------------------------------

/// Serve the built Svelte app from `web/dist` (sibling of crate root).
/// Falls back to a minimal placeholder page when the UI hasn't been built yet.
async fn ui_index() -> Html<String> {
    let index = dist_dir().join("index.html");
    if let Ok(html) = tokio::fs::read_to_string(&index).await {
        return Html(html);
    }
    Html(PLACEHOLDER_HTML.to_string())
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
    if let Ok(exe) = std::env::current_exe() {
        let mut dir = exe.parent().map(|p| p.to_path_buf());
        while let Some(d) = dir {
            let cand = d.join("web").join("dist");
            if cand.join("index.html").exists() {
                return cand;
            }
            if d.join("Cargo.toml").exists() {
                return d.join("web").join("dist");
            }
            dir = d.parent().map(|p| p.to_path_buf());
        }
    }
    cwd
}

const PLACEHOLDER_HTML: &str = r#"<!doctype html><html><body style="font-family:sans-serif;max-width:640px;margin:4rem auto">
<h1>Live Browser</h1>
<p>Daemon is running, but the Svelte UI hasn't been built yet.</p>
<pre>cd web && npm install && npm run build</pre>
<p>Meanwhile the JSON API works: <a href="/api/status">/api/status</a></p>
</body></html>"#;

// ---------------------------------------------------------------------------
// Daemon entrypoint
// ---------------------------------------------------------------------------

pub async fn run_daemon(args: DaemonArgs) -> Result<()> {
    let profile = args.profile.clone().unwrap_or_else(|| {
        let base = std::env::var("XDG_CACHE_HOME")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| {
                let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
                std::path::PathBuf::from(home).join(".cache")
            });
        let dir = base.join("live-browser").join("profile");
        // One-time migration from the old `simple-browser` profile location.
        let legacy = base.join("simple-browser").join("profile");
        if !dir.exists() && legacy.exists() && std::fs::rename(&legacy, &dir).is_ok() {
            tracing::info!("migrated browser profile from {}", legacy.display());
        }
        dir.to_string_lossy().to_string()
    });
    std::fs::create_dir_all(&profile).ok();

    let mut cfg = BrowserConfig::builder()
        .user_data_dir(std::path::PathBuf::from(&profile))
        .window_size(1280, 900)
        .launch_timeout(Duration::from_secs(60))
        .request_timeout(Duration::from_secs(20));

    if let Some(chrome) = &args.chrome {
        cfg = cfg.chrome_executable(chrome.as_str());
    } else if std::path::Path::new("/usr/bin/google-chrome-stable").exists() {
        cfg = cfg.chrome_executable("/usr/bin/google-chrome-stable");
    }

    if args.headless {
        // default builder is headless already
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

    tracing::info!("launching chrome (profile: {profile}) …");
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

    let page = browser
        .new_page(args.url.as_deref().unwrap_or("about:blank"))
        .await
        .context("failed to open browser page")?;

    let state = AppState {
        page,
        handover: Arc::new(Mutex::new(HandoverState {
            active: false,
            message: String::new(),
            claimed: false,
            requested_at: None,
        })),
        zoom: Arc::new(Mutex::new(1.0)),
        cursor: Arc::new(Mutex::new(CursorState::default())),
        last_url: Arc::new(Mutex::new(String::new())),
        log: Arc::new(Mutex::new(vec![LogEntry {
            at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            action: "daemon".into(),
            detail: "browser launched".into(),
        }])),
    };

    // Keep the browser alive for the process lifetime.
    let _browser_guard: Arc<Mutex<Browser>> = Arc::new(Mutex::new(browser));

    if let Some(u) = &args.url {
        *state.last_url.lock().await = u.clone();
    }

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
        .fallback_service(tower_http::services::ServeDir::new(dist_dir()))
        .with_state(state);

    let addr = format!("127.0.0.1:{}", args.port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    println!("live-browser daemon ready");
    println!("  UI:  http://{addr}/");
    println!("  API: http://{addr}/api/status");
    println!("CLI example:");
    println!("  lb --server http://{addr} open https://example.com");
    println!("  lb --server http://{addr} snapshot");
    println!("  lb --server http://{addr} handover \"please log in, then click Done in the UI\"");
    axum::serve(listener, app).await?;
    Ok(())
}
