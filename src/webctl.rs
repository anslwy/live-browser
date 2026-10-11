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
        let status = cmd
            .arg(pid.to_string())
            .status()
            .context("failed to run `kill`")?;
        if status.success() {
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

/// Pids whose command line mentions `profile` and looks like a browser
/// (Chrome/Chromium/headless_shell) — never our own process.
fn profile_browser_pids(profile: &str) -> Vec<u32> {
    let me = std::process::id();
    process_list()
        .into_iter()
        .filter(|(pid, args)| {
            *pid != me
                && args.contains(profile)
                && {
                    let lower = args.to_lowercase();
                    lower.contains("chrome")
                        || lower.contains("chromium")
                        || lower.contains("headless_shell")
                }
        })
        .map(|(pid, _)| pid)
        .collect()
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

/// Stop the daemon serving `port`. Idempotent: a missing or stale pidfile
/// is reported, not an error. Also reaps orphaned browser processes still
/// holding the profile lock (e.g. daemons started before pidfiles existed,
/// or killed with SIGKILL) so the next start succeeds.
pub fn stop(port: u16, profile: Option<&str>) -> Result<String> {
    let profile = resolve_profile(profile);
    let mut note = String::new();
    match read_pid(port)? {
        Some(pid) if process_alive(pid) => {
            terminate(pid, false)?;
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
            return Ok(format!("stopped daemon on port {port} (pid {pid}{note}){extra}"));
        }
        Some(pid) => {
            remove_pid_file(port);
            note = format!("daemon for port {port} was not running (stale pid {pid} cleaned up)");
        }
        None => {
            note = format!(
                "no daemon recorded for port {port} (was it started by this lb version?)"
            );
        }
    }
    // No live daemon (or none recorded): still free the profile lock from
    // orphaned browsers so the next `lb web` starts clean.
    let reaped = reap_profile_browsers(&profile);
    if reaped > 0 {
        Ok(format!("{note}; freed profile by stopping {reaped} leftover browser process(es)"))
    } else {
        Ok(format!("{note} — nothing to stop"))
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
