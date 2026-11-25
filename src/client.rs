use anyhow::{Context, Result};
use serde_json::Value;
use std::time::Duration;

fn api(server: &str, path: &str) -> String {
    format!("{}{}", server.trim_end_matches('/'), path)
}

/// Shared HTTP client (connection reuse / keep-alive within a process).
/// `reqwest::Client::new()` per request disables pooling and adds TLS/pool
/// setup overhead to every CLI call — even against localhost.
fn http() -> reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(60))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new())
        })
        .clone()
}

/// `?tab=N` suffix for GET endpoints (empty for the active tab).
fn tq(tab: Option<u32>) -> String {
    match tab {
        Some(id) => format!("?tab={id}"),
        None => String::new(),
    }
}

fn print_json(v: &Value) {
    println!("{}", serde_json::to_string_pretty(v).unwrap());
}

fn with_auth(
    builder: reqwest::RequestBuilder,
    passcode: Option<&str>,
) -> reqwest::RequestBuilder {
    match passcode {
        Some(p) if !p.is_empty() => builder.header("x-lb-passcode", p),
        _ => builder,
    }
}

fn unauthorized_value() -> Value {
    serde_json::json!({
        "ok": false,
        "error": "unauthorized — wrong or missing passcode (set --passcode / LB_PASSCODE)",
    })
}

async fn get(server: &str, passcode: Option<&str>, path: &str) -> Result<Value> {
    let res = with_auth(http().get(api(server, path)), passcode)
        .send()
        .await
        .context("daemon not reachable — did you run `lb web`?")?;
    if res.status() == reqwest::StatusCode::UNAUTHORIZED {
        return Ok(unauthorized_value());
    }
    let status = res.status();
    let body = res.text().await.context("invalid daemon response")?;
    if !status.is_success() {
        let snippet: String = body.chars().take(200).collect();
        anyhow::bail!("daemon error (HTTP {status}): {snippet}");
    }
    serde_json::from_str(&body).context("invalid daemon response")
}

async fn post(server: &str, passcode: Option<&str>, path: &str, body: Value) -> Result<Value> {
    let res = with_auth(http().post(api(server, path)), passcode)
        .json(&body)
        .send()
        .await
        .context("daemon not reachable — did you run `lb web`?")?;
    if res.status() == reqwest::StatusCode::UNAUTHORIZED {
        return Ok(unauthorized_value());
    }
    let status = res.status();
    let text = res.text().await.context("invalid daemon response")?;
    if !status.is_success() {
        let snippet: String = text.chars().take(200).collect();
        anyhow::bail!("daemon error (HTTP {status}): {snippet}");
    }
    serde_json::from_str(&text).context("invalid daemon response")
}

fn check_ok(v: &Value, json: bool) -> Result<()> {
    if v.get("ok").and_then(|x| x.as_bool()) == Some(true) {
        if json {
            print_json(v);
        } else if let Some(m) = v.get("message").and_then(|x| x.as_str()) {
            println!("{m}");
        } else {
            print_json(v);
        }
        Ok(())
    } else {
        let err = v
            .get("error")
            .and_then(|x| x.as_str())
            .unwrap_or("unknown daemon error");
        if json {
            print_json(v);
        } else {
            eprintln!("error: {err}");
        }
        std::process::exit(1);
    }
}

pub async fn open(server: &str, passcode: Option<&str>, url: &str, tab: Option<u32>, new_tab: bool, json: bool) -> Result<()> {
    let v = post(
        server,
        passcode,
        "/api/open",
        serde_json::json!({ "url": url, "tab": tab, "new_tab": new_tab }),
    )
    .await?;
    check_ok(&v, json)
}

pub async fn status(server: &str, passcode: Option<&str>, tab: Option<u32>, json: bool) -> Result<()> {
    let v = get(server, passcode, &format!("/api/status{}", tq(tab))).await?;
    if v.get("ok").and_then(|x| x.as_bool()) != Some(true) {
        eprintln!(
            "error: {}",
            v.get("error").and_then(|x| x.as_str()).unwrap_or("status failed")
        );
        std::process::exit(1);
    }
    if json {
        print_json(&v);
    } else {
        let id = v.get("tab").and_then(|x| x.as_u64()).unwrap_or(0);
        let is_active = v.get("active").and_then(|x| x.as_bool()).unwrap_or(true);
        let url = v.get("url").and_then(|x| x.as_str()).unwrap_or("?");
        let title = v.get("title").and_then(|x| x.as_str()).unwrap_or("");
        let ho = v.get("handover").cloned().unwrap_or(Value::Null);
        let active = ho.get("active").and_then(|x| x.as_bool()).unwrap_or(false);
        println!("tab:   {id}{}", if is_active { " (active)" } else { "" });
        println!("url:   {url}");
        println!("title: {title}");
        println!("ui:    {}/", server.trim_end_matches('/'));
        if active {
            println!(
                "handover: ACTIVE — {}",
                ho.get("message").and_then(|x| x.as_str()).unwrap_or("")
            );
        } else {
            println!("handover: none");
        }
        if v.get("locked").and_then(|x| x.as_bool()) == Some(true) {
            println!("locked: YES — the user locked the browser in the web UI; actions fail until they unlock it");
        }
    }
    Ok(())
}

pub async fn snapshot(server: &str, passcode: Option<&str>, tab: Option<u32>, json: bool, full: bool) -> Result<()> {
    let v = get(server, passcode, &format!("/api/snapshot{}", tq(tab))).await?;
    if json {
        print_json(&v);
        return Ok(());
    }
    // Compact agent-friendly lines: [ref] tag "name" -- href
    if v.get("ok").and_then(|x| x.as_bool()) != Some(true) {
        eprintln!(
            "error: {}",
            v.get("error").and_then(|x| x.as_str()).unwrap_or("snapshot failed")
        );
        std::process::exit(1);
    }
    let id = v.get("tab").and_then(|x| x.as_u64()).unwrap_or(0);
    let url = v.get("url").and_then(|x| x.as_str()).unwrap_or("");
    let title = v.get("title").and_then(|x| x.as_str()).unwrap_or("");
    println!("# tab {id}: {title} <{url}>");
    let empty = vec![];
    let els = v.get("elements").and_then(|x| x.as_array()).unwrap_or(&empty);
    if els.is_empty() {
        println!("(no interactive elements found — page may still be loading; try `lb wait <target>` or `lb snapshot` again)");
    }
    for el in els {
        let r = el.get("ref").and_then(|x| x.as_u64()).unwrap_or(0);
        let tag = el.get("tag").and_then(|x| x.as_str()).unwrap_or("?");
        let name = el.get("name").and_then(|x| x.as_str()).unwrap_or("");
        let href = el.get("href").and_then(|x| x.as_str()).unwrap_or("");
        if !full {
            // Mark refs living in cross-origin (out-of-process) iframes.
            let xo = el
                .get("frame")
                .and_then(|x| x.as_str())
                .filter(|f| f.starts_with("oopif["))
                .map(|f| format!("  [in {f}]"))
                .unwrap_or_default();
            if href.is_empty() {
                println!("[{r}] {tag} \"{name}\"{xo}");
            } else {
                println!("[{r}] {tag} \"{name}\" -- {href}{xo}");
            }
            continue;
        }
        // --full: one detailed line per element (type/value/frame/coords).
        let typ = el.get("type").and_then(|x| x.as_str()).unwrap_or("");
        let value = el.get("value").and_then(|x| x.as_str()).unwrap_or("");
        let frame = el.get("frame").and_then(|x| x.as_str()).unwrap_or("");
        let x = el.get("x").and_then(|x| x.as_f64()).unwrap_or(0.0);
        let y = el.get("y").and_then(|x| x.as_f64()).unwrap_or(0.0);
        let mut extra = vec![];
        if !typ.is_empty() {
            extra.push(format!("type={typ}"));
        }
        if !value.is_empty() {
            extra.push(format!("value={value:?}"));
        }
        if !frame.is_empty() {
            extra.push(format!("frame={frame}"));
        }
        extra.push(format!("@{x:.0},{y:.0}"));
        let mut line = format!("[{r}] {tag} \"{name}\"");
        if !href.is_empty() {
            line += &format!(" -- {href}");
        }
        line += &format!(" ({})", extra.join(" "));
        println!("{line}");
    }
    Ok(())
}

pub async fn shot(server: &str, passcode: Option<&str>, tab: Option<u32>, out: Option<&str>) -> Result<()> {
    let out = out.unwrap_or("screenshot.jpg");
    let url = api(server, &format!("/api/screenshot{}", tq(tab)));
    let res = with_auth(http().get(&url), passcode)
        .send()
        .await
        .context("daemon not reachable")?;
    if res.status() == reqwest::StatusCode::UNAUTHORIZED {
        eprintln!("error: unauthorized — wrong or missing passcode (set --passcode / LB_PASSCODE)");
        std::process::exit(1);
    }
    let status = res.status();
    let headers = res.headers().clone();
    let bytes = res.bytes().await?;
    let img = crate::screenshot::validate_response_with_dimensions(status, &bytes)?;
    let mime = img.mime;
    if out == "-" {
        let b64 = base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            &bytes,
        );
        println!("{b64}");
    } else {
        tokio::fs::write(out, &bytes).await?;
        // The daemon captures JPEG (q70); say so, and nudge `.png` names.
        let note = if out.ends_with(".png") && mime == "image/jpeg" {
            " (note: data is JPEG despite the .png name — prefer .jpg)"
        } else {
            ""
        };
        let vp_note = match (
            headers.get("x-lb-viewport-width").and_then(|v| v.to_str().ok()),
            headers.get("x-lb-viewport-height").and_then(|v| v.to_str().ok()),
            headers.get("x-lb-dpr").and_then(|v| v.to_str().ok()),
        ) {
            (Some(vw), Some(vh), Some(dpr)) => format!(", {}x{}px, viewport {vw}x{vh} dpr={dpr}", img.width, img.height),
            _ => format!(", {}x{}px", img.width, img.height),
        };
        println!("saved {out} ({} bytes {mime}{vp_note}) — live view: {}/{note}", bytes.len(), server.trim_end_matches('/'));
    }
    Ok(())
}

pub async fn click(server: &str, passcode: Option<&str>, target: &str, kind: &str, tab: Option<u32>, json: bool) -> Result<()> {
    let v = post(
        server,
        passcode,
        "/api/click",
        serde_json::json!({ "target": target, "kind": kind, "tab": tab }),
    )
    .await?;
    check_ok(&v, json)
}

#[allow(clippy::too_many_arguments)]
pub async fn click_at(
    server: &str,
    passcode: Option<&str>,
    x: f64,
    y: f64,
    coord_space: &str,
    kind: &str,
    tab: Option<u32>,
    json: bool,
) -> Result<()> {
    let v = post(
        server,
        passcode,
        "/api/click-at",
        serde_json::json!({ "x": x, "y": y, "coord_space": coord_space, "kind": kind, "tab": tab }),
    )
    .await?;
    check_ok(&v, json)
}

#[allow(clippy::too_many_arguments)]
pub async fn hover(
    server: &str,
    passcode: Option<&str>,
    x: Option<f64>,
    y: Option<f64>,
    target: Option<&str>,
    tab: Option<u32>,
    json: bool,
) -> Result<()> {
    let mut body = serde_json::json!({ "tab": tab });
    if let Some(t) = target {
        body["target"] = serde_json::Value::String(t.to_string());
    }
    if let Some(x) = x {
        body["x"] = serde_json::json!(x);
    }
    if let Some(y) = y {
        body["y"] = serde_json::json!(y);
    }
    let v = post(server, passcode, "/api/hover", body).await?;
    if json {
        print_json(&v);
        return Ok(());
    }
    if v.get("ok").and_then(|x| x.as_bool()) != Some(true) {
        eprintln!(
            "error: {}",
            v.get("error").and_then(|x| x.as_str()).unwrap_or("hover failed")
        );
        std::process::exit(1);
    }
    Ok(())
}

pub async fn scroll(server: &str, passcode: Option<&str>, dx: f64, dy: f64, tab: Option<u32>, json: bool) -> Result<()> {
    let v = post(
        server,
        passcode,
        "/api/scroll",
        serde_json::json!({ "dx": dx, "dy": dy, "tab": tab }),
    )
    .await?;
    check_ok(&v, json)
}

#[allow(clippy::too_many_arguments)]
pub async fn wheel(
    server: &str,
    passcode: Option<&str>,
    x: f64,
    y: f64,
    dx: f64,
    dy: f64,
    tab: Option<u32>,
    json: bool,
) -> Result<()> {
    let v = post(
        server,
        passcode,
        "/api/wheel",
        serde_json::json!({ "x": x, "y": y, "dx": dx, "dy": dy, "tab": tab }),
    )
    .await?;
    check_ok(&v, json)
}

#[allow(clippy::too_many_arguments)]
pub async fn drag(
    server: &str,
    passcode: Option<&str>,
    x1: f64,
    y1: f64,
    x2: f64,
    y2: f64,
    steps: u32,
    tab: Option<u32>,
    json: bool,
) -> Result<()> {
    let v = post(
        server,
        passcode,
        "/api/drag",
        serde_json::json!({ "x1": x1, "y1": y1, "x2": x2, "y2": y2, "steps": steps, "tab": tab }),
    )
    .await?;
    check_ok(&v, json)
}

pub async fn zoom(server: &str, passcode: Option<&str>, action: &str, tab: Option<u32>, json: bool) -> Result<()> {
    let v = post(
        server,
        passcode,
        "/api/zoom",
        serde_json::json!({ "action": action, "tab": tab }),
    )
    .await?;
    check_ok(&v, json)
}

#[allow(clippy::too_many_arguments)]
pub async fn resize(
    server: &str,
    passcode: Option<&str>,
    width: Option<u32>,
    height: Option<u32>,
    dpr: Option<f64>,
    mobile: Option<bool>,
    preset: Option<&str>,
    reset: bool,
    tab: Option<u32>,
    json: bool,
) -> Result<()> {
    let v = post(
        server,
        passcode,
        "/api/resize",
        serde_json::json!({
            "width": width,
            "height": height,
            "dpr": dpr,
            "mobile": mobile,
            "preset": preset,
            "reset": reset,
            "tab": tab,
        }),
    )
    .await?;
    check_ok(&v, json)
}

#[allow(clippy::too_many_arguments)]
pub async fn type_text(
    server: &str,
    passcode: Option<&str>,
    target: &str,
    text: &str,
    submit: bool,
    append: bool,
    tab: Option<u32>,
    json: bool,
) -> Result<()> {
    let v = post(
        server,
        passcode,
        "/api/type",
        serde_json::json!({ "target": target, "text": text, "submit": submit, "append": append, "tab": tab }),
    )
    .await?;
    check_ok(&v, json)
}

pub async fn press(server: &str, passcode: Option<&str>, key: &str, tab: Option<u32>, json: bool) -> Result<()> {
    let v = post(
        server,
        passcode,
        "/api/press",
        serde_json::json!({ "key": key, "tab": tab }),
    )
    .await?;
    check_ok(&v, json)
}

pub async fn select(server: &str, passcode: Option<&str>, target: &str, value: &str, tab: Option<u32>, json: bool) -> Result<()> {
    let v = post(
        server,
        passcode,
        "/api/select",
        serde_json::json!({ "target": target, "value": value, "tab": tab }),
    )
    .await?;
    check_ok(&v, json)
}

pub async fn insert(server: &str, passcode: Option<&str>, text: &str, tab: Option<u32>, json: bool) -> Result<()> {
    let v = post(
        server,
        passcode,
        "/api/insert",
        serde_json::json!({ "text": text, "tab": tab }),
    )
    .await?;
    check_ok(&v, json)
}

pub async fn eval(server: &str, passcode: Option<&str>, js: &str, tab: Option<u32>, json: bool) -> Result<()> {
    let v = post(
        server,
        passcode,
        "/api/eval",
        serde_json::json!({ "js": js, "tab": tab }),
    )
    .await?;
    if json {
        print_json(&v);
    } else if v.get("ok").and_then(|x| x.as_bool()) == Some(true) {
        match v.get("result") {
            Some(Value::String(s)) => println!("{s}"),
            Some(other) => println!("{}", serde_json::to_string_pretty(other).unwrap()),
            None => println!("ok"),
        }
    } else {
        eprintln!(
            "error: {}",
            v.get("error").and_then(|x| x.as_str()).unwrap_or("eval failed")
        );
        std::process::exit(1);
    }
    Ok(())
}

pub async fn nav(server: &str, passcode: Option<&str>, action: &str, tab: Option<u32>, json: bool) -> Result<()> {
    let v = post(
        server,
        passcode,
        "/api/nav",
        serde_json::json!({ "action": action, "tab": tab }),
    )
    .await?;
    check_ok(&v, json)
}

pub async fn handover(server: &str, passcode: Option<&str>, message: &str, tab: Option<u32>, timeout: u64) -> Result<()> {
    let v = post(
        server,
        passcode,
        "/api/handover/request",
        serde_json::json!({ "message": message, "tab": tab }),
    )
    .await?;
    if v.get("ok").and_then(|x| x.as_bool()) != Some(true) {
        eprintln!(
            "error: {}",
            v.get("error").and_then(|x| x.as_str()).unwrap_or("handover failed")
        );
        std::process::exit(1);
    }
    let tab_id = v.get("tab").and_then(|x| x.as_u64()).unwrap_or(0);
    println!("=== human handover requested (tab {tab_id}) ===");
    println!("message: {message}");
    println!("live monitor: {}/", server.trim_end_matches('/'));
    println!("The browser window is now yours: log in / solve captcha / 2FA,");
    println!("then click \"Done — return to agent\" in the web UI.");
    println!("Waiting for human... (timeout: {})", if timeout == 0 { "none".into() } else { format!("{timeout}s") });

    let client = http();
    let start = std::time::Instant::now();
    loop {
        if timeout != 0 && start.elapsed().as_secs() >= timeout {
            eprintln!("handover timed out after {timeout}s — still waiting? re-run with larger --timeout");
            std::process::exit(2);
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
        let req = with_auth(
            client.get(api(server, &format!("/api/handover?tab={tab_id}"))),
            passcode,
        );
        let st: Value = req.send().await?.json().await?;
        let active = st.get("active").and_then(|x| x.as_bool()).unwrap_or(true);
        if !active {
            println!("=== human done — resuming agent control ===");
            let _ = status(server, passcode, Some(tab_id as u32), false).await;
            return Ok(());
        }
    }
}

pub async fn handover_done(server: &str, passcode: Option<&str>, tab: Option<u32>, json: bool) -> Result<()> {
    let v = post(
        server,
        passcode,
        &format!("/api/handover/done{}", tq(tab)),
        serde_json::json!({}),
    )
    .await?;
    check_ok(&v, json)
}

pub async fn claim(server: &str, passcode: Option<&str>, message: &str, tab: Option<u32>, json: bool) -> Result<()> {
    let v = post(
        server,
        passcode,
        "/api/handover/claim",
        serde_json::json!({ "message": message, "tab": tab }),
    )
    .await?;
    check_ok(&v, json)
}

pub async fn wait(
    server: &str,
    passcode: Option<&str>,
    target: &str,
    timeout: u64,
    gone: bool,
    tab: Option<u32>,
    json: bool,
) -> Result<()> {
    let v = post(
        server,
        passcode,
        "/api/wait",
        serde_json::json!({ "target": target, "timeout": timeout, "gone": gone, "tab": tab }),
    )
    .await?;
    check_ok(&v, json)
}

pub async fn content(
    server: &str,
    passcode: Option<&str>,
    kind: &str,
    target: Option<&str>,
    max: usize,
    tab: Option<u32>,
    json: bool,
) -> Result<()> {
    let v = post(
        server,
        passcode,
        &format!("/api/{kind}"),
        serde_json::json!({ "target": target, "max": max, "tab": tab }),
    )
    .await?;
    if v.get("ok").and_then(|x| x.as_bool()) == Some(true) {
        if json {
            print_json(&v);
        } else {
            if let Some(c) = v.get("content").and_then(|x| x.as_str()) {
                println!("{c}");
            }
            if v.get("truncated").and_then(|x| x.as_bool()) == Some(true) {
                let n = v.get("chars").and_then(|x| x.as_u64()).unwrap_or(0);
                eprintln!("(truncated to {max} of {n} chars — raise --max or use --json)");
            }
        }
        Ok(())
    } else {
        let e = v.get("error").and_then(|x| x.as_str()).unwrap_or("failed");
        if json {
            print_json(&v);
        } else {
            eprintln!("error: {e}");
        }
        std::process::exit(1);
    }
}

pub async fn upload(
    server: &str,
    passcode: Option<&str>,
    target: &str,
    file: &str,
    tab: Option<u32>,
    json: bool,
) -> Result<()> {
    let v = post(
        server,
        passcode,
        "/api/upload",
        serde_json::json!({ "target": target, "file": file, "tab": tab }),
    )
    .await?;
    check_ok(&v, json)
}

pub async fn downloads(server: &str, passcode: Option<&str>, json: bool) -> Result<()> {
    let v = get(server, passcode, "/api/downloads").await?;
    if v.get("ok").and_then(|x| x.as_bool()) != Some(true) {
        eprintln!(
            "error: {}",
            v.get("error").and_then(|x| x.as_str()).unwrap_or("downloads failed")
        );
        std::process::exit(1);
    }
    if json {
        print_json(&v);
        return Ok(());
    }
    let dir = v.get("dir").and_then(|x| x.as_str()).unwrap_or("");
    println!("download dir: {dir}");
    let empty = vec![];
    let files = v.get("files").and_then(|x| x.as_array()).unwrap_or(&empty);
    if files.is_empty() {
        println!("(no downloads yet)");
        return Ok(());
    }
    for f in files {
        let name = f.get("name").and_then(|x| x.as_str()).unwrap_or("?");
        let bytes = f.get("bytes").and_then(|x| x.as_u64()).unwrap_or(0);
        println!("{bytes:>10}  {name}");
    }
    Ok(())
}

pub async fn log(server: &str, passcode: Option<&str>, json: bool) -> Result<()> {    let v = get(server, passcode, "/api/log").await?;
    if json {
        print_json(&v);
    } else {
        if v.get("ok").and_then(|x| x.as_bool()) != Some(true) {
            eprintln!(
                "error: {}",
                v.get("error").and_then(|x| x.as_str()).unwrap_or("log failed")
            );
            std::process::exit(1);
        }
        let empty = vec![];
        let entries = v.get("entries").and_then(|x| x.as_array()).unwrap_or(&empty);
        for e in entries.iter().rev().take(30).rev() {
            let t = e.get("at").and_then(|x| x.as_str()).unwrap_or("");
            let a = e.get("action").and_then(|x| x.as_str()).unwrap_or("");
            let d = e.get("detail").and_then(|x| x.as_str()).unwrap_or("");
            println!("{t} {a} {d}");
        }
    }
    Ok(())
}

pub async fn list(server: &str, passcode: Option<&str>, json: bool) -> Result<()> {
    let v = get(server, passcode, "/api/tabs").await?;
    if json {
        print_json(&v);
        return Ok(());
    }
    if v.get("ok").and_then(|x| x.as_bool()) != Some(true) {
        eprintln!(
            "error: {}",
            v.get("error").and_then(|x| x.as_str()).unwrap_or("failed to list tabs")
        );
        std::process::exit(1);
    }
    let empty = vec![];
    let tabs = v.get("tabs").and_then(|x| x.as_array()).unwrap_or(&empty);
    if tabs.is_empty() {
        println!("(no tabs)");
        return Ok(());
    }
    for t in tabs {
        let id = t.get("id").and_then(|x| x.as_u64()).unwrap_or(0);
        let mark = if t.get("active").and_then(|x| x.as_bool()).unwrap_or(false) {
            "*"
        } else {
            " "
        };
        let ho = if t.get("handover").and_then(|x| x.as_bool()).unwrap_or(false) {
            " [needs human]"
        } else {
            ""
        };
        let title = t.get("title").and_then(|x| x.as_str()).unwrap_or("(no title)");
        let url = t.get("url").and_then(|x| x.as_str()).unwrap_or("");
        let owner = match t.get("owner").and_then(|x| x.as_str()) {
            Some(o) => format!(" [agent: {o}]"),
            None => String::new(),
        };
        println!("{mark} tab {id}: {title} <{url}>{owner}{ho}");
    }
    Ok(())
}

pub async fn recover(server: &str, passcode: Option<&str>, check: bool, json: bool) -> Result<()> {
    if check {
        let v = get(server, passcode, "/api/health").await?;
        if json {
            print_json(&v);
            return Ok(());
        }
        let probe = v.get("probe").and_then(|x| x.as_str()).unwrap_or("?");
        let alive = v.get("browser_alive").and_then(|x| x.as_bool()).unwrap_or(false);
        if alive && probe == "ok" {
            println!("browser is healthy");
        } else {
            eprintln!("browser is dead (probe: {probe}) — run `lb recover` to relaunch it in place (open tabs reset)");
            std::process::exit(1);
        }
        return Ok(());
    }
    let v = post(server, passcode, "/api/recover", serde_json::json!({})).await?;
    check_ok(&v, json)
}

pub async fn close(server: &str, passcode: Option<&str>, tab: Option<u32>, json: bool) -> Result<()> {
    // Resolve the tab first so the warning names what is being closed.
    let tabs = get(server, passcode, "/api/tabs").await?;
    if tabs.get("ok").and_then(|x| x.as_bool()) != Some(true) {
        eprintln!(
            "error: {}",
            tabs.get("error").and_then(|x| x.as_str()).unwrap_or("failed to list tabs")
        );
        std::process::exit(1);
    }
    let empty = vec![];
    let list = tabs.get("tabs").and_then(|x| x.as_array()).unwrap_or(&empty);
    let id = match tab {
        Some(id) => id as u64,
        None => list
            .iter()
            .find(|t| t.get("active").and_then(|x| x.as_bool()).unwrap_or(false))
            .and_then(|t| t.get("id").and_then(|x| x.as_u64()))
            .unwrap_or(0),
    };
    let describe = list
        .iter()
        .find(|t| t.get("id").and_then(|x| x.as_u64()) == Some(id))
        .map(|t| {
            format!(
                "{} <{}>",
                t.get("title").and_then(|x| x.as_str()).unwrap_or("?"),
                t.get("url").and_then(|x| x.as_str()).unwrap_or("?")
            )
        })
        .unwrap_or_else(|| format!("tab {id}"));
    if !json {
        eprintln!("warning: closing {describe} — agents won't be able to control it afterwards.");
    }
    let v = post(
        server,
        passcode,
        "/api/close",
        serde_json::json!({ "tab": tab }),
    )
    .await?;
    check_ok(&v, json)
}

pub async fn use_tab(server: &str, passcode: Option<&str>, tab: u32, json: bool) -> Result<()> {
    let v = post(
        server,
        passcode,
        "/api/activate",
        serde_json::json!({ "tab": tab }),
    )
    .await?;
    check_ok(&v, json)
}

pub async fn link(server: &str, passcode: Option<&str>, tab: Option<u32>, json: bool) -> Result<()> {
    let v = get(server, passcode, "/api/tabs").await?;
    if v.get("ok").and_then(|x| x.as_bool()) != Some(true) {
        eprintln!(
            "error: {}",
            v.get("error").and_then(|x| x.as_str()).unwrap_or("failed to list tabs")
        );
        std::process::exit(1);
    }
    let empty = vec![];
    let tabs = v.get("tabs").and_then(|x| x.as_array()).unwrap_or(&empty);
    if tabs.is_empty() {
        eprintln!("error: no tabs — is `lb web` running?");
        std::process::exit(1);
    }
    let id = match tab {
        Some(id) => id as u64,
        None => tabs
            .iter()
            .find(|t| t.get("active").and_then(|x| x.as_bool()).unwrap_or(false))
            .and_then(|t| t.get("id").and_then(|x| x.as_u64()))
            .unwrap_or(0),
    };
    let found = tabs.iter().find(|t| t.get("id").and_then(|x| x.as_u64()) == Some(id));
    match found {
        Some(t) => {
            let url = format!("{}/?tabId={id}", server.trim_end_matches('/'));
            if json {
                let mut out = serde_json::json!({ "ok": true, "tab": id, "url": url });
                if let Some(title) = t.get("title").and_then(|x| x.as_str()) {
                    out["title"] = serde_json::Value::String(title.to_string());
                }
                if let Some(page_url) = t.get("url").and_then(|x| x.as_str()) {
                    out["page_url"] = serde_json::Value::String(page_url.to_string());
                }
                print_json(&out);
            } else {
                println!("{url}");
            }
            Ok(())
        }
        None => {
            let msg = format!("no such tab: {id} — see `lb list`");
            if json {
                print_json(&serde_json::json!({ "ok": false, "error": msg }));
            } else {
                eprintln!("error: {msg}");
            }
            std::process::exit(1);
        }
    }
}

pub async fn record_start(server: &str, passcode: Option<&str>, fps: f64, max_secs: u64, tab: Option<u32>, json: bool) -> Result<()> {
    let body = serde_json::json!({ "action": "start", "fps": fps, "max_secs": max_secs, "tab": tab });
    let v = post(server, passcode, "/api/record", body).await?;
    check_ok(&v, json)
}

pub async fn record_status(server: &str, passcode: Option<&str>, json: bool) -> Result<()> {
    let v = post(server, passcode, "/api/record", serde_json::json!({ "action": "status" })).await?;
    check_ok(&v, json)
}

/// Stop, then download the encoded video from the daemon (which may be remote).
pub async fn record_stop(server: &str, passcode: Option<&str>, out: Option<&str>, tab: Option<u32>, json: bool) -> Result<()> {
    let v = post(server, passcode, "/api/record", serde_json::json!({ "action": "stop", "tab": tab })).await?;
    if v.get("ok").and_then(|x| x.as_bool()) != Some(true) {
        return check_ok(&v, json);
    }
    let ext = v.get("ext").and_then(|x| x.as_str()).unwrap_or("mp4");
    let default_out = format!("recording.{ext}");
    let out = out.unwrap_or(&default_out);
    let dl = v.get("download").and_then(|x| x.as_str()).context("daemon gave no download link")?;
    let res = with_auth(http().get(api(server, dl)), passcode).send().await.context("daemon not reachable")?;
    if !res.status().is_success() {
        anyhow::bail!("download failed: HTTP {}", res.status());
    }
    let bytes = res.bytes().await?;
    tokio::fs::write(out, &bytes).await.with_context(|| format!("writing {out}"))?;
    if json {
        let mut v = v;
        v["saved"] = serde_json::json!(out);
        print_json(&v);
    } else {
        let secs = v.get("seconds").and_then(|x| x.as_f64()).unwrap_or(0.0);
        let enc = v.get("encoder").and_then(|x| x.as_str()).unwrap_or("");
        println!("saved {out} ({} bytes, {secs:.1}s) [{enc}]", bytes.len());
        if let Some(r) = v.get("stopped_early").and_then(|x| x.as_str()) {
            println!("note: recording had stopped early: {r}");
        }
    }
    Ok(())
}
