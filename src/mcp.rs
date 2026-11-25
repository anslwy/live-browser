//! `lb mcp` — MCP server on stdio exposing the live-browser daemon as tools.
//!
//! Thin bridge: each tool forwards to the `lb web` daemon HTTP API
//! (same endpoints as `src/client.rs`). The daemon must be running
//! (`lb web --port 8765`); this process is stateless.
//!
//! IMPORTANT: stdout is the JSON-RPC channel. Never `println!` here;
//! tracing is configured to stderr by `main.rs` in MCP mode.

use rmcp::{
    ErrorData, ServerHandler, ServiceExt,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, ContentBlock, ServerCapabilities, ServerConfig},
    schemars, tool, tool_handler, tool_router,
};
use serde::Deserialize;

#[derive(Debug, Clone)]
pub struct LbMcp {
    server: String,
    passcode: Option<String>,
    default_tab: Option<u32>,
    /// This process's agent-session id (sent as `X-LB-Session`): the
    /// daemon scopes every call to tabs this session owns.
    session: String,
    isolated: bool,
    http: reqwest::Client,
    tool_router: ToolRouter<Self>,
}

impl LbMcp {
    pub fn new(server: String, passcode: Option<String>, default_tab: Option<u32>, session: String, isolated: bool) -> Self {
        Self {
            server,
            passcode,
            default_tab,
            session,
            isolated,
            http: reqwest::Client::new(),
            tool_router: Self::tool_router(),
        }
    }

    fn api(&self, path: &str) -> String {
        format!("{}{}", self.server.trim_end_matches('/'), path)
    }

    /// Explicit `tab` wins, else `--tab`/`LB_TAB`, else daemon active tab.
    fn tab(&self, tab: Option<u32>) -> Option<u32> {
        tab.or(self.default_tab)
    }

    fn qs(&self, tab: Option<u32>) -> String {
        match self.tab(tab) {
            Some(id) => format!("?tab={id}"),
            None => String::new(),
        }
    }

    fn auth(&self, b: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        let b = b.header("x-lb-session", &self.session);
        let b = if self.isolated { b.header("x-lb-isolated", "1") } else { b };
        match &self.passcode {
            Some(p) if !p.is_empty() => b.header("x-lb-passcode", p),
            _ => b,
        }
    }

    fn unreachable(&self, e: reqwest::Error) -> ErrorData {
        ErrorData::internal_error(
            format!(
                "daemon not reachable at {} — did you run `lb web`? ({e:#})",
                self.server
            ),
            None,
        )
    }

    async fn get_json(&self, path: &str) -> Result<serde_json::Value, ErrorData> {
        let res = self
            .auth(self.http.get(self.api(path)))
            .send()
            .await
            .map_err(|e| self.unreachable(e))?;
        if res.status() == reqwest::StatusCode::UNAUTHORIZED {
            return Err(ErrorData::internal_error(
                "unauthorized — wrong or missing passcode (set LB_PASSCODE)",
                None,
            ));
        }
        res.json().await.map_err(|e| {
            ErrorData::internal_error(format!("invalid daemon response: {e:#}"), None)
        })
    }

    async fn post_json(
        &self,
        path: &str,
        body: serde_json::Value,
    ) -> Result<serde_json::Value, ErrorData> {
        let res = self
            .auth(self.http.post(self.api(path)))
            .json(&body)
            .send()
            .await
            .map_err(|e| self.unreachable(e))?;
        if res.status() == reqwest::StatusCode::UNAUTHORIZED {
            return Err(ErrorData::internal_error(
                "unauthorized — wrong or missing passcode (set LB_PASSCODE)",
                None,
            ));
        }
        res.json().await.map_err(|e| {
            ErrorData::internal_error(format!("invalid daemon response: {e:#}"), None)
        })
    }

    /// Map a daemon `{ok: …}` value to a tool result. `{ok:false}` becomes a
    /// *tool-level* error (`is_error`, visible to the caller) rather than a
    /// protocol error (which most clients render opaquely).
    fn daemon_result(v: serde_json::Value) -> CallToolResult {
        if v.get("ok").and_then(|x| x.as_bool()) == Some(true) {
            CallToolResult::success(vec![ContentBlock::text(pretty(&v))])
        } else {
            let msg = v
                .get("error")
                .and_then(|x| x.as_str())
                .unwrap_or("unknown daemon error");
            CallToolResult::error(vec![ContentBlock::text(format!("error: {msg}"))])
        }
    }
}

fn pretty(v: &serde_json::Value) -> String {
    serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string())
}

// ---------------------------------------------------------------------------
// Tool argument schemas
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct NoArgs {}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TabArgs {
    /// Tab id (default: active tab). See `list_tabs`.
    #[serde(default)]
    pub tab: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct OpenArgs {
    /// URL to navigate to (plain text searches Google).
    pub url: String,
    #[serde(default)]
    pub tab: Option<u32>,
    /// Deprecated, ignored: navigating your current tab is the default.
    #[serde(default)]
    #[allow(dead_code)]
    pub here: bool,
    /// Open an additional tab instead of navigating your current one
    /// (capped per session; the least recently used get closed).
    #[serde(default)]
    pub new_tab: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ClickArgs {
    /// Snapshot ref (e.g. "12") or CSS selector ("css=.login").
    pub target: String,
    /// "single" (default), "double" or "right".
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub tab: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ClickAtArgs {
    /// X coordinate in `coord_space` (CSS pixels by default; screenshot pixels for "image"; 0.0..=1.0 for "normalized").
    pub x: f64,
    /// Y coordinate in `coord_space` (CSS pixels by default; screenshot pixels for "image"; 0.0..=1.0 for "normalized").
    pub y: f64,
    /// Coordinate space: "css" (default, viewport CSS pixels), "image" (screenshot pixel coordinates), or "normalized" (0.0..=1.0).
    #[serde(default)]
    pub coord_space: Option<String>,
    /// "single" (default), "double" or "right".
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub tab: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct HoverArgs {
    /// Viewport X in CSS pixels (ignored when target is given).
    #[serde(default)]
    pub x: Option<f64>,
    /// Viewport Y in CSS pixels (ignored when target is given).
    #[serde(default)]
    pub y: Option<f64>,
    /// Snapshot ref or CSS selector to hover (menus, tooltips).
    #[serde(default)]
    pub target: Option<String>,
    #[serde(default)]
    pub tab: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ScrollArgs {
    /// Horizontal scroll in CSS pixels (default 0).
    #[serde(default)]
    pub dx: Option<f64>,
    /// Vertical scroll in CSS pixels (default 600).
    #[serde(default)]
    pub dy: Option<f64>,
    #[serde(default)]
    pub tab: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WheelArgs {
    pub x: f64,
    pub y: f64,
    /// Vertical wheel delta (default 300).
    #[serde(default)]
    pub dy: Option<f64>,
    /// Horizontal wheel delta (default 0).
    #[serde(default)]
    pub dx: Option<f64>,
    #[serde(default)]
    pub tab: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DragArgs {
    pub x1: f64,
    pub y1: f64,
    pub x2: f64,
    pub y2: f64,
    /// Mouse move steps (default 10).
    #[serde(default)]
    pub steps: Option<u32>,
    #[serde(default)]
    pub tab: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ZoomArgs {
    /// "in" (default), "out" or "reset".
    #[serde(default)]
    pub action: Option<String>,
    #[serde(default)]
    pub tab: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RecordArgs {
    /// "start", "stop" or "status".
    pub action: String,
    /// start: frames per second (0.5-15, default 5).
    #[serde(default)]
    pub fps: Option<f64>,
    /// start: auto-stop after this many seconds (default 300, max 3600).
    #[serde(default)]
    pub max_secs: Option<u64>,
    /// stop: local path to save the video to (default: ./recording.mp4 or .avi).
    #[serde(default)]
    pub out: Option<String>,
    #[serde(default)]
    pub tab: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ResizeArgs {
    /// Viewport width in CSS pixels (200-3840). Required unless `preset` or `reset` is set.
    #[serde(default)]
    pub width: Option<u32>,
    /// Viewport height in CSS pixels (200-2160). Required unless `preset` or `reset` is set.
    #[serde(default)]
    pub height: Option<u32>,
    /// Device pixel ratio (0.5-4.0; default 1.0, or preset default).
    #[serde(default)]
    pub dpr: Option<f64>,
    /// Emulate a mobile device (meta viewport + touch). Default false (or preset default).
    #[serde(default)]
    pub mobile: Option<bool>,
    /// Named preset: "desktop" (1280x900), "desktop-lg" (1440x900), "fhd" (1920x1080),
    /// "tablet" (768x1024, mobile), "mobile" / "iphone" (390x844, mobile), "mobile-sm" (375x667, mobile).
    #[serde(default)]
    pub preset: Option<String>,
    /// Reset to the default desktop viewport (1280x900, dpr=1, mobile=false).
    #[serde(default)]
    pub reset: bool,
    #[serde(default)]
    pub tab: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TypeArgs {
    /// Snapshot ref (e.g. "3") or CSS selector.
    pub target: String,
    /// Text to type.
    pub text: String,
    /// Press Enter after typing (default false).
    #[serde(default)]
    pub submit: bool,
    /// Append to existing content instead of clearing first (default false).
    #[serde(default)]
    pub append: bool,
    #[serde(default)]
    pub tab: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct InsertArgs {
    /// Text to paste at the cursor (focus the target first).
    pub text: String,
    #[serde(default)]
    pub tab: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SelectArgs {
    /// Snapshot ref or CSS selector of a <select>.
    pub target: String,
    /// Option value or visible text.
    pub value: String,
    #[serde(default)]
    pub tab: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct PressArgs {
    /// Key name (Enter, Tab, Escape, ArrowDown, ...) or chord (ctrl+c, ctrl+shift+t, alt+F4, cmd+v).
    pub key: String,
    #[serde(default)]
    pub tab: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct EvalArgs {
    /// JavaScript to evaluate in the page (returns JSON-ified result).
    pub js: String,
    #[serde(default)]
    pub tab: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct NavigateArgs {
    /// "back", "forward" or "reload".
    pub action: String,
    #[serde(default)]
    pub tab: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct HandoverRequestArgs {
    /// Message shown to the human (login instructions, ...).
    pub message: String,
    #[serde(default)]
    pub tab: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ClaimArgs {
    #[serde(default = "default_claim_message")]
    pub message: String,
    #[serde(default)]
    pub tab: Option<u32>,
}

fn default_claim_message() -> String {
    "Human claimed control from the agent.".to_string()
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ActivateArgs {
    /// Tab id to switch to (see `list_tabs`).
    pub tab: u32,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WaitArgs {
    /// Snapshot ref, css= selector or plain CSS to wait for.
    pub target: String,
    /// Max seconds to wait (default 15, max 120).
    #[serde(default)]
    pub timeout: Option<u64>,
    /// Wait until the target disappears instead of appears.
    #[serde(default)]
    pub gone: bool,
    #[serde(default)]
    pub tab: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ContentArgs {
    /// Snapshot ref, css= selector or plain CSS (default: whole body).
    #[serde(default)]
    pub target: Option<String>,
    /// Max characters returned (default 8000).
    #[serde(default)]
    pub max: Option<usize>,
    #[serde(default)]
    pub tab: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct UploadArgs {
    /// Snapshot ref or CSS selector of an <input type=file>.
    pub target: String,
    /// Local file path to attach (resolved on the `lb web` machine).
    pub file: String,
    #[serde(default)]
    pub tab: Option<u32>,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct BatchStep {
    /// Action to perform: "click", "click_at", "type_text", "insert", "select",
    /// "press", "hover", "scroll", "wheel", "wait", "sleep", "eval", "open", or "navigate".
    pub action: String,
    /// Target ref or CSS selector (for click, type_text, select, hover, wait).
    #[serde(default)]
    pub target: Option<String>,
    /// Text to type or insert (for type_text, insert).
    #[serde(default)]
    pub text: Option<String>,
    /// X coordinate (for click_at, hover, wheel).
    #[serde(default)]
    pub x: Option<f64>,
    /// Y coordinate (for click_at, hover, wheel).
    #[serde(default)]
    pub y: Option<f64>,
    /// Coordinate space for click_at: "css" (default), "image", or "normalized".
    #[serde(default)]
    pub coord_space: Option<String>,
    /// Click kind ("single", "double", "right") for click / click_at.
    #[serde(default)]
    pub kind: Option<String>,
    /// Dropdown option value/text (for select) or navigation action ("back", "forward", "reload" for navigate).
    #[serde(default)]
    pub value: Option<String>,
    /// Key name or chord (for press).
    #[serde(default)]
    pub key: Option<String>,
    /// Horizontal delta (for scroll, wheel).
    #[serde(default)]
    pub dx: Option<f64>,
    /// Vertical delta (for scroll, wheel).
    #[serde(default)]
    pub dy: Option<f64>,
    /// Press Enter after type_text (default false).
    #[serde(default)]
    pub submit: bool,
    /// Append instead of clearing in type_text (default false).
    #[serde(default)]
    pub append: bool,
    /// Wait timeout in seconds (for wait, default 15).
    #[serde(default)]
    pub timeout: Option<u64>,
    /// Wait until target disappears (for wait, default false).
    #[serde(default)]
    pub gone: bool,
    /// Sleep duration in milliseconds (for sleep, max 10000).
    #[serde(default)]
    pub ms: Option<u64>,
    /// JavaScript expression (for eval).
    #[serde(default)]
    pub js: Option<String>,
    /// URL to open (for open).
    #[serde(default)]
    pub url: Option<String>,
    /// Optional tab id override for this step.
    #[serde(default)]
    pub tab: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct BatchArgs {
    /// Ordered list of actions to run sequentially (max 50).
    pub actions: Vec<BatchStep>,
    /// Default tab id for all actions in the batch.
    #[serde(default)]
    pub tab: Option<u32>,
    /// Stop on the first failed action (default true).
    #[serde(default = "default_true")]
    pub stop_on_error: bool,
    /// Include a fresh snapshot of interactive elements after the batch finishes (default false).
    #[serde(default)]
    pub snapshot_after: bool,
    /// Capture and return a screenshot after the batch finishes (default false).
    #[serde(default)]
    pub screenshot_after: bool,
}

// ---------------------------------------------------------------------------
// Tools (one per CLI command; back/forward/reload share `navigate`)
// ---------------------------------------------------------------------------

#[tool_router]
impl LbMcp {
    #[tool(description = "Navigate to a URL (plain text searches Google). Reuses this session's own tab (created on first call); pass new_tab=true for an extra tab.")]
    async fn open(
        &self,
        Parameters(a): Parameters<OpenArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let v = self
            .post_json(
                "/api/open",
                serde_json::json!({ "url": a.url, "tab": self.tab(a.tab), "new_tab": a.new_tab }),
            )
            .await?;
        Ok(Self::daemon_result(v))
    }

    #[tool(description = "Current URL, title, viewport ({width, height, dpr, mobile}), handover state of a tab, and `locked` (true = the user locked the browser from the web UI; actions fail until they unlock).")]
    async fn status(
        &self,
        Parameters(a): Parameters<TabArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let v = self
            .get_json(&format!("/api/status{}", self.qs(a.tab)))
            .await?;
        Ok(Self::daemon_result(v))
    }

    #[tool(description = "List all tabs (id, title, url, owner session, mine). You can only act on tabs where mine=true (or shared ones by explicit id).")]
    async fn list_tabs(
        &self,
        Parameters(_): Parameters<NoArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let v = self.get_json("/api/tabs").await?;
        Ok(Self::daemon_result(v))
    }

    #[tool(
        description = "Agent-friendly snapshot of interactive elements: [{ref, tag, name, href, value}]. Call this before clicking/typing; refs expire after navigation."
    )]
    async fn snapshot(
        &self,
        Parameters(a): Parameters<TabArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let v = self
            .get_json(&format!("/api/snapshot{}", self.qs(a.tab)))
            .await?;
        Ok(Self::daemon_result(v))
    }

    #[tool(
        description = "Screenshot of the current page viewport. Returns an image plus its exact pixel dimensions, CSS viewport dimensions, DPR, and coordinate mapping for click_at — call after navigation or when the snapshot is ambiguous."
    )]
    async fn screenshot(
        &self,
        Parameters(a): Parameters<TabArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let url = self.api(&format!("/api/screenshot{}", self.qs(a.tab)));
        let result = async {
            let res = self.auth(self.http.get(&url)).send().await?;
            let status = res.status();
            let headers = res.headers().clone();
            let bytes = res.bytes().await?;
            let img = crate::screenshot::validate_response_with_dimensions(status, &bytes)?;
            let vp_w = headers
                .get("x-lb-viewport-width")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<u32>().ok())
                .unwrap_or(img.width);
            let vp_h = headers
                .get("x-lb-viewport-height")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<u32>().ok())
                .unwrap_or(img.height);
            let dpr = headers
                .get("x-lb-dpr")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("1");
            let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &bytes);
            let meta = format!(
                "screenshot ({} bytes, {}) | image: {}x{}px | viewport: {}x{} CSS px (dpr={dpr}) | click_at coord_space: \"image\" (0..{}, 0..{}), \"css\" (0..{vp_w}, 0..{vp_h}, default), or \"normalized\" (0..1, 0..1)",
                bytes.len(),
                img.mime,
                img.width,
                img.height,
                vp_w,
                vp_h,
                img.width,
                img.height,
            );
            Ok::<_, anyhow::Error>(CallToolResult::success(vec![
                ContentBlock::image(b64, img.mime),
                ContentBlock::text(meta),
            ]))
        }.await;
        Ok(result.unwrap_or_else(|e| CallToolResult::error(vec![
            ContentBlock::text(format!("Screenshot failed: {e:#}. Check browser health; run lb recover if the browser is dead.")),
        ])))
    }

    #[tool(
        description = "Click an element by snapshot ref (e.g. \"12\") or CSS selector (\"css=.login\"). Works for same-origin iframe elements too. Set kind to double/right for double- or right-click. CSS clicks return after input dispatch. Use wait for any asynchronous DOM change you need to observe."
    )]
    async fn click(
        &self,
        Parameters(a): Parameters<ClickArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let v = self
            .post_json(
                "/api/click",
                serde_json::json!({ "target": a.target, "kind": a.kind.unwrap_or_else(|| "single".into()), "tab": self.tab(a.tab) }),
            )
            .await?;
        Ok(Self::daemon_result(v))
    }

    #[tool(
        description = "Click at viewport or screenshot coordinates. Use coord_space=\"image\" for pixel coordinates directly from a screenshot (automatically scaled for DPR/HiDPI), \"normalized\" for (0..1, 0..1), or \"css\" (default) for CSS viewport pixels. Returns the resolved CSS (x, y) and hit element summary so you can verify the target. Set kind to double/right for double- or right-click."
    )]
    async fn click_at(
        &self,
        Parameters(a): Parameters<ClickAtArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let v = self
            .post_json(
                "/api/click-at",
                serde_json::json!({
                    "x": a.x,
                    "y": a.y,
                    "coord_space": a.coord_space.unwrap_or_else(|| "css".into()),
                    "kind": a.kind.unwrap_or_else(|| "single".into()),
                    "tab": self.tab(a.tab),
                }),
            )
            .await?;
        Ok(Self::daemon_result(v))
    }

    #[tool(description = "Move the mouse: hover an element by snapshot ref/CSS (target) or move to viewport coordinates x/y (hover states, tooltips, menus).")]
    async fn hover(
        &self,
        Parameters(a): Parameters<HoverArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let v = self
            .post_json(
                "/api/hover",
                serde_json::json!({ "x": a.x, "y": a.y, "target": a.target, "tab": self.tab(a.tab) }),
            )
            .await?;
        Ok(Self::daemon_result(v))
    }

    #[tool(description = "Scroll the page by dx/dy CSS pixels (default: down 600px).")]
    async fn scroll(
        &self,
        Parameters(a): Parameters<ScrollArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let v = self
            .post_json(
                "/api/scroll",
                serde_json::json!({
                    "dx": a.dx.unwrap_or(0.0),
                    "dy": a.dy.unwrap_or(600.0),
                    "tab": self.tab(a.tab),
                }),
            )
            .await?;
        Ok(Self::daemon_result(v))
    }

    #[tool(
        description = "Trackpad-style wheel scroll over whatever is under the point (scrolls inner containers, unlike scroll)."
    )]
    async fn wheel(
        &self,
        Parameters(a): Parameters<WheelArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let v = self
            .post_json(
                "/api/wheel",
                serde_json::json!({
                    "x": a.x, "y": a.y,
                    "dx": a.dx.unwrap_or(0.0), "dy": a.dy.unwrap_or(300.0),
                    "tab": self.tab(a.tab),
                }),
            )
            .await?;
        Ok(Self::daemon_result(v))
    }

    #[tool(
        description = "Mouse drag from (x1,y1) to (x2,y2): text selection, sliders, resize handles, drag-and-drop."
    )]
    async fn drag(
        &self,
        Parameters(a): Parameters<DragArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let v = self
            .post_json(
                "/api/drag",
                serde_json::json!({
                    "x1": a.x1, "y1": a.y1, "x2": a.x2, "y2": a.y2,
                    "steps": a.steps.unwrap_or(10),
                    "tab": self.tab(a.tab),
                }),
            )
            .await?;
        Ok(Self::daemon_result(v))
    }

    #[tool(description = "Page zoom for small viewports (persists across navigations): in, out or reset.")]
    async fn zoom(
        &self,
        Parameters(a): Parameters<ZoomArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let v = self
            .post_json(
                "/api/zoom",
                serde_json::json!({
                    "action": a.action.unwrap_or_else(|| "in".to_string()),
                    "tab": self.tab(a.tab),
                }),
            )
            .await?;
        Ok(Self::daemon_result(v))
    }

    #[tool(description = "Screen-record a tab. action=start begins capturing (fps, max_secs); keep driving the page; action=stop encodes the video (MP4 if ffmpeg is on the daemon host, else MJPEG AVI) and saves it to `out` locally; action=status lists active recordings. Works while the user lock is on, so you can record the user driving.")]
    async fn record(
        &self,
        Parameters(a): Parameters<RecordArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let action = a.action.to_lowercase();
        let v = self
            .post_json(
                "/api/record",
                serde_json::json!({ "action": action, "fps": a.fps, "max_secs": a.max_secs, "tab": self.tab(a.tab) }),
            )
            .await?;
        if action != "stop" || v.get("ok").and_then(|x| x.as_bool()) != Some(true) {
            return Ok(Self::daemon_result(v));
        }
        let ext = v.get("ext").and_then(|x| x.as_str()).unwrap_or("mp4").to_string();
        let out = a.out.unwrap_or_else(|| format!("recording.{ext}"));
        let dl = v.get("download").and_then(|x| x.as_str()).unwrap_or_default().to_string();
        let saved = async {
            let res = self.auth(self.http.get(self.api(&dl))).send().await?;
            if !res.status().is_success() {
                anyhow::bail!("HTTP {}", res.status());
            }
            let bytes = res.bytes().await?;
            tokio::fs::write(&out, &bytes).await?;
            Ok::<_, anyhow::Error>(std::fs::canonicalize(&out).map(|p| p.display().to_string()).unwrap_or(out.clone()))
        }
        .await;
        let mut v = v;
        match saved {
            Ok(p) => v["saved"] = serde_json::json!(p),
            Err(e) => v["save_error"] = serde_json::json!(format!("could not save locally ({e:#}); file stays on the daemon host at `path`")),
        }
        Ok(Self::daemon_result(v))
    }

    #[tool(description = "Resize a tab's viewport (CSS pixels, optional dpr & mobile emulation, or a named preset like mobile/tablet/desktop/desktop-lg/fhd, or reset=true). Persists across navigations; check `status` for current viewport dimensions.")]
    async fn resize(
        &self,
        Parameters(a): Parameters<ResizeArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let v = self
            .post_json(
                "/api/resize",
                serde_json::json!({
                    "width": a.width,
                    "height": a.height,
                    "dpr": a.dpr,
                    "mobile": a.mobile,
                    "preset": a.preset,
                    "reset": a.reset,
                    "tab": self.tab(a.tab),
                }),
            )
            .await?;
        Ok(Self::daemon_result(v))
    }

    #[tool(
        description = "Type text into an element by snapshot ref or CSS. Clears first unless append=true. Set submit=true to press Enter afterwards."
    )]
    async fn type_text(
        &self,
        Parameters(a): Parameters<TypeArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let v = self
            .post_json(
                "/api/type",
                serde_json::json!({
                    "target": a.target, "text": a.text,
                    "submit": a.submit, "append": a.append, "tab": self.tab(a.tab),
                }),
            )
            .await?;
        Ok(Self::daemon_result(v))
    }

    #[tool(description = "Fast paste of text at the cursor (focus the target first, e.g. via click).")]
    async fn insert(
        &self,
        Parameters(a): Parameters<InsertArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let v = self
            .post_json(
                "/api/insert",
                serde_json::json!({ "text": a.text, "tab": self.tab(a.tab) }),
            )
            .await?;
        Ok(Self::daemon_result(v))
    }

    #[tool(description = "Set a dropdown (<select>) option by value or visible text.")]
    async fn select(
        &self,
        Parameters(a): Parameters<SelectArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let v = self
            .post_json(
                "/api/select",
                serde_json::json!({
                    "target": a.target, "value": a.value, "tab": self.tab(a.tab),
                }),
            )
            .await?;
        Ok(Self::daemon_result(v))
    }

    #[tool(description = "Press a key (Enter, Tab, Escape, ArrowDown, ...) or a modifier chord (ctrl+c, ctrl+shift+t, alt+F4, cmd+v).")]
    async fn press(
        &self,
        Parameters(a): Parameters<PressArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let v = self
            .post_json(
                "/api/press",
                serde_json::json!({ "key": a.key, "tab": self.tab(a.tab) }),
            )
            .await?;
        Ok(Self::daemon_result(v))
    }

    #[tool(description = "Evaluate arbitrary JavaScript in the page (returns a JSON-ified result).")]
    async fn eval(
        &self,
        Parameters(a): Parameters<EvalArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let v = self
            .post_json(
                "/api/eval",
                serde_json::json!({ "js": a.js, "tab": self.tab(a.tab) }),
            )
            .await?;
        Ok(Self::daemon_result(v))
    }

    #[tool(description = "History navigation: back, forward or reload.")]
    async fn navigate(
        &self,
        Parameters(a): Parameters<NavigateArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let v = self
            .post_json(
                "/api/nav",
                serde_json::json!({ "action": a.action, "tab": self.tab(a.tab) }),
            )
            .await?;
        Ok(Self::daemon_result(v))
    }

    #[tool(
        description = "Ask a human to take over (login, captcha, 2FA). Returns immediately — poll handover_status until active=false, then continue with snapshot."
    )]
    async fn handover_request(
        &self,
        Parameters(a): Parameters<HandoverRequestArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let v = self
            .post_json(
                "/api/handover/request",
                serde_json::json!({ "message": a.message, "tab": self.tab(a.tab) }),
            )
            .await?;
        Ok(Self::daemon_result(v))
    }

    #[tool(description = "Check whether a requested human handover is done (active=false means resume).")]
    async fn handover_status(
        &self,
        Parameters(a): Parameters<TabArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let v = self
            .get_json(&format!("/api/handover{}", self.qs(a.tab)))
            .await?;
        Ok(Self::daemon_result(v))
    }

    #[tool(description = "End a handover (human scripts / testing).")]
    async fn handover_done(
        &self,
        Parameters(a): Parameters<TabArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let v = self
            .post_json(
                &format!("/api/handover/done{}", self.qs(a.tab)),
                serde_json::json!({}),
            )
            .await?;
        Ok(Self::daemon_result(v))
    }

    #[tool(description = "Claim control as a human (same as the Take control button in the web UI).")]
    async fn claim(
        &self,
        Parameters(a): Parameters<ClaimArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let v = self
            .post_json(
                "/api/handover/claim",
                serde_json::json!({ "message": a.message, "tab": self.tab(a.tab) }),
            )
            .await?;
        Ok(Self::daemon_result(v))
    }

    #[tool(description = "Close a tab (default: active). Warns: agents lose control of it.")]
    async fn close(
        &self,
        Parameters(a): Parameters<TabArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let v = self
            .post_json("/api/close", serde_json::json!({ "tab": self.tab(a.tab) }))
            .await?;
        Ok(Self::daemon_result(v))
    }

    #[tool(description = "Close one of your tabs (default: your current tab). Same as close.")]
    async fn close_tab(
        &self,
        Parameters(a): Parameters<TabArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.close(Parameters(a)).await
    }

    #[tool(description = "Switch your current tab (see list_tabs for ids).")]
    async fn activate_tab(
        &self,
        Parameters(a): Parameters<ActivateArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let v = self
            .post_json("/api/activate", serde_json::json!({ "tab": a.tab }))
            .await?;
        Ok(Self::daemon_result(v))
    }

    #[tool(
        description = "Shareable web UI link for a tab (/?tabId=N). Opening it focuses that tab in the UI — send it to the human so they land on the right tab."
    )]
    async fn tab_link(
        &self,
        Parameters(a): Parameters<TabArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let v = self.get_json("/api/tabs").await?;
        if v.get("ok").and_then(|x| x.as_bool()) != Some(true) {
            return Ok(Self::daemon_result(v));
        }
        let tabs = v.get("tabs").and_then(|x| x.as_array()).cloned().unwrap_or_default();
        if tabs.is_empty() {
            return Ok(CallToolResult::error(vec![ContentBlock::text(
                "error: no tabs — is `lb web` running?",
            )]));
        }
        let wanted = self.tab(a.tab);
        let id: u64 = match wanted {
            Some(id) => id as u64,
            None => tabs
                .iter()
                .find(|t| t.get("active").and_then(|x| x.as_bool()).unwrap_or(false))
                .and_then(|t| t.get("id").and_then(|x| x.as_u64()))
                .unwrap_or(0),
        };
        match tabs.iter().find(|t| t.get("id").and_then(|x| x.as_u64()) == Some(id)) {
            Some(t) => {
                let url = format!("{}/?tabId={id}", self.server.trim_end_matches('/'));
                let mut out = serde_json::json!({ "ok": true, "tab": id, "url": url });
                if let Some(title) = t.get("title").and_then(|x| x.as_str()) {
                    out["title"] = serde_json::Value::String(title.to_string());
                }
                if let Some(page_url) = t.get("url").and_then(|x| x.as_str()) {
                    out["page_url"] = serde_json::Value::String(page_url.to_string());
                }
                Ok(CallToolResult::success(vec![ContentBlock::text(pretty(&out))]))
            }
            None => Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                "error: no such tab: {id} — see list_tabs"
            ))])),
        }
    }

    #[tool(description = "Browser health probe (dead after macOS sleep?). Returns probe ok/dead — call recover_browser when dead.")]
    async fn browser_health(
        &self,
        Parameters(_): Parameters<NoArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let v = self.get_json("/api/health").await?;
        Ok(Self::daemon_result(v))
    }

    #[tool(description = "Relaunch a dead/wedged browser in place (same engine, no daemon restart). Open tabs reset. Call after screenshot/CDP transport failures e.g. post-sleep 502s.")]
    async fn recover_browser(
        &self,
        Parameters(_): Parameters<NoArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let v = self.post_json("/api/recover", serde_json::json!({})).await?;
        Ok(Self::daemon_result(v))
    }

    #[tool(description = "Recent daemon action log (last entries).")]
    async fn log(
        &self,
        Parameters(_): Parameters<NoArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let v = self.get_json("/api/log").await?;
        Ok(Self::daemon_result(v))
    }

    #[tool(description = "Wait until an element appears (or disappears with gone=true): polls server-side. Replaces sleep/retry loops after navigation.")]
    async fn wait(
        &self,
        Parameters(a): Parameters<WaitArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let v = self
            .post_json(
                "/api/wait",
                serde_json::json!({
                    "target": a.target,
                    "timeout": a.timeout.unwrap_or(15),
                    "gone": a.gone,
                    "tab": self.tab(a.tab),
                }),
            )
            .await?;
        Ok(Self::daemon_result(v))
    }

    #[tool(description = "Visible text of the page (or one element by ref/CSS). For reading articles, results, messages.")]
    async fn text(
        &self,
        Parameters(a): Parameters<ContentArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let v = self
            .post_json(
                "/api/text",
                serde_json::json!({
                    "target": a.target,
                    "max": a.max.unwrap_or(8000),
                    "tab": self.tab(a.tab),
                }),
            )
            .await?;
        Ok(Self::daemon_result(v))
    }

    #[tool(description = "HTML of the page (or one element by ref/CSS). For tables and structures the snapshot can't see.")]
    async fn html(
        &self,
        Parameters(a): Parameters<ContentArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let v = self
            .post_json(
                "/api/html",
                serde_json::json!({
                    "target": a.target,
                    "max": a.max.unwrap_or(8000),
                    "tab": self.tab(a.tab),
                }),
            )
            .await?;
        Ok(Self::daemon_result(v))
    }

    #[tool(description = "Attach a local file to an <input type=file> (path is resolved on the `lb web` machine).")]
    async fn upload(
        &self,
        Parameters(a): Parameters<UploadArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let v = self
            .post_json(
                "/api/upload",
                serde_json::json!({
                    "target": a.target, "file": a.file, "tab": self.tab(a.tab),
                }),
            )
            .await?;
        Ok(Self::daemon_result(v))
    }

    #[tool(description = "Files downloaded through the browser (newest first).")]
    async fn downloads(
        &self,
        Parameters(_): Parameters<NoArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let v = self.get_json("/api/downloads").await?;
        Ok(Self::daemon_result(v))
    }

    #[tool(
        description = "Run a sequence of browser actions in order in a single call (ideal for filling forms: click_at+insert, type_text, select, press, scroll, wait, sleep, eval, etc.). Stops on first error by default; set snapshot_after=true or screenshot_after=true to inspect the result without an extra round-trip."
    )]
    async fn batch(
        &self,
        Parameters(a): Parameters<BatchArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        const MAX_BATCH: usize = 50;
        if a.actions.is_empty() {
            return Ok(CallToolResult::error(vec![ContentBlock::text(
                "error: actions list is empty",
            )]));
        }
        if a.actions.len() > MAX_BATCH {
            return Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                "error: too many actions ({}, max {MAX_BATCH})",
                a.actions.len()
            ))]));
        }

        let total = a.actions.len();
        let mut results: Vec<serde_json::Value> = Vec::with_capacity(total);
        let mut failed_index: Option<usize> = None;

        for (idx, step) in a.actions.into_iter().enumerate() {
            let tab = self.tab(step.tab.or(a.tab));
            let action = step.action.trim().to_lowercase();
            let res: serde_json::Value = match action.as_str() {
                "click" => match step.target {
                    Some(target) => {
                        self.post_json(
                            "/api/click",
                            serde_json::json!({
                                "target": target,
                                "kind": step.kind.unwrap_or_else(|| "single".into()),
                                "tab": tab,
                            }),
                        )
                        .await?
                    }
                    None => serde_json::json!({ "ok": false, "error": "click requires `target`" }),
                },
                "click_at" => match (step.x, step.y) {
                    (Some(x), Some(y)) => {
                        self.post_json(
                            "/api/click-at",
                            serde_json::json!({
                                "x": x,
                                "y": y,
                                "coord_space": step.coord_space.unwrap_or_else(|| "css".into()),
                                "kind": step.kind.unwrap_or_else(|| "single".into()),
                                "tab": tab,
                            }),
                        )
                        .await?
                    }
                    _ => serde_json::json!({ "ok": false, "error": "click_at requires `x` and `y`" }),
                },
                "type_text" | "type" => match (step.target, step.text) {
                    (Some(target), Some(text)) => {
                        self.post_json(
                            "/api/type",
                            serde_json::json!({
                                "target": target,
                                "text": text,
                                "submit": step.submit,
                                "append": step.append,
                                "tab": tab,
                            }),
                        )
                        .await?
                    }
                    _ => serde_json::json!({ "ok": false, "error": "type_text requires `target` and `text`" }),
                },
                "insert" => match step.text {
                    Some(text) => {
                        self.post_json(
                            "/api/insert",
                            serde_json::json!({ "text": text, "tab": tab }),
                        )
                        .await?
                    }
                    None => serde_json::json!({ "ok": false, "error": "insert requires `text`" }),
                },
                "select" => match (step.target, step.value) {
                    (Some(target), Some(value)) => {
                        self.post_json(
                            "/api/select",
                            serde_json::json!({ "target": target, "value": value, "tab": tab }),
                        )
                        .await?
                    }
                    _ => serde_json::json!({ "ok": false, "error": "select requires `target` and `value`" }),
                },
                "press" => match step.key {
                    Some(key) => {
                        self.post_json(
                            "/api/press",
                            serde_json::json!({ "key": key, "tab": tab }),
                        )
                        .await?
                    }
                    None => serde_json::json!({ "ok": false, "error": "press requires `key`" }),
                },
                "hover" => {
                    self.post_json(
                        "/api/hover",
                        serde_json::json!({
                            "x": step.x,
                            "y": step.y,
                            "target": step.target,
                            "tab": tab,
                        }),
                    )
                    .await?
                }
                "scroll" => {
                    self.post_json(
                        "/api/scroll",
                        serde_json::json!({
                            "dx": step.dx.unwrap_or(0.0),
                            "dy": step.dy.unwrap_or(600.0),
                            "tab": tab,
                        }),
                    )
                    .await?
                }
                "wheel" => match (step.x, step.y) {
                    (Some(x), Some(y)) => {
                        self.post_json(
                            "/api/wheel",
                            serde_json::json!({
                                "x": x,
                                "y": y,
                                "dx": step.dx.unwrap_or(0.0),
                                "dy": step.dy.unwrap_or(300.0),
                                "tab": tab,
                            }),
                        )
                        .await?
                    }
                    _ => serde_json::json!({ "ok": false, "error": "wheel requires `x` and `y`" }),
                },
                "wait" => match step.target {
                    Some(target) => {
                        self.post_json(
                            "/api/wait",
                            serde_json::json!({
                                "target": target,
                                "timeout": step.timeout.unwrap_or(15),
                                "gone": step.gone,
                                "tab": tab,
                            }),
                        )
                        .await?
                    }
                    None => serde_json::json!({ "ok": false, "error": "wait requires `target`" }),
                },
                "sleep" => {
                    let ms = step.ms.unwrap_or(200).min(10_000);
                    tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
                    serde_json::json!({ "ok": true, "slept_ms": ms })
                }
                "eval" => match step.js {
                    Some(js) => {
                        self.post_json("/api/eval", serde_json::json!({ "js": js, "tab": tab }))
                            .await?
                    }
                    None => serde_json::json!({ "ok": false, "error": "eval requires `js`" }),
                },
                "open" => match step.url {
                    Some(url) => {
                        self.post_json(
                            "/api/open",
                            serde_json::json!({ "url": url, "tab": tab, "new_tab": false }),
                        )
                        .await?
                    }
                    None => serde_json::json!({ "ok": false, "error": "open requires `url`" }),
                },
                "navigate" => match step.value {
                    Some(nav_action) => {
                        self.post_json(
                            "/api/nav",
                            serde_json::json!({ "action": nav_action, "tab": tab }),
                        )
                        .await?
                    }
                    None => serde_json::json!({ "ok": false, "error": "navigate requires `value` (back/forward/reload)" }),
                },
                other => serde_json::json!({
                    "ok": false,
                    "error": format!("unknown batch action: {other:?}")
                }),
            };

            let step_ok = res.get("ok").and_then(|x| x.as_bool()) == Some(true);
            results.push(serde_json::json!({
                "index": idx,
                "action": action,
                "ok": step_ok,
                "result": res,
            }));
            if !step_ok {
                if failed_index.is_none() {
                    failed_index = Some(idx);
                }
                if a.stop_on_error {
                    break;
                }
            }
        }

        let mut summary = serde_json::json!({
            "ok": failed_index.is_none(),
            "completed": results.len(),
            "total": total,
            "failed_index": failed_index,
            "results": results,
        });

        if a.snapshot_after {
            let snap = self.get_json(&format!("/api/snapshot{}", self.qs(a.tab))).await?;
            summary["snapshot"] = snap;
        }

        let mut blocks = vec![ContentBlock::text(pretty(&summary))];

        if a.screenshot_after {
            let shot = self.screenshot(Parameters(TabArgs { tab: a.tab })).await?;
            blocks.extend(shot.content);
        }

        if failed_index.is_some() {
            Ok(CallToolResult::error(blocks))
        } else {
            Ok(CallToolResult::success(blocks))
        }
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for LbMcp {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions(
                "Live Browser: drive the shared browser tab. Typical loop: \
                 status -> snapshot -> click/type_text -> snapshot. \
                 Use batch to run multiple sequential actions (e.g. click_at+insert, type_text, select, press, scroll) in one call instead of one round-trip per field; pass screenshot_after=true or snapshot_after=true to verify in the same call. \
                 Use wait after navigation instead of guessing load timing; \
                 use text/html to read page content; use screenshot when layout/coordinates matter. \
                 For logins/captchas use handover_request, poll handover_status, then snapshot again. \
                 Use tab_link to get a shareable UI link (/?tabId=N) that opens the UI on a specific tab. \
                 This session has its own tab(s): open navigates it in place, other agents' tabs are off limits, \
                 and when you disconnect your tabs stay open as shared tabs (e.g. for a login the user finishes later), \
                 so a later session can find them with list_tabs and keep using them. \
                 USER LOCK: the user can Lock the browser from the web UI while driving it by hand. \
                 While locked, every action that changes the browser (open, click, type, press, scroll, eval, close, …) \
                 fails with \"Browser is locked by the user\"; read-only tools (status, snapshot, screenshot, text, html, \
                 wait, list_tabs, log) still work and status reports locked: true. Do not retry in a loop: \
                 wait and check status occasionally, or ask the user to unlock. Agents cannot unlock it.",
            )
    }
}

/// Serve MCP over stdio (never returns until the client disconnects).
pub async fn run(
    server: String,
    passcode: Option<String>,
    default_tab: Option<u32>,
    session: String,
    isolated: bool,
) -> anyhow::Result<()> {
    // Unique per process so two clients with the same label never share
    // (or close) each other's tabs.
    let label: String = session
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || "-_.:".contains(*c))
        .take(40)
        .collect();
    let label = if label.is_empty() { "mcp".to_string() } else { label };
    let session_id = format!("{label}-{}", std::process::id());
    let svc = LbMcp::new(server, passcode, default_tab, session_id, isolated);
    let closer = svc.clone();
    let running = svc.serve(rmcp::transport::stdio()).await?;
    // End the session however we stop: client disconnect (stdin EOF),
    // Ctrl-C or SIGTERM — so an agent's tabs never outlive it.
    #[cfg(unix)]
    let term = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! {
        r = running.waiting() => { r?; }
        _ = tokio::signal::ctrl_c() => {}
        _ = term => {}
    }
    let _ = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        closer.post_json("/api/session/close", serde_json::json!({})),
    )
    .await;
    // Exit now: tokio's blocking stdin reader would otherwise keep the
    // process alive after a signal until stdin closes.
    std::process::exit(0)
}

#[cfg(test)]
mod screenshot_tests {
    use super::*;
    use axum::{http::StatusCode, routing::get, Router};

    async fn call(status: StatusCode, body: Vec<u8>) -> serde_json::Value {
        // Deliberately wrong header: MIME must come from validated bytes.
        let app = Router::new().route("/api/screenshot", get(move || async move {
            (status, [("content-type", "image/jpeg")], body)
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let mcp = LbMcp::new(format!("http://{addr}"), None, None, "test".into(), false);
        let result = mcp.screenshot(Parameters(TabArgs { tab: None })).await.unwrap();
        task.abort();
        serde_json::to_value(result).unwrap()
    }

    #[tokio::test]
    async fn failed_capture_is_text_tool_error_never_image() {
        for (status, body) in [
            (StatusCode::OK, b"".as_slice()),
            (StatusCode::OK, b"{\"error\":\"failed\"}"),
            (StatusCode::OK, b"<html>error</html>"),
            (StatusCode::OK, b"\xff\xd8\xffgarbage\xff\xd9"),
            (StatusCode::NOT_FOUND, b"no tab"),
            (StatusCode::UNAUTHORIZED, b"unauthorized"),
            (StatusCode::BAD_GATEWAY, b"screenshot failed: browser dead"),
            (StatusCode::GATEWAY_TIMEOUT, b"screenshot timed out"),
        ] {
            let result = call(status, body.to_vec()).await;
            assert_eq!(result["isError"], true, "{result}");
            for block in result["content"].as_array().unwrap() {
                assert_eq!(block["type"], "text");
                assert!(block["text"].as_str().unwrap().contains("Screenshot failed"));
            }
        }
    }

    #[tokio::test]
    async fn interrupted_http_body_is_a_text_tool_error() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0; 4096];
            let received = stream.read(&mut request).await.unwrap();
            assert!(received > 0);
            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1000\r\nConnection: close\r\n\r\n\xff\xd8\xff").await.unwrap();
        });
        let mcp = LbMcp::new(format!("http://{addr}"), None, None, "test".into(), false);
        let result = serde_json::to_value(mcp.screenshot(Parameters(TabArgs { tab: None })).await.unwrap()).unwrap();
        task.await.unwrap();
        assert_eq!(result["isError"], true);
        assert!(result["content"].as_array().unwrap().iter().all(|block| block["type"] == "text"));
    }

    #[tokio::test]
    async fn successful_capture_has_detected_mime_and_original_bytes() {
        for (format, mime) in [(image::ImageFormat::Png, "image/png"), (image::ImageFormat::Jpeg, "image/jpeg")] {
            let img = image::DynamicImage::ImageRgb8(image::RgbImage::new(2, 2));
            let mut bytes = std::io::Cursor::new(Vec::new());
            img.write_to(&mut bytes, format).unwrap();
            let bytes = bytes.into_inner();
            let result = call(StatusCode::OK, bytes.clone()).await;
            assert_ne!(result["isError"], true);
            assert_eq!(result["content"][0]["type"], "image");
            assert_eq!(result["content"][0]["mimeType"], mime);
            let decoded = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, result["content"][0]["data"].as_str().unwrap()).unwrap();
            assert_eq!(decoded, bytes);
        }
    }
}

#[cfg(test)]
mod batch_tests {
    use super::*;
    use axum::{
        extract::State,
        routing::{get, post},
        Json, Router,
    };
    use std::sync::{Arc, Mutex};

    fn step(action: &str) -> BatchStep {
        BatchStep {
            action: action.into(),
            target: None,
            text: None,
            x: None,
            y: None,
            coord_space: None,
            kind: None,
            value: None,
            key: None,
            dx: None,
            dy: None,
            submit: false,
            append: false,
            timeout: None,
            gone: false,
            ms: None,
            js: None,
            url: None,
            tab: None,
        }
    }

    async fn mock_server() -> (String, Arc<Mutex<Vec<String>>>, tokio::task::JoinHandle<()>) {
        let log: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let app = Router::new()
            .route(
                "/api/click-at",
                post(|State(log): State<Arc<Mutex<Vec<String>>>>, Json(b): Json<serde_json::Value>| async move {
                    log.lock().unwrap().push(format!("click_at:{},{}", b["x"], b["y"]));
                    Json(serde_json::json!({ "ok": true, "x": b["x"], "y": b["y"] }))
                }),
            )
            .route(
                "/api/insert",
                post(|State(log): State<Arc<Mutex<Vec<String>>>>, Json(b): Json<serde_json::Value>| async move {
                    log.lock().unwrap().push(format!("insert:{}", b["text"].as_str().unwrap_or("")));
                    Json(serde_json::json!({ "ok": true }))
                }),
            )
            .route(
                "/api/click",
                post(|State(log): State<Arc<Mutex<Vec<String>>>>, Json(b): Json<serde_json::Value>| async move {
                    let t = b["target"].as_str().unwrap_or("");
                    log.lock().unwrap().push(format!("click:{t}"));
                    if t == "bad" {
                        Json(serde_json::json!({ "ok": false, "error": "element not found" }))
                    } else {
                        Json(serde_json::json!({ "ok": true }))
                    }
                }),
            )
            .route(
                "/api/snapshot",
                get(|| async { Json(serde_json::json!({ "ok": true, "elements": [] })) }),
            )
            .with_state(log.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{addr}"), log, task)
    }

    #[tokio::test]
    async fn batch_executes_in_order_and_supports_snapshot_after() {
        let (base, log, task) = mock_server().await;
        let mcp = LbMcp::new(base, None, Some(2), "test".into(), false);

        let mut s1 = step("click_at");
        s1.x = Some(400.0);
        s1.y = Some(502.0);
        let mut s2 = step("insert");
        s2.text = Some("Vicuna".into());
        let mut s3 = step("click");
        s3.target = Some("12".into());

        let res = mcp
            .batch(Parameters(BatchArgs {
                actions: vec![s1, s2, s3],
                tab: None,
                stop_on_error: true,
                snapshot_after: true,
                screenshot_after: false,
            }))
            .await
            .unwrap();
        task.abort();

        let v = serde_json::to_value(&res).unwrap();
        assert_ne!(v["isError"], true);
        assert_eq!(
            *log.lock().unwrap(),
            vec!["click_at:400.0,502.0", "insert:Vicuna", "click:12"]
        );
        let body: serde_json::Value =
            serde_json::from_str(v["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(body["ok"], true);
        assert_eq!(body["completed"], 3);
        assert_eq!(body["total"], 3);
        assert_eq!(body["snapshot"]["ok"], true);
    }

    #[tokio::test]
    async fn batch_stops_on_first_error_by_default() {
        let (base, log, task) = mock_server().await;
        let mcp = LbMcp::new(base, None, None, "test".into(), false);

        let mut s1 = step("click");
        s1.target = Some("bad".into());
        let mut s2 = step("insert");
        s2.text = Some("never".into());

        let res = mcp
            .batch(Parameters(BatchArgs {
                actions: vec![s1, s2],
                tab: None,
                stop_on_error: true,
                snapshot_after: false,
                screenshot_after: false,
            }))
            .await
            .unwrap();
        task.abort();

        let v = serde_json::to_value(&res).unwrap();
        assert_eq!(v["isError"], true);
        assert_eq!(*log.lock().unwrap(), vec!["click:bad"]);
        let body: serde_json::Value =
            serde_json::from_str(v["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(body["ok"], false);
        assert_eq!(body["completed"], 1);
        assert_eq!(body["failed_index"], 0);
    }
}
