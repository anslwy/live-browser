//! `lb web` lifecycle: `stop` / `restart` find the running daemon via its
//! pidfile and terminate it; a plain start backgrounds by default (detach,
//! survive terminal close) unless `--foreground` is given. No new
//! dependencies — `kill(1)` on Unix, `taskkill` / `tasklist` on Windows,
//! `setsid`/`nohup` for detaching on Unix.
use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::cli::DaemonArgs;

/// Open `url` in the OS default browser. Best-effort: headless servers,
/// containers and SSH sessions have no browser to open — that must never
/// fail startup. Called only from interactive contexts (terminal stdout);
/// the detached daemon itself never pops a window (see callers).
pub(crate) fn open_in_default_browser(url: &str) {
    let attempt = if cfg!(target_os = "macos") {
        std::process::Command::new("open").arg(url).status()
    } else if cfg!(windows) {
        // `start` is a cmd builtin; the empty title arg keeps quoted URLs working.
        // CREATE_NO_WINDOW: spawning cmd must not flash a console window.
        #[cfg(windows)]
        use std::os::windows::process::CommandExt;
        // `mut` is only needed on Windows (creation_flags below); the allow
        // keeps non-Windows clippy (`-D warnings`) quiet.
        #[allow(unused_mut)]
        let mut cmd = std::process::Command::new("cmd");
        cmd.args(["/c", "start", "", url]);
        #[cfg(windows)]
        cmd.creation_flags(0x08000000);
        cmd.status()
    } else if let Ok(browser) = std::env::var("BROWSER") {
        // Respect $BROWSER when set (common on Linux).
        let mut parts = browser.split_whitespace();
        match parts.next() {
            Some(bin) if !bin.is_empty() => {
                let mut cmd = std::process::Command::new(bin);
                cmd.args(parts).arg(url);
                cmd.status()
            }
            _ => std::process::Command::new("xdg-open").arg(url).status(),
        }
    } else {
        std::process::Command::new("xdg-open").arg(url).status()
    };
    match attempt {
        Ok(s) if s.success() => tracing::info!("opened web UI in default browser"),
        Ok(s) => tracing::warn!("default browser exited with {s} — open the UI manually"),
        Err(e) => tracing::warn!("could not open a browser ({e:#}) — open the UI manually"),
    }
}

fn pid_file(port: u16) -> PathBuf {
    crate::daemon::pid_file(port)
}

fn log_file(port: u16) -> PathBuf {
    crate::daemon::daemon_log_file(port)
}

fn read_pid(port: u16) -> Result<Option<u32>> {
    let file = pid_file(port);
    let text = match std::fs::read_to_string(&file) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).context(format!("cannot read {}", file.display())),
    };
    let pid: u32 = text
        .trim()
        .parse()
        .context(format!("pid file {} is corrupt", file.display()))?;
    Ok(Some(pid))
}

/// True when a process with this pid exists (and we can signal it).
fn process_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        // `kill -0` performs no kill; it only checks existence/permission.
        Command::new("kill")
            .arg("-0")
            .arg(pid.to_string())
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }
    #[cfg(windows)]
    {
        // `tasklist` prints an INFO line (no `.exe`) when nothing matches.
        Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
            .output()
            .map(|o| {
                o.status.success()
                    && o.stdout
                        .windows(b".exe".len())
                        .any(|w| w == b".exe")
            })
            .unwrap_or(false)
    }
}

fn terminate(pid: u32, force: bool) -> Result<()> {
    #[cfg(unix)]
    {
        let mut cmd = Command::new("kill");
        if force {
            cmd.arg("-9");
        }
        // `.output()` (not `.status()`): procps `kill` complains on stderr
        // for already-exited pids, and that must not leak to our terminal.
        let out = cmd
            .arg(pid.to_string())
            .output()
            .context("failed to run `kill`")?;
        if out.status.success() {
            Ok(())
        } else {
            anyhow::bail!("`kill` reported failure for pid {pid}")
        }
    }
    #[cfg(windows)]
    {
        let mut cmd = Command::new("taskkill");
        cmd.args(["/PID", &pid.to_string()]);
        if force {
            cmd.arg("/F");
        }
        let out = cmd.output().context("failed to run `taskkill`")?;
        if out.status.success() {
            Ok(())
        } else {
            anyhow::bail!(
                "taskkill failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )
        }
    }
}

fn remove_pid_file(port: u16) {
    std::fs::remove_file(pid_file(port)).ok();
}

/// The profile dir the target daemon uses (explicit `--profile`, else the
/// default). Needed to reap orphaned browser processes below.
fn resolve_profile(profile: Option<&str>) -> String {
    profile
        .map(|s| s.to_string())
        .unwrap_or_else(crate::daemon::default_profile_dir)
}

/// Pids whose command line shows them as a browser using exactly `profile`
/// as its `--user-data-dir` — never our own process.
///
/// Matched on the exact `--user-data-dir=<profile>` (or two-token) argument,
/// not a substring: the default profile `…/live-browser/profile` is a string
/// prefix of a sibling daemon's `…/live-browser/profile-serve`, and a
/// substring match would SIGKILL the wrong daemon's browser on `lb web stop`.
///
/// Shared with the daemon: the Settings → Browser switch pre-flights the
/// target profile with it (a live holder aborts the new launch with a
/// SingletonLock error — better refused up front with the current browser
/// untouched than after it has been closed).
pub(crate) fn profile_browser_pids(profile: &str) -> Vec<u32> {
    let me = std::process::id();
    process_list()
        .into_iter()
        .filter(|(pid, args)| {
            *pid != me
                && {
                    let lower = args.to_lowercase();
                    (lower.contains("chrome")
                        || lower.contains("chromium")
                        || lower.contains("headless_shell"))
                        && user_data_dir(args) == Some(profile.to_string())
                }
        })
        .map(|(pid, _)| pid)
        .collect()
}

/// Value of the `--user-data-dir` argument in a process command line, if any
/// (handles both `--user-data-dir=X` and `--user-data-dir X` spellings).
fn user_data_dir(args: &str) -> Option<String> {
    let mut tokens = args.split_whitespace().peekable();
    while let Some(tok) = tokens.next() {
        if let Some(v) = tok.strip_prefix("--user-data-dir=") {
            let v = v.trim_matches('"').trim_matches('\'');
            if !v.is_empty() {
                return Some(v.to_string());
            }
        } else if tok == "--user-data-dir" {
            if let Some(next) = tokens.next() {
                let v = next.trim_matches('"').trim_matches('\'');
                if !v.is_empty() {
                    return Some(v.to_string());
                }
            }
        }
    }
    None
}

/// All processes as (pid, full command line).
fn process_list() -> Vec<(u32, String)> {
    #[cfg(unix)]
    {
        let out = Command::new("ps").args(["-eo", "pid,args"]).output();
        let out = match out {
            Ok(o) if o.status.success() => o,
            _ => return vec![],
        };
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .skip(1) // header
            .filter_map(|line| {
                let line = line.trim_start();
                let (pid, args) = line.split_once(char::is_whitespace)?;
                Some((pid.parse().ok()?, args.trim_start().to_string()))
            })
            .collect()
    }
    #[cfg(windows)]
    {
        // CommandLine can be null (system processes) — filter those out in
        // PowerShell and emit "<pid> <cmdline>" lines.
        let script = "Get-CimInstance Win32_Process | Where-Object { $_.CommandLine } | ForEach-Object { \\\"$($_.ProcessId) $($_.CommandLine)\\\" }";
        let out = Command::new("powershell")
            .args(["-NoProfile", "-Command", script])
            .output();
        let out = match out {
            Ok(o) if o.status.success() => o,
            _ => return vec![],
        };
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|line| {
                let line = line.trim();
                let (pid, args) = line.split_once(char::is_whitespace)?;
                Some((pid.parse().ok()?, args.trim_start().to_string()))
            })
            .collect()
    }
}

/// SIGKILL leftover browser processes holding `profile` (e.g. after a crash
/// or `kill -9` of the daemon). Returns how many were killed.
fn reap_profile_browsers(profile: &str) -> usize {
    let mut killed = 0;
    for pid in profile_browser_pids(profile) {
        if terminate(pid, true).is_ok() && wait_gone(pid, Duration::from_secs(3)) {
            killed += 1;
        }
    }
    killed
}

/// Both profile dirs one daemon may own: the base and its `-cloak` sibling
/// (per-kind profiles — see [`crate::daemon::kind_profile`]). Cleanup must
/// cover both: after a cloak switch, orphans on the sibling otherwise
/// survive `stop` and wedge the next cloak launch/switch with a
/// SingletonLock abort.
fn owned_profiles(base: &str) -> Vec<String> {
    let sibling = crate::daemon::kind_profile(base, "cloak");
    if sibling == base {
        vec![base.to_string()]
    } else {
        vec![base.to_string(), sibling]
    }
}

/// SIGKILL leftover browsers holding any profile this daemon may own (base
/// and `-cloak` sibling). Returns how many were killed.
fn reap_owned_browsers(base: &str) -> usize {
    owned_profiles(base).iter().map(|p| reap_profile_browsers(p)).sum()
}

/// Wait (up to `timeout`) for the process to disappear. Returns true when gone.
fn wait_gone(pid: u32, timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if !process_alive(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    !process_alive(pid)
}

/// Another recorded daemon (any port) that is currently alive, if any.
/// Reaping profile browsers is skipped while one exists: it may own them.
fn other_live_daemon(except_port: u16) -> Option<(u16, u32)> {
    let base = crate::daemon::cache_base().join("live-browser");
    let entries = std::fs::read_dir(&base).ok()?;
    let mut found: Vec<(u16, u32)> = vec![];
    for entry in entries.filter_map(|e| e.ok()) {
        let name = entry.file_name().to_string_lossy().to_string();
        let port: u16 = name
            .strip_prefix("daemon-")?
            .strip_suffix(".pid")?
            .parse()
            .ok()?;
        if port == except_port {
            continue;
        }
        let pid: u32 = std::fs::read_to_string(entry.path())
            .ok()?
            .trim()
            .parse()
            .ok()?;
        if process_alive(pid) {
            found.push((port, pid));
        }
    }
    found.into_iter().next()
}

/// True when something accepts TCP connections on 127.0.0.1:`port`
/// (a daemon that never wrote a pidfile, e.g. an older lb).
fn port_serves(port: u16) -> bool {
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
    std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(500)).is_ok()
}

/// Pid of the process listening on 127.0.0.1:`port`, if discoverable.
/// Used only to name the process in messages — never killed blindly.
fn listener_pid(port: u16) -> Option<u32> {
    #[cfg(unix)]
    {
        // `lsof` ships by default on macOS and most Linux distros.
        let out = Command::new("lsof")
            .args(["-ti", &format!(":{port}")])
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .next()?
            .trim()
            .parse()
            .ok()
    }
    #[cfg(windows)]
    {
        let out = Command::new("cmd")
            .args(["/c", "netstat -ano | findstr LISTENING"])
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|l| l.contains(&format!(":{port}")))
            .filter_map(|l| l.split_whitespace().last()?.parse().ok())
            .next()
    }
}

/// Reap orphaned profile browsers only when no live daemon could own them.
fn reap_if_safe(port: u16, profile: &str) -> String {
    if let Some((other_port, other_pid)) = other_live_daemon(port) {
        return format!(
            " (another daemon is running: pid {other_pid} on port {other_port} — left its browser alone)"
        );
    }
    let reaped = reap_owned_browsers(profile);
    if reaped > 0 {
        format!("; freed profile by stopping {reaped} leftover browser process(es)")
    } else {
        String::new()
    }
}

/// Stop the daemon serving `port`. Idempotent: a missing or stale pidfile
/// is reported, not an error.
///
/// Orphaned browsers (crashes, SIGKILLs, pre-pidfile daemons) holding the
/// profile lock are reaped — but never while a live daemon could own them.
pub fn stop(port: u16, profile: Option<&str>) -> Result<String> {
    let profile = resolve_profile(profile);
    match read_pid(port)? {
        // Case 1: our daemon. Kill it, then clean up its leftovers —
        // we just ended it, so nothing else can own them.
        Some(pid) if process_alive(pid) => {
            terminate(pid, false)?;
            let mut note = String::new();
            if !wait_gone(pid, Duration::from_secs(5)) {
                terminate(pid, true)?;
                if !wait_gone(pid, Duration::from_secs(3)) {
                    anyhow::bail!("pid {pid} would not die — kill it manually and remove {}", pid_file(port).display());
                }
                note = " (had to force)".to_string();
            }
            remove_pid_file(port);
            let reaped = reap_owned_browsers(&profile);
            let extra = if reaped > 0 {
                format!(" (+{reaped} leftover browser process(es))")
            } else {
                String::new()
            };
            Ok(format!("stopped daemon on port {port} (pid {pid}{note}){extra}"))
        }
        // Case 2: stale pidfile — that daemon is dead.
        Some(pid) => {
            remove_pid_file(port);
            let mut msg =
                format!("daemon for port {port} was not running (stale pid {pid} cleaned up)");
            if port_serves(port) {
                msg += &format!(
                    " — but something still serves :{port} (a daemon without a pidfile?) — left it alone"
                );
            } else {
                msg += &reap_if_safe(port, &profile);
            }
            Ok(msg)
        }
        // Case 3: no pidfile at all.
        None => {
            if port_serves(port) {
                let hint = match listener_pid(port) {
                    Some(pid) => format!(
                        " — if that is an old lb daemon, stop it with `kill {pid}`, then retry"
                    ),
                    None => String::new(),
                };
                Ok(format!(
                    "something is serving port {port} but left no pid record \
                     (started by an older lb?) — not touching it{hint}"
                ))
            } else {
                let extra = reap_if_safe(port, &profile);
                Ok(format!("no daemon recorded for port {port}{extra} — nothing to stop"))
            }
        }
    }
}

/// Refuse to start when a live daemon already owns `port`.
pub fn ensure_not_running(port: u16) -> Result<()> {
    match read_pid(port)? {
        Some(pid) if process_alive(pid) => anyhow::bail!(
            "a daemon is already running (pid {pid}, port {port}) — `lb web --port {port} stop` first, or use `lb web restart`"
        ),
        Some(_) => {
            // Stale pidfile from a crash — clear it so this start owns the port.
            remove_pid_file(port);
            Ok(())
        }
        None => Ok(()),
    }
}

// ---------------------------------------------------------------------------
// Background start (`lb web`, the default): detach so the server survives
// the terminal closing, reuse an already-running server instead of starting
// a second one.
// ---------------------------------------------------------------------------

/// Argv tokens that select start mode. The detached child is the server
/// itself and must run in the foreground (blocking) — otherwise it would
/// re-detach forever. So the child gets these stripped and `--foreground`
/// appended (see [`child_argv_foreground`]).
fn is_start_mode_flag(arg: &str) -> bool {
    matches!(arg, "--background" | "--detach" | "--foreground")
        || arg.starts_with("--background=")
        || arg.starts_with("--detach=")
        || arg.starts_with("--foreground=")
}

/// Parent argv (minus the binary) rewritten for the detached child: start-mode
/// flags stripped, `--foreground` forced so the child is the blocking server.
fn child_argv_foreground() -> Vec<String> {
    let mut out: Vec<String> = std::env::args()
        .skip(1)
        .filter(|a| !is_start_mode_flag(a))
        .collect();
    out.push("--foreground".to_string());
    out
}

/// Try to spawn `exe + args` detached (new session on Unix so terminal close
/// delivers no SIGHUP; detached process group on Windows). Stdout/stderr go
/// to `log_path`, stdin comes from null. Returns the child's pid.
#[cfg(unix)]
fn spawn_detached(exe: &std::path::Path, args: &[String], log_path: &std::path::Path) -> Result<u32> {
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let header = format!(
        "\n=== lb web (background) starting {} — {} ===\n",
        chrono_like_now(),
        args.join(" ")
    );
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
        .and_then(|mut f| {
            use std::io::Write;
            f.write_all(header.as_bytes()).map(|_| ())
        })
        .ok();
    let open_log = |what: &str| {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_path)
            .context(format!("cannot open {what} log {}", log_path.display()))
    };
    // `setsid prog args…`: setsid creates the new session, then execs prog
    // (child pid == server pid — good for status messages).
    match Command::new("setsid")
        .arg(exe)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::from(open_log("stdout")?))
        .stderr(Stdio::from(open_log("stderr")?))
        .spawn()
    {
        Ok(child) => {
            let pid = child.id();
            std::mem::forget(child); // orphan: keep running after we exit
            Ok(pid)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // No setsid (e.g. macOS): try nohup, else plain spawn.
            // (`nohup` also redirects stdout to nohup.out when it is a tty —
            // we pass explicit files, so output always lands in our log.)
            match Command::new("nohup")
                .arg(exe)
                .args(args)
                .stdin(Stdio::null())
                .stdout(Stdio::from(open_log("stdout")?))
                .stderr(Stdio::from(open_log("stderr")?))
                .spawn()
            {
                Ok(child) => {
                    let pid = child.id();
                    std::mem::forget(child);
                    Ok(pid)
                }
                Err(e2) if e2.kind() == std::io::ErrorKind::NotFound => {
                    let child = Command::new(exe)
                        .args(args)
                        .stdin(Stdio::null())
                        .stdout(Stdio::from(open_log("stdout")?))
                        .stderr(Stdio::from(open_log("stderr")?))
                        .spawn()
                        .context("failed to spawn detached daemon")?;
                    let pid = child.id();
                    std::mem::forget(child);
                    Ok(pid)
                }
                Err(e2) => Err(e2).context("failed to spawn detached daemon via nohup"),
            }
        }
        Err(e) => Err(e).context("failed to spawn detached daemon via setsid"),
    }
}

/// Best-effort timestamp for the background log header (no chrono here —
/// webctl stays dependency-light; the daemon itself logs proper timestamps).
#[cfg(unix)]
fn chrono_like_now() -> String {
    // `date` is ubiquitous on Unix; fall back to a plain marker.
    Command::new("date")
        .args(["-u", "+%Y-%m-%dT%H:%M:%SZ"])
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
            } else {
                None
            }
        })
        .unwrap_or_else(|| "now".to_string())
}

/// Try to spawn `exe + args` detached on Windows (no SIGHUP concept; a
/// detached process group outlives the parent console). Logs to `log_path`.
#[cfg(windows)]
fn spawn_detached(
    exe: &std::path::Path,
    args: &[String],
    log_path: &std::path::Path,
) -> Result<u32> {
    use std::fs::OpenOptions;
    use std::os::windows::process::CommandExt;
    // DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP: survive console close.
    const DETACHED: u32 = 0x08000000 | 0x00000200;
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let log_out = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
        .context(format!("cannot open log {}", log_path.display()))?;
    let log_err = log_out.try_clone().context("cannot clone log handle")?;
    let mut child = Command::new(exe)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log_out))
        .stderr(Stdio::from(log_err))
        .creation_flags(DETACHED)
        .spawn()
        .context("failed to spawn detached daemon")?;
    let pid = child.id();
    std::mem::forget(child);
    Ok(pid)
}

/// Wait (up to `timeout`) for TCP on 127.0.0.1:`port` to accept connections.
fn wait_for_port(port: u16, timeout: Duration) -> bool {
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
    let start = Instant::now();
    while start.elapsed() < timeout {
        if std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(300)).is_ok() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(300)).is_ok()
}

/// Start the server in the background (default `lb web` behavior):
/// - already running (pidfile alive, or something serving the port) → reuse
///   it, open the UI for an interactive human, and show the URL (never start
///   a second daemon);
/// - otherwise detach a child server (logs to `daemon-<port>.log`), wait for
///   it to accept connections, open the UI for an interactive human, and
///   report the URL.
pub fn start_background(args: &DaemonArgs) -> Result<String> {
    let port = args.port;
    let url = format!("http://127.0.0.1:{port}/");
    // Reusing still opens the UI for an interactive human (unless
    // --no-open) — the user asked for the UI to be shown, and the already
    // running daemon never pops one itself. Non-terminal callers (scripts,
    // CI) just get the URL printed.
    let maybe_open = || {
        if !args.no_open && std::io::stdout().is_terminal() {
            open_in_default_browser(&url);
        }
    };
    // Reuse: our own daemon is already up — never start a second one.
    match read_pid(port)? {
        Some(pid) if process_alive(pid) => {
            maybe_open();
            return Ok(format!(
                "a daemon is already running (pid {pid}, port {port}) — reusing it\n  UI:  {url}\n  (stop it with: lb web --port {port} stop)"
            ));
        }
        Some(_) => remove_pid_file(port), // stale crash file — we own the port now
        None => {}
    }
    if port_serves(port) {
        maybe_open();
        return Ok(format!(
            "something is already serving port {port} (no pid record — started by an older lb?) — reusing it\n  UI:  {url}\n  (if that is a stale process, stop it, then run `lb web --port {port}` again)"
        ));
    }
    let exe = std::env::current_exe().context("cannot determine own binary path")?;
    let child_args = child_argv_foreground();
    let log = log_file(port);
    let _spawned_pid = spawn_detached(&exe, &child_args, &log)?;
    if !wait_for_port(port, Duration::from_secs(60)) {
        let tail = std::fs::read_to_string(&log)
            .map(|s| {
                let lines: Vec<&str> = s.lines().collect();
                let n = lines.len().min(15);
                lines[lines.len().saturating_sub(n)..].join("\n")
            })
            .unwrap_or_else(|_| "(could not read log)".to_string());
        anyhow::bail!(
            "background daemon on port {port} did not come up within 60s — log tail from {}:\n{tail}",
            log.display()
        );
    }
    // The interactive parent opens the UI in the default browser: the
    // detached child never pops a window itself — its stdout is a file, and
    // a daemon opening browsers is never wanted. Non-terminal parents
    // (scripts, CI) just print the URL.
    maybe_open();
    // Prefer the pidfile pid (the server's own record) over the spawn pid
    // (identical via setsid-exec, but the file is authoritative).
    let pid_msg = match read_pid(port)? {
        Some(pid) => format!("pid {pid}"),
        None => format!("spawned pid {_spawned_pid}"),
    };
    Ok(format!(
        "started daemon on port {port} ({pid_msg}, background — survives terminal close)\n  UI:  {url}\n  logs: {}\n  (stop it with: lb web --port {port} stop)",
        log.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_data_dir_parsing() {
        assert_eq!(
            user_data_dir("/opt/google/chrome/chrome --user-data-dir=/home/u/.cache/live-browser/profile --headless"),
            Some("/home/u/.cache/live-browser/profile".to_string())
        );
        assert_eq!(
            user_data_dir("chrome --user-data-dir /tmp/p --headless"),
            Some("/tmp/p".to_string())
        );
        assert_eq!(user_data_dir("chrome --headless --no-sandbox"), None);
    }

    /// Regression test: the default profile `…/live-browser/profile` must NOT
    /// match a sibling daemon's `…/live-browser/profile-serve` (exact
    /// `--user-data-dir` comparison, never substring).
    #[test]
    fn profile_match_is_exact() {
        let sibling =
            "/opt/google/chrome/chrome --user-data-dir=/home/u/.cache/live-browser/profile-serve --headless";
        assert_ne!(
            user_data_dir(sibling),
            Some("/home/u/.cache/live-browser/profile".to_string())
        );
        assert_eq!(
            user_data_dir(sibling),
            Some("/home/u/.cache/live-browser/profile-serve".to_string())
        );
    }

    /// Cleanup covers the base profile and its `-cloak` sibling: cloak
    /// orphans must not survive `stop` to wedge the next cloak launch.
    #[test]
    fn owned_profiles_pairs_cloak_sibling() {
        assert_eq!(
            owned_profiles("/home/u/.cache/live-browser/profile"),
            vec![
                "/home/u/.cache/live-browser/profile".to_string(),
                "/home/u/.cache/live-browser/profile-cloak".to_string(),
            ]
        );
        // Already-suffixed base (explicit `--profile *-cloak`) dedups.
        assert_eq!(
            owned_profiles("/home/u/.cache/live-browser/profile-cloak"),
            vec!["/home/u/.cache/live-browser/profile-cloak".to_string()]
        );
    }

    /// Start-mode flags are stripped from the detached child's argv so it
    /// runs as the blocking server (never re-detaches).
    #[test]
    fn start_mode_flags_detected() {
        assert!(is_start_mode_flag("--background"));
        assert!(is_start_mode_flag("--detach"));
        assert!(is_start_mode_flag("--foreground"));
        assert!(!is_start_mode_flag("--port"));
        assert!(!is_start_mode_flag("--headed"));
        assert!(!is_start_mode_flag("stop"));
        assert!(!is_start_mode_flag("--no-open"));
    }

    /// Background is the default; `--foreground` opts out, `--background`
    /// overrides `--foreground` (clap `overrides_with`, last flag wins —
    /// same shape as the `--headless` / `--headed` pair).
    #[test]
    fn background_is_default() {
        use clap::Parser;
        use crate::cli::{Cli, Commands};
        let parse = |argv: &[&str]| {
            let mut full = vec!["lb"];
            full.extend(argv);
            Cli::try_parse_from(full).unwrap()
        };
        // Plain `lb web` → background.
        let Commands::Web(w) = parse(&["web"]).command else { panic!("no web") };
        assert!(w.daemon.background_mode());
        // Explicit `--background` → background.
        let Commands::Web(w) = parse(&["web", "--background"]).command else { panic!("no web") };
        assert!(w.daemon.background_mode());
        // `--foreground` → foreground.
        let Commands::Web(w) = parse(&["web", "--foreground"]).command else { panic!("no web") };
        assert!(!w.daemon.background_mode());
        // Port + foreground compose.
        let Commands::Web(w) = parse(&["web", "--port", "9000", "--foreground"]).command else { panic!("no web") };
        assert_eq!(w.daemon.port, 9000);
        assert!(!w.daemon.background_mode());
    }
}
