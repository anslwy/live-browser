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
    /// Start the browser + web UI server (foreground, keep alive).
    /// With no subcommand this starts the server; see `stop`/`restart`.
    /// (The old `lb daemon` spelling still works as an alias.)
    #[command(visible_alias = "daemon")]
    Web(WebArgs),

    /// Open a URL in the running browser
    Open {
        url: String,
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
        /// Include href/value details (default prints compact lines)
        #[arg(long)]
        full: bool,
    },
    /// Save a PNG screenshot (also viewable live in the web UI)
    Shot {
        /// Output file (default: screenshot.png). Use `-` to print base64 to stdout.
        #[arg(long, default_value = "screenshot.png")]
        out: String,
    },
    /// Click an element by snapshot ref (e.g. `12`) or CSS (`css=.login`)
    Click {
        target: String,
        #[arg(long)]
        json: bool,
    },
    /// Click at viewport coordinates (CSS pixels, e.g. from the web UI)
    ClickAt {
        x: f64,
        y: f64,
        #[arg(long)]
        json: bool,
    },
    /// Move the mouse (hover: menus, tooltips, highlights)
    Hover {
        x: f64,
        y: f64,
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
    /// Wheel-scroll whatever is under the point (inner containers included)
    Wheel {
        x: f64,
        y: f64,
        #[arg(default_value_t = 300.0)]
        dy: f64,
        #[arg(default_value_t = 0.0)]
        dx: f64,
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

    /// Type text into an element by ref or CSS
    Type {
        target: String,
        text: String,
        /// Press Enter after typing
        #[arg(long)]
        submit: bool,
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
    /// Press a key (Enter, Tab, Escape, ArrowDown, ...)
    Press {
        key: String,
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
    /// Stop the running server (if any) and start a new one in the foreground
    Restart,
}

#[derive(Args, Debug)]
pub struct DaemonArgs {
    /// HTTP port for API + Svelte UI
    #[arg(long, env = "LB_PORT", default_value_t = 8765)]
    pub port: u16,
    /// Chrome executable (defaults to auto-detect)
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
}
