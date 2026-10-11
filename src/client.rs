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
    let v: Value = res.json().await.context("invalid daemon response")?;
    Ok(v)
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
    let v: Value = res.json().await.context("invalid daemon response")?;
    Ok(v)
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

pub async fn open(server: &str, passcode: Option<&str>, url: &str, tab: Option<u32>, json: bool) -> Result<()> {
    let v = post(
        server,
        passcode,
        "/api/open",
        serde_json::json!({ "url": url, "tab": tab }),
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
    }
    Ok(())
}

pub async fn snapshot(server: &str, passcode: Option<&str>, tab: Option<u32>, json: bool, full: bool) -> Result<()> {
    let v = get(server, passcode, &format!("/api/snapshot{}", tq(tab))).await?;
    if json || full {
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
        println!("(no interactive elements found — page may still be loading; try `lb snapshot` again or `lb shot`)");
    }
    for el in els {
        let r = el.get("ref").and_then(|x| x.as_u64()).unwrap_or(0);
        let tag = el.get("tag").and_then(|x| x.as_str()).unwrap_or("?");
        let name = el.get("name").and_then(|x| x.as_str()).unwrap_or("");
        let href = el.get("href").and_then(|x| x.as_str()).unwrap_or("");
        if href.is_empty() {
            println!("[{r}] {tag} \"{name}\"");
        } else {
            println!("[{r}] {tag} \"{name}\" -- {href}");
        }
    }
    Ok(())
}

pub async fn shot(server: &str, passcode: Option<&str>, tab: Option<u32>, out: Option<&str>) -> Result<()> {
    let out = out.unwrap_or("screenshot.png");
    let url = api(server, &format!("/api/screenshot{}", tq(tab)));
    let res = with_auth(http().get(&url), passcode)
        .send()
        .await
        .context("daemon not reachable")?;
    if res.status() == reqwest::StatusCode::UNAUTHORIZED {
        eprintln!("error: unauthorized — wrong or missing passcode (set --passcode / LB_PASSCODE)");
        std::process::exit(1);
    }
    let bytes = res.bytes().await?;
    if out == "-" {
        let b64 = base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            &bytes,
        );
        println!("{b64}");
    } else {
        // Guard against saving a JSON error body as a screenshot.
        if bytes.starts_with(b"{") {
            if let Ok(v) = serde_json::from_slice::<Value>(&bytes) {
                if v.get("ok").and_then(|x| x.as_bool()) == Some(false) {
                    eprintln!(
                        "error: {}",
                        v.get("error").and_then(|x| x.as_str()).unwrap_or("screenshot failed")
                    );
                    std::process::exit(1);
                }
            }
        }
        tokio::fs::write(out, &bytes).await?;
        println!("saved {} ({} bytes) — live view: {}/", out, bytes.len(), server.trim_end_matches('/'));
    }
    Ok(())
}

pub async fn click(server: &str, passcode: Option<&str>, target: &str, tab: Option<u32>, json: bool) -> Result<()> {
    let v = post(
        server,
        passcode,
        "/api/click",
        serde_json::json!({ "target": target, "tab": tab }),
    )
    .await?;
    check_ok(&v, json)
}

pub async fn click_at(server: &str, passcode: Option<&str>, x: f64, y: f64, tab: Option<u32>, json: bool) -> Result<()> {
    let v = post(
        server,
        passcode,
        "/api/click-at",
        serde_json::json!({ "x": x, "y": y, "tab": tab }),
    )
    .await?;
    check_ok(&v, json)
}

pub async fn hover(server: &str, passcode: Option<&str>, x: f64, y: f64, tab: Option<u32>, json: bool) -> Result<()> {
    let v = post(
        server,
        passcode,
        "/api/hover",
        serde_json::json!({ "x": x, "y": y, "tab": tab }),
    )
    .await?;
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

pub async fn type_text(
    server: &str,
    passcode: Option<&str>,
    target: &str,
    text: &str,
    submit: bool,
    tab: Option<u32>,
    json: bool,
) -> Result<()> {
    let v = post(
        server,
        passcode,
        "/api/type",
        serde_json::json!({ "target": target, "text": text, "submit": submit, "tab": tab }),
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

pub async fn log(server: &str, passcode: Option<&str>, json: bool) -> Result<()> {
    let v = get(server, passcode, "/api/log").await?;
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
        println!("{mark} tab {id}: {title} <{url}>{ho}");
    }
    Ok(())
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
