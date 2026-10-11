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
    http: reqwest::Client,
    tool_router: ToolRouter<Self>,
}

impl LbMcp {
    pub fn new(server: String, passcode: Option<String>, default_tab: Option<u32>) -> Self {
        Self {
            server,
            passcode,
            default_tab,
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
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ClickArgs {
    /// Snapshot ref (e.g. "12") or CSS selector ("css=.login").
    pub target: String,
    #[serde(default)]
    pub tab: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct PointArgs {
    /// Viewport X in CSS pixels.
    pub x: f64,
    /// Viewport Y in CSS pixels.
    pub y: f64,
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
pub struct TypeArgs {
    /// Snapshot ref (e.g. "3") or CSS selector.
    pub target: String,
    /// Text to type.
    pub text: String,
    /// Press Enter after typing (default false).
    #[serde(default)]
    pub submit: bool,
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
    /// Key name: Enter, Tab, Escape, ArrowDown, ...
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

// ---------------------------------------------------------------------------
// Tools (one per CLI command; back/forward/reload share `navigate`)
// ---------------------------------------------------------------------------

#[tool_router]
impl LbMcp {
    #[tool(description = "Navigate the tab to a URL (plain text searches Google).")]
    async fn open(
        &self,
        Parameters(a): Parameters<OpenArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let v = self
            .post_json(
                "/api/open",
                serde_json::json!({ "url": a.url, "tab": self.tab(a.tab) }),
            )
            .await?;
        Ok(Self::daemon_result(v))
    }

    #[tool(description = "Current URL, title and handover state of a tab.")]
    async fn status(
        &self,
        Parameters(a): Parameters<TabArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let v = self
            .get_json(&format!("/api/status{}", self.qs(a.tab)))
            .await?;
        Ok(Self::daemon_result(v))
    }

    #[tool(description = "List all tabs (id, title, url, active).")]
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
        description = "Screenshot of the current page viewport. Returns an image — call after navigation or when the snapshot is ambiguous."
    )]
    async fn screenshot(
        &self,
        Parameters(a): Parameters<TabArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let url = self.api(&format!("/api/screenshot{}", self.qs(a.tab)));
        let res = self
            .auth(self.http.get(&url))
            .send()
            .await
            .map_err(|e| self.unreachable(e))?;
        if res.status() == reqwest::StatusCode::UNAUTHORIZED {
            return Err(ErrorData::internal_error(
                "unauthorized — wrong or missing passcode (set LB_PASSCODE)",
                None,
            ));
        }
        let bytes = res
            .bytes()
            .await
            .map_err(|e| ErrorData::internal_error(format!("screenshot failed: {e:#}"), None))?;
        // A JSON error body is not a screenshot.
        if bytes.starts_with(b"{") {
            if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&bytes) {
                if v.get("ok").and_then(|x| x.as_bool()) == Some(false) {
                    return Ok(Self::daemon_result(v));
                }
            }
        }
        let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &bytes);
        Ok(CallToolResult::success(vec![
            ContentBlock::image(b64, "image/png"),
            ContentBlock::text(format!("screenshot ({} bytes)", bytes.len())),
        ]))
    }

    #[tool(
        description = "Click an element by snapshot ref (e.g. \"12\") or CSS selector (\"css=.login\")."
    )]
    async fn click(
        &self,
        Parameters(a): Parameters<ClickArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let v = self
            .post_json(
                "/api/click",
                serde_json::json!({ "target": a.target, "tab": self.tab(a.tab) }),
            )
            .await?;
        Ok(Self::daemon_result(v))
    }

    #[tool(description = "Click at viewport coordinates in CSS pixels.")]
    async fn click_at(
        &self,
        Parameters(a): Parameters<PointArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let v = self
            .post_json(
                "/api/click-at",
                serde_json::json!({ "x": a.x, "y": a.y, "tab": self.tab(a.tab) }),
            )
            .await?;
        Ok(Self::daemon_result(v))
    }

    #[tool(description = "Move the mouse to viewport coordinates (hover states, tooltips, menus).")]
    async fn hover(
        &self,
        Parameters(a): Parameters<PointArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let v = self
            .post_json(
                "/api/hover",
                serde_json::json!({ "x": a.x, "y": a.y, "tab": self.tab(a.tab) }),
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

    #[tool(
        description = "Type text into an element by snapshot ref or CSS. Set submit=true to press Enter afterwards."
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
                    "submit": a.submit, "tab": self.tab(a.tab),
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

    #[tool(description = "Press a key: Enter, Tab, Escape, ArrowDown, ...")]
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

    #[tool(description = "Switch the active tab (see list_tabs for ids).")]
    async fn activate_tab(
        &self,
        Parameters(a): Parameters<ActivateArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let v = self
            .post_json("/api/activate", serde_json::json!({ "tab": a.tab }))
            .await?;
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
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for LbMcp {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions(
                "Live Browser: drive the shared browser tab. Typical loop: \
                 status -> snapshot -> click/type_text -> snapshot. \
                 Use screenshot when layout/coordinates matter. \
                 For logins/captchas use handover_request, poll handover_status, then snapshot again.",
            )
    }
}

/// Serve MCP over stdio (never returns until the client disconnects).
pub async fn run(
    server: String,
    passcode: Option<String>,
    default_tab: Option<u32>,
) -> anyhow::Result<()> {
    let svc = LbMcp::new(server, passcode, default_tab);
    let running = svc.serve(rmcp::transport::stdio()).await?;
    running.waiting().await?;
    Ok(())
}
