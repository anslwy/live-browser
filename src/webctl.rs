//! `lb web stop` / `lb web restart`: find the running daemon via its pidfile
//! and terminate it. No new dependencies — `kill(1)` on Unix, `taskkill` /
//! `tasklist` on Windows.
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

fn pid_file(port: u16) -> PathBuf {
    crate::daemon::pid_file(port)
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
fn profile_browser_pids(profile: &str) -> Vec<u32> {
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
    let reaped = reap_profile_browsers(profile);
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
            let reaped = reap_profile_browsers(&profile);
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
}
