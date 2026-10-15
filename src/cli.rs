use clap::{Args, Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(name = "lb", version, about = "Live Browser — a remote browser that humans and agents control together, live")]
pub struct Cli {
    /// Daemon base URL (the `lb web` server you started)
    #[arg(long, global = true, env = "LB_SERVER")]
    pub server: Option<String>,

    /// Tab id to operate on (default: active tab). See `lb list`.
    #[arg(long, global = true, env = "LB_TAB")]
    pub tab: Option<u32>,

    /// Passcode protecting the daemon (API + UI). Same value must be given
    /// to `lb web` (to set it) and to every other command (to use it).
    /// Prefer LB_PASSCODE env var over the flag (flags leak via `ps`).
    #[arg(long, global = true, visible_alias = "password", env = "LB_PASSCODE", hide_env_values = true)]
    pub passcode: Option<String>,

    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Start the browser + web UI server (background by default, survives
    /// terminal close). With no subcommand this starts the server; see
    /// `stop`/`restart`. Use `--foreground` to keep it in the terminal.
    /// (The old `lb daemon` spelling still works as an alias.)
    #[command(visible_alias = "daemon")]
    Web(WebArgs),

    /// Open a URL in the running browser (new tab by default; --here reuses the active tab)
    Open {
        url: String,
        /// Navigate the active tab in place instead of opening a new tab
        #[arg(long)]
        here: bool,
        #[arg(long)]
        json: bool,
    },
    /// Show current URL / title / handover state
    Status {
        #[arg(long)]
        json: bool,
    },
    /// List all tabs (id, title, url)
    List {
        #[arg(long)]
        json: bool,
    },
    /// Check browser health (dead after macOS sleep?) / relaunch it in place.
    /// `lb recover` relaunches the same engine without a daemon restart
    /// (open tabs reset); `--check` only probes via /api/health.
    Recover {
        /// Only check health, don't relaunch
        #[arg(long)]
        check: bool,
        #[arg(long)]
        json: bool,
    },
    /// Close a tab (default: active). Warns: agents lose control of it.
    Close {
        tab: Option<u32>,
        #[arg(long)]
        json: bool,
    },
    /// Switch the active tab
    Use {
        tab: u32,
        #[arg(long)]
        json: bool,
    },
    /// Agent-friendly snapshot of interactive elements ([ref] tag "name")
    Snapshot {
        #[arg(long)]
        json: bool,
        /// Detailed lines (type/value/frame/coordinates) instead of compact ones
        #[arg(long)]
        full: bool,
    },
    /// Save a JPEG screenshot (also viewable live in the web UI)
    Shot {
        /// Output file (default: screenshot.jpg). Use `-` to print base64 to stdout.
        #[arg(long, default_value = "screenshot.jpg")]
        out: String,
    },
    /// Click an element by snapshot ref (e.g. `12`) or CSS (`css=.login`)
    Click {
        target: String,
        /// Double-click instead of single-click
        #[arg(long, conflicts_with = "right")]
        double: bool,
        /// Right-click (context menu) instead of left-click
        #[arg(long, conflicts_with = "double")]
        right: bool,
        #[arg(long)]
        json: bool,
    },
    /// Click at viewport coordinates (CSS pixels, e.g. from the web UI)
    ClickAt {
        x: f64,
        y: f64,
        /// Double-click instead of single-click
        #[arg(long, conflicts_with = "right")]
        double: bool,
        /// Right-click (context menu) instead of left-click
        #[arg(long, conflicts_with = "double")]
        right: bool,
        #[arg(long)]
        json: bool,
    },
    /// Move the mouse (hover: menus, tooltips, highlights).
    /// Give x y coordinates, or --on <ref|css> to hover an element.
    Hover {
        x: Option<f64>,
        y: Option<f64>,
        /// Snapshot ref (e.g. `12`) or CSS selector (`css=.menu`) to hover
        #[arg(long, conflicts_with_all = ["x", "y"])]
        on: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Scroll the page by dx/dy CSS pixels
    Scroll {
        #[arg(default_value_t = 0.0)]
        dx: f64,
        #[arg(default_value_t = 600.0)]
        dy: f64,
        #[arg(long)]
        json: bool,
    },
    /// Wheel-scroll whatever is under the point (inner containers included).
    /// Deltas can be positional (`lb wheel 400 300 500`) or flags
    /// (`lb wheel 400 300 --dy 500`); flags win when both are given.
    Wheel {
        x: f64,
        y: f64,
        #[arg(default_value_t = 300.0)]
        dy: f64,
        #[arg(default_value_t = 0.0)]
        dx: f64,
        /// Vertical wheel delta (overrides positional DY)
        #[arg(long = "dy")]
        dy_flag: Option<f64>,
        /// Horizontal wheel delta (overrides positional DX)
        #[arg(long = "dx")]
        dx_flag: Option<f64>,
        #[arg(long)]
        json: bool,
    },
    /// Mouse drag from (x1,y1) to (x2,y2) — selection, sliders, resize handles
    Drag {
        x1: f64,
        y1: f64,
        x2: f64,
        y2: f64,
        #[arg(long, default_value_t = 10)]
        steps: u32,
        #[arg(long)]
        json: bool,
    },
    /// Zoom the page (in / out / reset) — useful when the viewport is small
    Zoom {
        #[arg(default_value = "in")]
        action: String,
        #[arg(long)]
        json: bool,
    },

    /// Type text into an element by ref or CSS (clears first; --append keeps content)
    Type {
        target: String,
        text: String,
        /// Press Enter after typing
        #[arg(long)]
        submit: bool,
        /// Append to existing content instead of clearing first
        #[arg(long)]
        append: bool,
        #[arg(long)]
        json: bool,
    },
    /// Insert text at the cursor (fast paste; focus the target first)
    Insert {
        text: String,
        #[arg(long)]
        json: bool,
    },
    /// Set a dropdown (<select>) option by value or visible text
    Select {
        target: String,
        value: String,
        #[arg(long)]
        json: bool,
    },
    /// Press a key (Enter, Tab, Escape, ArrowDown, ...) or a chord (ctrl+c, ctrl+shift+t, alt+F4)
    Press {
        key: String,
        #[arg(long)]
        json: bool,
    },
    /// Wait until an element appears (or disappears with --gone) — polls server-side
    Wait {
        /// Snapshot ref (`12`), `css=` selector or plain CSS
        target: String,
        /// Max seconds to wait (default 15, max 120)
        #[arg(long, default_value_t = 15)]
        timeout: u64,
        /// Wait until the target disappears instead of appears
        #[arg(long)]
        gone: bool,
        #[arg(long)]
        json: bool,
    },
    /// Read visible text of the page (or one element) — for "read the article" tasks
    Text {
        /// Snapshot ref, `css=` selector or plain CSS (default: whole body)
        target: Option<String>,
        /// Max characters returned (default 8000)
        #[arg(long, default_value_t = 8000)]
        max: usize,
        #[arg(long)]
        json: bool,
    },
    /// Read HTML of the page (or one element) — for tables/structures snapshot can't see
    Html {
        /// Snapshot ref, `css=` selector or plain CSS (default: whole body)
        target: Option<String>,
        /// Max characters returned (default 8000)
        #[arg(long, default_value_t = 8000)]
        max: usize,
        #[arg(long)]
        json: bool,
    },
    /// Attach a local file to an <input type=file> (path is resolved on the `lb web` machine)
    Upload {
        /// Snapshot ref or CSS selector of the file input
        target: String,
        /// Local file path to attach
        file: String,
        #[arg(long)]
        json: bool,
    },
    /// List files downloaded through the browser (newest first)
    Downloads {
        #[arg(long)]
        json: bool,
    },
    /// Evaluate arbitrary JS in the page (returns JSON-ified result)
    Eval {
        js: String,
        #[arg(long)]
        json: bool,
    },
    Back {
        #[arg(long)]
        json: bool,
    },
    Forward {
        #[arg(long)]
        json: bool,
    },
    Reload {
        #[arg(long)]
        json: bool,
    },
    /// Ask a human to take over (login, captcha, 2FA...). Blocks until the
    /// human clicks "Done" in the web UI (or timeout secs, 0 = forever).
    Handover {
        #[arg(default_value = "Human help needed: please take over in the browser window, then click Done in the web UI.")]
        message: String,
        #[arg(long, default_value_t = 0)]
        timeout: u64,
    },
    /// Signal handover is done (used by human scripts / testing)
    HandoverDone {
        #[arg(long)]
        json: bool,
    },
    /// Claim control as a human (same as the "Take control" button in the web UI)
    Claim {
        #[arg(default_value = "Human claimed control from the CLI.")]
        message: String,
        #[arg(long)]
        json: bool,
    },
    /// Show recent action log
    Log {
        #[arg(long)]
        json: bool,
    },
    /// Start an MCP server on stdio (for opencode / Claude Code / etc.).
    /// Reads LB_SERVER / LB_PASSCODE like every other command.
    Mcp,
    /// Update lb to the latest release (says so when already current)
    Update {
        /// Reinstall the latest release even when already current
        #[arg(long)]
        force: bool,
    },
}

#[derive(Args, Debug)]
pub struct WebArgs {
    #[command(subcommand)]
    pub action: Option<WebAction>,
    #[command(flatten)]
    pub daemon: DaemonArgs,
}

#[derive(Subcommand, Debug)]
pub enum WebAction {
    /// Stop the running server (`--port` selects it: `lb web --port 9000 stop`)
    Stop,
    /// Stop the running server (if any) and start a new one
    /// (background by default, like a plain start; `--foreground` keeps it
    /// in the terminal instead).
    Restart,
    /// Print a shareable web UI link for a tab (`/?tabId=N`).
    /// Opening it focuses that tab in the UI.
    Link {
        /// Tab id to link to (default: active tab; also honors global --tab / LB_TAB)
        tab: Option<u32>,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Args, Debug)]
pub struct DaemonArgs {
    /// HTTP port for API + Svelte UI
    #[arg(long, env = "LB_PORT", default_value_t = 8765)]
    pub port: u16,
    /// Chrome executable (defaults to auto-detect; the last choice —
    /// this flag or Settings → Browser — is remembered for the next start)
    #[arg(long, env = "LB_CHROME")]
    pub chrome: Option<String>,
    /// Run headless: no visible browser window (this is the default).
    /// Kept for explicitness; see --headed for the opposite.
    #[arg(long, overrides_with = "headed")]
    pub headless: bool,
    /// Show the browser window so a human can watch and take over.
    /// Overrides --headless.
    #[arg(long, visible_alias = "no-headless", overrides_with = "headless")]
    pub headed: bool,
    /// Persistent profile dir (keeps logins/cookies across restarts)
    #[arg(long, env = "LB_PROFILE")]
    pub profile: Option<String>,
    /// Pass --no-sandbox to Chrome (needed in some containers)
    #[arg(long, default_value_t = false)]
    pub no_sandbox: bool,
    /// URL to open on start
    #[arg(long)]
    pub url: Option<String>,
    /// Run in the background (this is the default): detach, survive terminal
    /// close, log to `daemon-<port>.log`. Explicit alias for the default
    /// behavior — useful in scripts for clarity. Overrides `--foreground`.
    #[arg(long, visible_alias = "detach", overrides_with = "foreground")]
    pub background: bool,
    /// Keep the server in the foreground (logs to the terminal) instead of
    /// detaching. Needed under supervisors that already background for you
    /// (e.g. systemd `Type=simple` services). Overrides `--background`.
    #[arg(long, overrides_with = "background")]
    pub foreground: bool,
    /// Don't auto-open the web UI in the default browser on start
    #[arg(long)]
    pub no_open: bool,
}

impl DaemonArgs {
    /// Headless unless the user asked for a visible window with --headed.
    /// (`--headless` is the default and only exists to override `--headed`.)
    pub fn headless_mode(&self) -> bool {
        !self.headed || self.headless
    }

    /// Background unless the user asked for `--foreground`.
    /// (`--background` is the default and only exists to override
    /// `--foreground`, mirroring the `--headless` / `--headed` pair.)
    pub fn background_mode(&self) -> bool {
        !self.foreground || self.background
    }
}
