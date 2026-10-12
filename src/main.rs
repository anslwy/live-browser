mod cli;
mod client;
mod daemon;
mod mcp;
mod update;
mod webctl;

use anyhow::Result;
use cli::{Cli, Commands, WebAction};

#[tokio::main]
async fn main() -> Result<()> {
    let cli = <Cli as clap::Parser>::parse();
    // MCP speaks JSON-RPC on stdout — logs must go to stderr there,
    // otherwise the protocol stream gets corrupted.
    let log_writer_is_stderr = matches!(cli.command, Commands::Mcp);
    if log_writer_is_stderr {
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| "lb=info,live_browser=info".into()),
            )
            .with_target(false)
            .compact()
            .with_writer(std::io::stderr)
            .init();
    } else {
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    // NOTE: the binary crate is named `lb` ([[bin]] in Cargo.toml),
                    // so `live_browser=…` alone would silence every tracing event.
                    .unwrap_or_else(|_| "lb=info,live_browser=info".into()),
            )
            .with_target(false)
            .compact()
            .init();
    }
    // Server precedence: --server flag > LB_SERVER > SB_SERVER (legacy) > default.
    let server = cli
        .server
        .clone()
        .or_else(|| std::env::var("SB_SERVER").ok())
        .unwrap_or_else(|| "http://127.0.0.1:8765".to_string());
    let tab = cli.tab;
    // Passcode precedence: --passcode flag > LB_PASSCODE > LB_PASSWORD (compat) > none.
    let passcode = cli
        .passcode
        .clone()
        .or_else(|| std::env::var("LB_PASSWORD").ok())
        .filter(|s| !s.is_empty());
    let pass = passcode.as_deref();

    match cli.command {
        Commands::Web(w) => match w.action {
            None => {
                webctl::ensure_not_running(w.daemon.port)?;
                update::spawn_update_reminder();
                daemon::run_daemon(w.daemon, passcode).await
            }
            Some(WebAction::Stop) => {
                println!("{}", webctl::stop(w.daemon.port, w.daemon.profile.as_deref())?);
                Ok(())
            }
            Some(WebAction::Restart) => {
                match webctl::stop(w.daemon.port, w.daemon.profile.as_deref()) {
                    Ok(msg) => println!("{msg}"),
                    Err(e) => println!("stop skipped: {e:#}"),
                }
                webctl::ensure_not_running(w.daemon.port)?;
                update::spawn_update_reminder();
                daemon::run_daemon(w.daemon, passcode).await
            }
            Some(WebAction::Link { tab: t, json }) => {
                // Base URL precedence: explicit --server / LB_SERVER / SB_SERVER
                // wins; otherwise derive from --port so
                // `lb web --port 9000 link 2` links to :9000, not the default :8765.
                let explicit = cli.server.is_some()
                    || std::env::var("LB_SERVER")
                        .ok()
                        .is_some_and(|s| !s.trim().is_empty())
                    || std::env::var("SB_SERVER")
                        .ok()
                        .is_some_and(|s| !s.trim().is_empty());
                let base = if explicit {
                    server.clone()
                } else {
                    format!("http://127.0.0.1:{}", w.daemon.port)
                };
                client::link(&base, pass, t.or(tab), json).await
            }
        },
        Commands::Update { force } => update::update(force).await,
        Commands::Open { url, here, json } => client::open(&server, pass, &url, tab, here, json).await,
        Commands::Status { json } => client::status(&server, pass, tab, json).await,
        Commands::List { json } => client::list(&server, pass, json).await,
        Commands::Close { tab: t, json } => client::close(&server, pass, t.or(tab), json).await,
        Commands::Use { tab: t, json } => client::use_tab(&server, pass, t, json).await,
        Commands::Snapshot { json, full } => client::snapshot(&server, pass, tab, json, full).await,
        Commands::Shot { out } => client::shot(&server, pass, tab, Some(out.as_str())).await,
        Commands::Click { target, double, right, json } => {
            let kind = if double { "double" } else if right { "right" } else { "single" };
            client::click(&server, pass, &target, kind, tab, json).await
        }
        Commands::ClickAt { x, y, double, right, json } => {
            let kind = if double { "double" } else if right { "right" } else { "single" };
            client::click_at(&server, pass, x, y, kind, tab, json).await
        }
        Commands::Hover { x, y, on, json } => client::hover(&server, pass, x, y, on.as_deref(), tab, json).await,
        Commands::Scroll { dx, dy, json } => client::scroll(&server, pass, dx, dy, tab, json).await,
        Commands::Wheel { x, y, dy, dx, dy_flag, dx_flag, json } => {
            client::wheel(&server, pass, x, y, dx_flag.unwrap_or(dx), dy_flag.unwrap_or(dy), tab, json).await
        }
        Commands::Drag { x1, y1, x2, y2, steps, json } => {
            client::drag(&server, pass, x1, y1, x2, y2, steps, tab, json).await
        }
        Commands::Zoom { action, json } => client::zoom(&server, pass, &action, tab, json).await,
        Commands::Type {
            target,
            text,
            submit,
            append,
            json,
        } => client::type_text(&server, pass, &target, &text, submit, append, tab, json).await,
        Commands::Press { key, json } => client::press(&server, pass, &key, tab, json).await,
        Commands::Select { target, value, json } => {
            client::select(&server, pass, &target, &value, tab, json).await
        }
        Commands::Insert { text, json } => client::insert(&server, pass, &text, tab, json).await,
        Commands::Eval { js, json } => client::eval(&server, pass, &js, tab, json).await,
        Commands::Back { json } => client::nav(&server, pass, "back", tab, json).await,
        Commands::Forward { json } => client::nav(&server, pass, "forward", tab, json).await,
        Commands::Reload { json } => client::nav(&server, pass, "reload", tab, json).await,
        Commands::Handover { message, timeout } => {
            client::handover(&server, pass, &message, tab, timeout).await
        }
        Commands::HandoverDone { json } => client::handover_done(&server, pass, tab, json).await,
        Commands::Claim { message, json } => client::claim(&server, pass, &message, tab, json).await,
        Commands::Log { json } => client::log(&server, pass, json).await,
        Commands::Wait { target, timeout, gone, json } => {
            client::wait(&server, pass, &target, timeout, gone, tab, json).await
        }
        Commands::Text { target, max, json } => {
            client::content(&server, pass, "text", target.as_deref(), max, tab, json).await
        }
        Commands::Html { target, max, json } => {
            client::content(&server, pass, "html", target.as_deref(), max, tab, json).await
        }
        Commands::Upload { target, file, json } => {
            client::upload(&server, pass, &target, &file, tab, json).await
        }
        Commands::Downloads { json } => client::downloads(&server, pass, json).await,
        Commands::Mcp => mcp::run(server, passcode, tab).await,
    }
}
