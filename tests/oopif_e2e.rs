//! Real headless Chrome E2E for cross-origin (out-of-process) iframes.
//!
//! The parent page is served from `http://localhost:<A>` and embeds an
//! iframe from `http://127.0.0.1:<B>`. Chrome isolates by *site* (scheme +
//! host; the port is ignored), so `localhost` vs `127.0.0.1` gives a genuine
//! OOPIF with its own renderer process and CDP target. The test asserts that
//! via the snapshot's `frames` list, then drives the frame's button, input
//! and select with ordinary snapshot refs through `/api/click`, `/api/type`,
//! `/api/select`, `/api/wait` and the MCP `batch` tool (`lb mcp` over stdio).
//! Frame state is reported to the parent via `postMessage` so it can be
//! read with a main-frame `eval`.
//!
//! Run with:
//! `cargo test --test oopif_e2e -- --ignored --nocapture`

use axum::{response::Html, routing::get, Router};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const FRAME_HTML: &str = r#"<!doctype html>
<html><head><meta charset="utf-8"><title>frame</title>
<style> body { margin: 0; padding: 10px; font: 16px sans-serif; } * { margin: 4px; } </style></head>
<body>
  <button id="btn">Frame Button</button>
  <input id="name" placeholder="Your name">
  <select id="color">
    <option value="">-- pick --</option>
    <option value="red">Red</option>
    <option value="blue">Blue</option>
  </select>
  <button id="reveal">Show Later</button>
  <div id="slot"></div>
  <script>
    const state = { clicks: 0, name: '', color: '', inputEvents: 0, changeEvents: 0, origin: location.origin };
    const report = () => parent.postMessage({ lbFrame: JSON.parse(JSON.stringify(state)) }, '*');
    document.getElementById('btn').addEventListener('click', () => { state.clicks++; report(); });
    const name = document.getElementById('name');
    name.addEventListener('input', () => { state.name = name.value; state.inputEvents++; report(); });
    const color = document.getElementById('color');
    color.addEventListener('change', () => { state.color = color.value; state.changeEvents++; report(); });
    document.getElementById('reveal').addEventListener('click', () => {
      setTimeout(() => {
        const d = document.createElement('div');
        d.id = 'late'; d.textContent = 'late content';
        document.getElementById('slot').appendChild(d);
      }, 600);
    });
    report();
  </script>
</body></html>
"#;

fn parent_html(frame_url: &str) -> String {
    format!(
        r#"<!doctype html>
<html><head><meta charset="utf-8"><title>OOPIF parent</title>
<style>
  body {{ margin: 0; font: 16px sans-serif; }}
  #spacer {{ height: 1100px; background: #eef; }}
  iframe {{ margin-left: 37px; border: 6px solid #333; padding: 9px; width: 520px; height: 320px; }}
</style></head>
<body>
  <button id="top">Parent Button</button>
  <div id="spacer"></div>
  <iframe id="xo" src="{frame_url}"></iframe>
  <script>
    window.__frame = null;
    window.addEventListener('message', (e) => {{ if (e.data && e.data.lbFrame) window.__frame = e.data.lbFrame; }});
  </script>
</body></html>
"#
    )
}

struct DaemonGuard {
    child: Child,
    profile_dir: std::path::PathBuf,
}

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.profile_dir);
    }
}

fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    listener.local_addr().unwrap().port()
}

async fn wait_for_health(http: &reqwest::Client, base: &str) {
    let deadline = Instant::now() + Duration::from_secs(45);
    while Instant::now() < deadline {
        if let Ok(res) = http.get(format!("{base}/api/health")).send().await {
            if let Ok(v) = res.json::<Value>().await {
                if v.get("browser_alive").and_then(|b| b.as_bool()) == Some(true) {
                    return;
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    panic!("lb daemon at {base} did not become healthy within 45s");
}

async fn post(http: &reqwest::Client, base: &str, path: &str, body: Value) -> Value {
    http.post(format!("{base}{path}")).json(&body).send().await.unwrap().json().await.unwrap()
}

async fn frame_state(http: &reqwest::Client, base: &str) -> Value {
    let v = post(http, base, "/api/eval", json!({ "js": "window.__frame" })).await;
    assert_eq!(v["ok"], true, "eval failed: {v}");
    v["result"].clone()
}

/// Poll the parent-side mirror of the iframe state until `pred` holds.
async fn wait_state<F: Fn(&Value) -> bool>(http: &reqwest::Client, base: &str, what: &str, pred: F) -> Value {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let st = frame_state(http, base).await;
        if pred(&st) {
            return st;
        }
        if Instant::now() > deadline {
            panic!("frame state never satisfied `{what}`: {st}");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn find_ref(els: &[Value], pred: impl Fn(&Value) -> bool, what: &str) -> u64 {
    els.iter()
        .find(|e| pred(e))
        .and_then(|e| e["ref"].as_u64())
        .unwrap_or_else(|| panic!("no element for {what} in snapshot: {els:#?}"))
}

struct Refs {
    top: u64,
    button: u64,
    input: u64,
    select: u64,
    reveal: u64,
}

fn refs_from(snap: &Value) -> Refs {
    let els = snap["elements"].as_array().unwrap();
    let xo = |e: &Value| e["frame"].as_str().unwrap_or("").starts_with("oopif[");
    Refs {
        top: find_ref(els, |e| e["name"] == "Parent Button" && !xo(e), "parent button"),
        button: find_ref(els, |e| e["name"] == "Frame Button" && xo(e), "frame button"),
        input: find_ref(els, |e| e["tag"] == "input" && e["placeholder"] == "Your name" && xo(e), "frame input"),
        select: find_ref(els, |e| e["tag"] == "select" && xo(e), "frame select"),
        reveal: find_ref(els, |e| e["name"] == "Show Later" && xo(e), "frame reveal button"),
    }
}

/// Minimal stdio JSON-RPC client for `lb mcp`.
struct Mcp {
    child: Child,
    out: BufReader<std::process::ChildStdout>,
    next_id: u64,
}

impl Mcp {
    fn spawn(bin: &str, base: &str) -> Self {
        let mut child = Command::new(bin)
            .args(["--server", base, "mcp", "--session", "oopif-e2e"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn lb mcp");
        let out = BufReader::new(child.stdout.take().unwrap());
        let mut m = Mcp { child, out, next_id: 1 };
        m.request("initialize", json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": { "name": "oopif-e2e", "version": "0" }
        }));
        m.send(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));
        m
    }

    fn send(&mut self, msg: Value) {
        let stdin = self.child.stdin.as_mut().unwrap();
        writeln!(stdin, "{msg}").unwrap();
        stdin.flush().unwrap();
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        loop {
            let mut line = String::new();
            let n = self.out.read_line(&mut line).unwrap();
            assert!(n > 0, "lb mcp closed stdout");
            let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
            if v["id"] == id {
                assert!(v.get("error").is_none(), "{method} error: {v}");
                return v["result"].clone();
            }
        }
    }

    /// Call a tool; returns (is_error, text).
    fn tool(&mut self, name: &str, args: Value) -> (bool, String) {
        let r = self.request("tools/call", json!({ "name": name, "arguments": args }));
        let text = r["content"]
            .as_array()
            .map(|c| c.iter().filter_map(|b| b["text"].as_str()).collect::<Vec<_>>().join("\n"))
            .unwrap_or_default();
        (r["isError"] == true, text)
    }
}

impl Drop for Mcp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires local Chrome/Chromium binary"]
async fn cross_origin_iframe_refs_snapshot_click_type_select_wait_batch() {
    // 1. Frame origin (127.0.0.1:B) and parent origin (localhost:A).
    let frame_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let frame_port = frame_listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(frame_listener, Router::new().route("/", get(|| async { Html(FRAME_HTML) }))).await.unwrap();
    });
    let frame_url = format!("http://127.0.0.1:{frame_port}/");
    let parent = parent_html(&frame_url);
    let parent_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let parent_port = parent_listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let app = Router::new().route("/", get(move || async move { Html(parent.clone()) }));
        axum::serve(parent_listener, app).await.unwrap();
    });
    let parent_url = format!("http://localhost:{parent_port}/");

    // 2. Real `lb web` daemon with an isolated profile.
    let lb_port = free_port();
    let profile_dir = std::env::temp_dir().join(format!("lb-e2e-oopif-{lb_port}"));
    let _ = std::fs::create_dir_all(&profile_dir);
    let bin = env!("CARGO_BIN_EXE_lb");
    let child = Command::new(bin)
        .args([
            "web", "--foreground", "--headless", "--no-open", "--no-sandbox",
            "--port", &lb_port.to_string(),
            "--profile", profile_dir.to_str().unwrap(),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn lb web");
    let _guard = DaemonGuard { child, profile_dir };
    let base = format!("http://127.0.0.1:{lb_port}");
    let http = reqwest::Client::builder().timeout(Duration::from_secs(60)).build().unwrap();
    wait_for_health(&http, &base).await;

    let open = post(&http, &base, "/api/open", json!({ "url": parent_url })).await;
    assert_eq!(open["ok"], true, "open failed: {open}");
    wait_state(&http, &base, "iframe loaded", |s| s.is_object()).await;

    // 3. Snapshot: the cross-origin frame's controls get normal refs.
    let deadline = Instant::now() + Duration::from_secs(15);
    let snap = loop {
        let snap: Value = http.get(format!("{base}/api/snapshot")).send().await.unwrap().json().await.unwrap();
        assert_eq!(snap["ok"], true, "snapshot failed: {snap}");
        let has = snap["elements"].as_array().unwrap().iter().any(|e| e["name"] == "Frame Button");
        if has {
            break snap;
        }
        assert!(Instant::now() < deadline, "OOPIF elements never appeared: {snap:#}");
        tokio::time::sleep(Duration::from_millis(250)).await;
    };
    println!("snapshot: {}", serde_json::to_string_pretty(&snap).unwrap());
    let frames = snap["frames"].as_array().expect("snapshot lists attached OOPIF frames");
    assert_eq!(frames.len(), 1, "exactly one OOPIF: {frames:?}");
    assert!(frames[0]["url"].as_str().unwrap().starts_with(&frame_url), "frame url: {frames:?}");
    let r = refs_from(&snap);
    assert_eq!(r.top, 1, "main-frame refs come first");
    assert!(r.button > r.top && r.input > r.top && r.select > r.top);
    // Refs are one dense sequence with no duplicates (same-origin dedupe intact).
    let mut all: Vec<u64> = snap["elements"].as_array().unwrap().iter().map(|e| e["ref"].as_u64().unwrap()).collect();
    let n = all.len();
    all.sort();
    all.dedup();
    assert_eq!(all.len(), n, "duplicate refs");
    assert_eq!(all, (1..=n as u64).collect::<Vec<_>>());
    // Coordinates are translated into top-level viewport space: x includes
    // the iframe's margin (37) + border (6) + padding (9) = 52px.
    let btn = snap["elements"].as_array().unwrap().iter().find(|e| e["ref"] == r.button).unwrap();
    assert!(btn["x"].as_f64().unwrap() > 52.0, "button x not offset: {btn}");
    let off = frames[0]["offset"].as_array().unwrap();
    assert!((off[0].as_f64().unwrap() - 52.0).abs() < 1.0, "iframe content-box x offset: {off:?}");

    // 4. click (iframe is below the fold: must scroll parent + frame).
    let v = post(&http, &base, "/api/click", json!({ "target": r.button.to_string() })).await;
    assert_eq!(v["ok"], true, "click failed: {v}");
    println!("click: {v}");
    wait_state(&http, &base, "clicks == 1", |s| s["clicks"] == 1).await;

    // 5. type_text (clears, then trusted insert -> input events).
    let v = post(&http, &base, "/api/type", json!({ "target": r.input.to_string(), "text": "Ada Lovelace" })).await;
    assert_eq!(v["ok"], true, "type failed: {v}");
    println!("type: {v}");
    let st = wait_state(&http, &base, "name typed", |s| s["name"] == "Ada Lovelace").await;
    assert!(st["inputEvents"].as_u64().unwrap() >= 1);
    // Re-typing replaces (default clears first); append keeps.
    let v = post(&http, &base, "/api/type", json!({ "target": r.input.to_string(), "text": "Grace" })).await;
    assert_eq!(v["ok"], true, "retype failed: {v}");
    wait_state(&http, &base, "name replaced", |s| s["name"] == "Grace").await;
    let v = post(&http, &base, "/api/type", json!({ "target": r.input.to_string(), "text": " Hopper", "append": true })).await;
    assert_eq!(v["ok"], true, "append failed: {v}");
    wait_state(&http, &base, "name appended", |s| s["name"] == "Grace Hopper").await;

    // 6. select by visible text, no native popup.
    let v = post(&http, &base, "/api/select", json!({ "target": r.select.to_string(), "value": "Blue" })).await;
    assert_eq!(v["ok"], true, "select failed: {v}");
    println!("select: {v}");
    wait_state(&http, &base, "color blue", |s| s["color"] == "blue").await;

    // 7. wait: ref present, delayed css selector inside the OOPIF, gone.
    let v = post(&http, &base, "/api/wait", json!({ "target": r.button.to_string(), "timeout": 5 })).await;
    assert_eq!(v["ok"], true, "wait ref failed: {v}");
    let v = post(&http, &base, "/api/click", json!({ "target": r.reveal.to_string() })).await;
    assert_eq!(v["ok"], true, "reveal click failed: {v}");
    let v = post(&http, &base, "/api/wait", json!({ "target": "css=#late", "timeout": 8 })).await;
    assert_eq!(v["ok"], true, "wait css in OOPIF failed: {v}");
    println!("wait: {v}");
    let v = post(&http, &base, "/api/wait", json!({ "target": "css=#never", "timeout": 2, "gone": true })).await;
    assert_eq!(v["ok"], true, "wait gone failed: {v}");
    let v = post(&http, &base, "/api/wait", json!({ "target": "css=#never", "timeout": 1 })).await;
    assert_eq!(v["ok"], false, "wait for missing element must time out: {v}");

    // Main-frame refs still work alongside OOPIF refs.
    let v = post(&http, &base, "/api/click", json!({ "target": r.top.to_string() })).await;
    assert_eq!(v["ok"], true, "main-frame click failed: {v}");

    // 8. batch via the real MCP server (its own session tab).
    let mut mcp = Mcp::spawn(bin, &base);
    let (e, t) = mcp.tool("open", json!({ "url": parent_url }));
    assert!(!e, "mcp open: {t}");
    let deadline = Instant::now() + Duration::from_secs(15);
    let msnap: Value = loop {
        let (e, t) = mcp.tool("snapshot", json!({}));
        assert!(!e, "mcp snapshot: {t}");
        let v: Value = serde_json::from_str(&t).unwrap();
        if v["elements"].as_array().unwrap().iter().any(|e| e["name"] == "Frame Button") {
            break v;
        }
        assert!(Instant::now() < deadline, "mcp snapshot never saw OOPIF: {v}");
        std::thread::sleep(Duration::from_millis(250));
    };
    let mr = refs_from(&msnap);
    let (e, t) = mcp.tool("batch", json!({
        "actions": [
            { "action": "wait", "target": mr.input.to_string(), "timeout": 5 },
            { "action": "type_text", "target": mr.input.to_string(), "text": "Batch Name" },
            { "action": "select", "target": mr.select.to_string(), "value": "red" },
            { "action": "click", "target": mr.button.to_string() },
            { "action": "click", "target": mr.button.to_string() },
            { "action": "click", "target": mr.reveal.to_string() },
            { "action": "wait", "target": "css=#late", "timeout": 8 },
            { "action": "eval", "js": "JSON.stringify(window.__frame)" }
        ],
        "stop_on_error": true
    }));
    println!("batch: {t}");
    assert!(!e, "batch failed: {t}");
    let (e, t) = mcp.tool("eval", json!({ "js": "window.__frame" }));
    assert!(!e, "mcp eval: {t}");
    let v: Value = serde_json::from_str(&t).unwrap();
    let st = &v["result"];
    assert_eq!(st["name"], "Batch Name", "batch type: {v}");
    assert_eq!(st["color"], "red", "batch select: {v}");
    assert_eq!(st["clicks"], 2, "batch clicks: {v}");
    assert_eq!(st["origin"], frame_url.trim_end_matches('/'));
}
