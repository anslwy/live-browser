//! Real headless Chrome E2E verification for screenshot-to-click coordinate mapping.
//!
//! Spins up a local test page with distinctly colored buttons, launches a real
//! `lb web --foreground --headless` daemon on an ephemeral port, captures real
//! JPEG screenshots, locates each button's pixel centroid directly from the
//! decoded screenshot bytes, clicks via `/api/click-at` (`image`, `normalized`,
//! and `css` coord spaces) across desktop (dpr=1), mobile (dpr=3), and zoomed
//! tablet (dpr=2) viewports, and asserts both the `hit` element summary and the
//! DOM click handler firing.
//!
//! Run with:
//! `cargo test --test click_at_e2e -- --ignored --nocapture`

use axum::{response::Html, routing::get, Router};
use serde_json::Value;
use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const TEST_HTML: &str = r#"<!doctype html>
<html>
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1.0">
  <title>Click-At Coords E2E</title>
  <style>
    * { box-sizing: border-box; margin: 0; padding: 0; }
    html, body { width: 100%; height: 100%; background: #ffffff; overflow: hidden; }
    .grid {
      display: grid;
      grid-template-columns: 1fr 1fr;
      grid-template-rows: 1fr 1fr;
      gap: 24px;
      padding: 24px;
      width: 100vw;
      height: 100vh;
    }
    button {
      border: none;
      width: 100%;
      height: 100%;
      font-size: 18px;
      font-weight: bold;
      color: rgba(255, 255, 255, 0.01);
      cursor: pointer;
    }
    #btn-red    { background: #ff0000; }
    #btn-green  { background: #00ff00; }
    #btn-blue   { background: #0000ff; }
    #btn-yellow { background: #ffff00; }
  </style>
</head>
<body>
  <div class="grid">
    <button id="btn-red" aria-label="Red Target">Red</button>
    <button id="btn-green" aria-label="Green Target">Green</button>
    <button id="btn-blue" aria-label="Blue Target">Blue</button>
    <button id="btn-yellow" aria-label="Yellow Target">Yellow</button>
  </div>
  <script>
    window.__clicks = [];
    for (const id of ['btn-red', 'btn-green', 'btn-blue', 'btn-yellow']) {
      document.getElementById(id).addEventListener('click', (e) => {
        window.__clicks.push({ id, clientX: e.clientX, clientY: e.clientY });
      });
    }
  </script>
</body>
</html>
"#;

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

/// Find the `(x, y)` pixel centroid of all pixels matching `pred` in an RGB image.
fn find_color_centroid<F>(img: &image::RgbImage, pred: F) -> (f64, f64, usize)
where
    F: Fn(u8, u8, u8) -> bool,
{
    let mut sum_x = 0.0f64;
    let mut sum_y = 0.0f64;
    let mut count = 0usize;
    for (x, y, px) in img.enumerate_pixels() {
        let [r, g, b] = px.0;
        if pred(r, g, b) {
            sum_x += x as f64;
            sum_y += y as f64;
            count += 1;
        }
    }
    assert!(count > 100, "expected >100 matching pixels, found {count}");
    (sum_x / count as f64, sum_y / count as f64, count)
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

async fn get_clicks(http: &reqwest::Client, base: &str) -> Vec<String> {
    let v: Value = http
        .post(format!("{base}/api/eval"))
        .json(&serde_json::json!({ "js": "window.__clicks.map(c => c.id)" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(v["ok"], true, "eval failed: {v}");
    v["result"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x.as_str().unwrap().to_string())
        .collect()
}

async fn clear_clicks(http: &reqwest::Client, base: &str) {
    let v: Value = http
        .post(format!("{base}/api/eval"))
        .json(&serde_json::json!({ "js": "window.__clicks = []; true" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(v["ok"], true, "clear clicks failed: {v}");
}

async fn take_screenshot(
    http: &reqwest::Client,
    base: &str,
) -> (image::RgbImage, reqwest::header::HeaderMap) {
    let res = http
        .get(format!("{base}/api/screenshot"))
        .send()
        .await
        .unwrap();
    assert!(res.status().is_success(), "screenshot HTTP {}", res.status());
    let headers = res.headers().clone();
    let bytes = res.bytes().await.unwrap();
    let img = image::load_from_memory(&bytes)
        .expect("decode screenshot")
        .to_rgb8();
    (img, headers)
}

fn header_u32(headers: &reqwest::header::HeaderMap, name: &str) -> u32 {
    headers
        .get(name)
        .unwrap_or_else(|| panic!("missing header {name}"))
        .to_str()
        .unwrap()
        .parse()
        .unwrap()
}

fn header_f64(headers: &reqwest::header::HeaderMap, name: &str) -> f64 {
    headers
        .get(name)
        .unwrap_or_else(|| panic!("missing header {name}"))
        .to_str()
        .unwrap()
        .parse()
        .unwrap()
}

#[tokio::test]
#[ignore = "requires local Chrome/Chromium binary"]
async fn screenshot_to_click_at_desktop_mobile_and_zoomed_tablet() {
    // 1. Start local HTML server on an ephemeral port.
    let html_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let html_port = html_listener.local_addr().unwrap().port();
    let html_app = Router::new().route("/", get(|| async { Html(TEST_HTML) }));
    tokio::spawn(async move {
        axum::serve(html_listener, html_app).await.unwrap();
    });

    // 2. Start real `lb web` daemon on an ephemeral port with isolated temp profile.
    let lb_port = free_port();
    let profile_dir = std::env::temp_dir().join(format!("lb-e2e-click-at-{lb_port}"));
    let _ = std::fs::create_dir_all(&profile_dir);
    let bin = env!("CARGO_BIN_EXE_lb");
    let child = Command::new(bin)
        .args([
            "web",
            "--foreground",
            "--headless",
            "--no-open",
            "--no-sandbox",
            "--port",
            &lb_port.to_string(),
            "--profile",
            profile_dir.to_str().unwrap(),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn lb web");
    let _guard = DaemonGuard { child, profile_dir };

    let base = format!("http://127.0.0.1:{lb_port}");
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap();
    wait_for_health(&http, &base).await;

    // 3. Open the test page.
    let open_res: Value = http
        .post(format!("{base}/api/open"))
        .json(&serde_json::json!({ "url": format!("http://127.0.0.1:{html_port}/") }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(open_res["ok"], true, "open failed: {open_res}");

    // --- Scenario A: Desktop (1280x900, dpr=1) ---
    let (img, hdrs) = take_screenshot(&http, &base).await;
    assert_eq!(header_u32(&hdrs, "x-lb-image-width"), img.width());
    assert_eq!(header_u32(&hdrs, "x-lb-image-height"), img.height());
    assert_eq!(header_u32(&hdrs, "x-lb-viewport-width"), 1280);
    assert_eq!(header_u32(&hdrs, "x-lb-viewport-height"), 900);
    assert!((header_f64(&hdrs, "x-lb-dpr") - 1.0).abs() < 1e-6);

    let (rx, ry, _) = find_color_centroid(&img, |r, g, b| r > 200 && g < 50 && b < 50);
    let (gx, gy, _) = find_color_centroid(&img, |r, g, b| g > 200 && r < 50 && b < 50);
    let (bx, by, _) = find_color_centroid(&img, |r, g, b| b > 200 && r < 50 && g < 50);
    println!(
        "[desktop 1280x900 dpr=1] img={}x{} red=({rx:.1},{ry:.1}) green=({gx:.1},{gy:.1}) blue=({bx:.1},{by:.1})",
        img.width(),
        img.height()
    );

    // Click red with coord_space="image"
    let r1: Value = http
        .post(format!("{base}/api/click-at"))
        .json(&serde_json::json!({ "x": rx, "y": ry, "coord_space": "image" }))
        .send().await.unwrap().json().await.unwrap();
    assert_eq!(r1["ok"], true, "desktop image click failed: {r1}");
    assert_eq!(r1["hit"]["id"], "btn-red", "unexpected hit: {r1}");

    // Click green with coord_space="normalized"
    let r2: Value = http
        .post(format!("{base}/api/click-at"))
        .json(&serde_json::json!({
            "x": gx / img.width() as f64,
            "y": gy / img.height() as f64,
            "coord_space": "normalized"
        }))
        .send().await.unwrap().json().await.unwrap();
    assert_eq!(r2["ok"], true, "desktop normalized click failed: {r2}");
    assert_eq!(r2["hit"]["id"], "btn-green", "unexpected hit: {r2}");

    // Click blue with coord_space="css" (default)
    let css_bx = bx * 1280.0 / img.width() as f64;
    let css_by = by * 900.0 / img.height() as f64;
    let r3: Value = http
        .post(format!("{base}/api/click-at"))
        .json(&serde_json::json!({ "x": css_bx, "y": css_by }))
        .send().await.unwrap().json().await.unwrap();
    assert_eq!(r3["ok"], true, "desktop css click failed: {r3}");
    assert_eq!(r3["hit"]["id"], "btn-blue", "unexpected hit: {r3}");

    assert_eq!(get_clicks(&http, &base).await, vec!["btn-red", "btn-green", "btn-blue"]);

    // Out-of-bounds rejection check
    let oob: Value = http
        .post(format!("{base}/api/click-at"))
        .json(&serde_json::json!({ "x": 1.5, "y": 0.5, "coord_space": "normalized" }))
        .send().await.unwrap().json().await.unwrap();
    assert_eq!(oob["ok"], false, "expected out-of-bounds error: {oob}");
    assert!(oob["error"].as_str().unwrap().contains("0.0..=1.0"));

    // --- Scenario B: Mobile HiDPI (390x844, dpr=3 -> 1170x2532 screenshot) ---
    clear_clicks(&http, &base).await;
    let resize_mobile: Value = http
        .post(format!("{base}/api/resize"))
        .json(&serde_json::json!({ "preset": "mobile" }))
        .send().await.unwrap().json().await.unwrap();
    assert_eq!(resize_mobile["ok"], true, "mobile resize failed: {resize_mobile}");

    let (img, hdrs) = take_screenshot(&http, &base).await;
    assert_eq!(header_u32(&hdrs, "x-lb-viewport-width"), 390);
    assert_eq!(header_u32(&hdrs, "x-lb-viewport-height"), 844);
    assert!((header_f64(&hdrs, "x-lb-dpr") - 3.0).abs() < 1e-6);
    assert_eq!(header_u32(&hdrs, "x-lb-image-width"), 1170);
    assert_eq!(header_u32(&hdrs, "x-lb-image-height"), 2532);
    assert_eq!((img.width(), img.height()), (1170, 2532));

    let (rx, ry, _) = find_color_centroid(&img, |r, g, b| r > 200 && g < 50 && b < 50);
    let (gx, gy, _) = find_color_centroid(&img, |r, g, b| g > 200 && r < 50 && b < 50);
    let (bx, by, _) = find_color_centroid(&img, |r, g, b| b > 200 && r < 50 && g < 50);
    println!(
        "[mobile 390x844 dpr=3] img={}x{} red=({rx:.1},{ry:.1}) green=({gx:.1},{gy:.1}) blue=({bx:.1},{by:.1})",
        img.width(),
        img.height()
    );

    let m1: Value = http
        .post(format!("{base}/api/click-at"))
        .json(&serde_json::json!({ "x": rx, "y": ry, "coord_space": "image" }))
        .send().await.unwrap().json().await.unwrap();
    assert_eq!(m1["ok"], true, "mobile image click failed: {m1}");
    assert_eq!(m1["hit"]["id"], "btn-red", "unexpected mobile hit: {m1}");

    let m2: Value = http
        .post(format!("{base}/api/click-at"))
        .json(&serde_json::json!({
            "x": gx / img.width() as f64,
            "y": gy / img.height() as f64,
            "coord_space": "normalized"
        }))
        .send().await.unwrap().json().await.unwrap();
    assert_eq!(m2["ok"], true, "mobile normalized click failed: {m2}");
    assert_eq!(m2["hit"]["id"], "btn-green", "unexpected mobile hit: {m2}");

    let m3: Value = http
        .post(format!("{base}/api/click-at"))
        .json(&serde_json::json!({ "x": bx / 3.0, "y": by / 3.0, "coord_space": "css" }))
        .send().await.unwrap().json().await.unwrap();
    assert_eq!(m3["ok"], true, "mobile css click failed: {m3}");
    assert_eq!(m3["hit"]["id"], "btn-blue", "unexpected mobile hit: {m3}");

    assert_eq!(get_clicks(&http, &base).await, vec!["btn-red", "btn-green", "btn-blue"]);

    // --- Scenario C: Tablet HiDPI (768x1024, dpr=2 -> 1536x2048) + Page Zoom ---
    clear_clicks(&http, &base).await;
    let resize_tablet: Value = http
        .post(format!("{base}/api/resize"))
        .json(&serde_json::json!({ "preset": "tablet" }))
        .send().await.unwrap().json().await.unwrap();
    assert_eq!(resize_tablet["ok"], true, "tablet resize failed: {resize_tablet}");

    let zoom_res: Value = http
        .post(format!("{base}/api/zoom"))
        .json(&serde_json::json!({ "action": "in" }))
        .send().await.unwrap().json().await.unwrap();
    assert_eq!(zoom_res["ok"], true, "zoom failed: {zoom_res}");
    let zoom = zoom_res["zoom"].as_f64().unwrap();
    assert!(zoom > 1.0, "expected zoom > 1.0, got {zoom}");

    let (img, hdrs) = take_screenshot(&http, &base).await;
    assert_eq!(header_u32(&hdrs, "x-lb-viewport-width"), 768);
    assert_eq!(header_u32(&hdrs, "x-lb-viewport-height"), 1024);
    assert!((header_f64(&hdrs, "x-lb-dpr") - 2.0).abs() < 1e-6);
    assert_eq!((img.width(), img.height()), (1536, 2048));

    let (yx, yy, _) = find_color_centroid(&img, |r, g, b| r > 200 && g > 200 && b < 50);
    println!(
        "[tablet 768x1024 dpr=2 zoom={zoom}] img={}x{} yellow=({yx:.1},{yy:.1})",
        img.width(),
        img.height()
    );

    let t1: Value = http
        .post(format!("{base}/api/click-at"))
        .json(&serde_json::json!({ "x": yx, "y": yy, "coord_space": "image" }))
        .send().await.unwrap().json().await.unwrap();
    assert_eq!(t1["ok"], true, "tablet zoomed image click failed: {t1}");
    assert_eq!(t1["hit"]["id"], "btn-yellow", "unexpected tablet zoomed hit: {t1}");
    assert_eq!(get_clicks(&http, &base).await, vec!["btn-yellow"]);
}
