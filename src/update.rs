//! `lb update`: replace this install with the latest GitHub release.
//!
//! Layout assumptions (same as install.sh / install.ps1): the running
//! binary lives in an install dir next to `web/dist`:
//!
//! ```text
//! $PREFIX/share/live-browser/lb[.exe]
//! $PREFIX/share/live-browser/web/dist/…
//! ```
//!
//! Anything else (e.g. `target/debug/lb` in a source checkout) is refused
//! with a pointer at the right update path.
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};

const REPO: &str = "anslwy/live-browser";
const CURRENT: &str = env!("CARGO_PKG_VERSION");

/// `v0.2.5` → `[0, 2, 5]`. Non-numeric tails are ignored (`1.2-beta` → `[1, 2]`).
fn parse_version(tag: &str) -> Vec<u64> {
    tag.trim()
        .trim_start_matches('v')
        .split('.')
        .map(|p| p.chars().take_while(|c| c.is_ascii_digit()).collect::<String>())
        .filter(|p| !p.is_empty())
        .map(|p| p.parse().unwrap_or(0))
        .collect()
}

/// True when `latest` is strictly newer than `current`.
fn is_newer(current: &str, latest: &str) -> bool {
    let (mut a, mut b) = (parse_version(current), parse_version(latest));
    let n = a.len().max(b.len());
    a.resize(n, 0);
    b.resize(n, 0);
    a < b
}

/// Release asset stem for this platform (`live-browser-<tag>-<stem>.<ext>`),
/// or `None` when no prebuilt binary is published for it.
///
/// NOTE: Rust reports Apple Silicon as `aarch64` (not `arm64`, which is
/// Apple's/Node's name) — matching the wrong string silently disables
/// updates on every modern Mac.
fn asset_stem() -> Option<(&'static str, &'static str)> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => Some(("linux-x86_64", "tar.gz")),
        ("macos", "aarch64") => Some(("darwin-arm64", "tar.gz")),
        ("macos", "x86_64") => Some(("darwin-x86_64", "tar.gz")),
        ("windows", _) => Some(("windows-x86_64", "zip")),
        _ => None,
    }
}

fn asset_name(tag: &str) -> Option<String> {
    asset_stem().map(|(platform, ext)| format!("live-browser-{tag}-{platform}.{ext}"))
}

fn github_api() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent("live-browser")
        .timeout(Duration::from_secs(10))
        .build()
        .context("cannot build HTTP client")
}

/// `tag_name` of the latest GitHub release (e.g. `v0.2.5`).
pub async fn latest_tag() -> Result<String> {
    let client = github_api()?;
    let v: serde_json::Value = client
        .get(format!("https://api.github.com/repos/{REPO}/releases/latest"))
        .send()
        .await
        .context("could not reach api.github.com (network? rate limit?)")?
        .error_for_status()
        .context("GitHub API error")?
        .json()
        .await
        .context("invalid GitHub API response")?;
    v.get("tag_name")
        .and_then(|t| t.as_str())
        .map(|t| t.to_string())
        .context("GitHub response has no tag_name")
}

/// Human one-liner comparing this binary with `latest`.
/// Names both versions so a stale "already latest" vs. a startup "update
/// available" can never contradict each other silently.
pub fn status_line(latest: &str) -> String {
    if is_newer(CURRENT, latest) {
        format!("update available: {latest} (you have v{CURRENT}) — run `lb update`")
    } else {
        format!("already on the latest release (you have v{CURRENT}; latest is {latest})")
    }
}

/// Install dir of the running binary (canonicalized, symlinks resolved).
/// Refuses source checkouts: those update via `git pull`, not downloads.
fn install_dir() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("cannot locate own binary")?;
    let exe = std::fs::canonicalize(&exe).unwrap_or(exe);
    let dir = exe
        .parent()
        .context("binary has no parent dir")?
        .to_path_buf();
    if dir.join("web").join("dist").join("index.html").exists()
        || dir.join("web").exists()
    {
        return Ok(dir);
    }
    anyhow::bail!(
        "{} looks like a dev build (no web/ next to it) — update with `git pull` + rebuild instead",
        exe.display()
    )
}

/// Stop every recorded daemon so files can be replaced cleanly.
fn stop_all_daemons() -> Vec<String> {
    let base = crate::daemon::cache_base().join("live-browser");
    let mut msgs = vec![];
    let entries = std::fs::read_dir(&base).map(|r| {
        r.filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect::<Vec<_>>()
    });
    let mut ports: Vec<u16> = entries
        .unwrap_or_default()
        .iter()
        .filter_map(|n| {
            n.strip_prefix("daemon-")?
                .strip_suffix(".pid")?
                .parse()
                .ok()
        })
        .collect();
    ports.sort_unstable();
    ports.dedup();
    for port in ports {
        match crate::webctl::stop(port, None) {
            Ok(msg) => msgs.push(msg),
            Err(e) => msgs.push(format!("port {port}: stop skipped ({e:#})")),
        }
    }
    msgs
}

async fn download(url: &str, dest: &Path) -> Result<()> {
    let client = github_api()?;
    let bytes = client
        .get(url)
        .send()
        .await
        .context("download failed (network?)")?
        .error_for_status()
        .context("download failed")?
        .bytes()
        .await
        .context("download failed")?;
    std::fs::write(dest, &bytes).context("cannot write download")?;
    Ok(())
}

/// Unpack `archive` (tar.gz on Unix, zip on Windows) into `dest`.
fn unpack(archive: &Path, dest: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        let status = std::process::Command::new("tar")
            .args(["-xzf"])
            .arg(archive)
            .arg("-C")
            .arg(dest)
            .status()
            .context("failed to run `tar` (needed to unpack the release)")?;
        anyhow::ensure!(status.success(), "`tar` failed to unpack the release");
        Ok(())
    }
    #[cfg(windows)]
    {
        unpack_zip(archive, dest)
    }
}

/// Extract a `.zip` with the in-crate `zip` dependency — never PowerShell's
/// `Expand-Archive`, whose behavior differs between PS 5.1 (user machines)
/// and PS 7 (our release runners) and fails on our archives with an opaque
/// `Join-Path ... drive is null` error. Split out so every CI host,
/// including Linux, exercises the Windows code path via tests.
#[cfg(any(windows, test))]
fn unpack_zip(archive: &Path, dest: &Path) -> Result<()> {
    let f = std::fs::File::open(archive)?;
    let mut zip = zip::ZipArchive::new(f)?;
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i)?;
        let rel = entry
            .enclosed_name()
            .with_context(|| format!("unsafe zip entry #{i}"))?;
        let out = dest.join(rel);
        if entry.is_dir() {
            std::fs::create_dir_all(&out)?;
        } else {
            if let Some(p) = out.parent() {
                std::fs::create_dir_all(p)?;
            }
            let mut f = std::fs::File::create(&out)?;
            std::io::copy(&mut entry, &mut f)?;
        }
    }
    Ok(())
}

fn copy_dir_recursive(src: &Path, dest: &Path) -> Result<()> {
    std::fs::create_dir_all(dest)?;
    for entry in std::fs::read_dir(src).with_context(|| format!("cannot list {}", src.display()))? {
        let entry = entry?;
        let (from, to) = (entry.path(), dest.join(entry.file_name()));
        if entry.file_type()?.is_dir() {
            copy_dir_recursive(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)
                .with_context(|| format!("cannot copy {}", from.display()))?;
        }
    }
    Ok(())
}

/// Move `new_file` onto `target`, tolerating a running binary: renames work
/// over running executables on Unix, and rename-away works on Windows too.
///
/// `new_file` and `target` must live on the same filesystem for the fast
/// path; if the rename fails with EXDEV (e.g. a temp dir on another mount),
/// fall back to copy+remove — safe here because `target` was already moved
/// aside, so no running image is ever overwritten in place.
fn replace_file(new_file: &Path, target: &Path) -> Result<()> {
    // Rename-away first so a half-written target is impossible; the backup
    // is removed afterwards (best-effort: a running Windows binary can't
    // always be deleted immediately, but it harms nothing).
    let backup = target.with_extension("lb-update-bak");
    let _ = std::fs::remove_file(&backup);
    if target.exists() {
        std::fs::rename(target, &backup)
            .with_context(|| format!("cannot move aside {}", target.display()))?;
    }
    let moved = std::fs::rename(new_file, target).is_ok()
        || (std::fs::copy(new_file, target).is_ok() && std::fs::remove_file(new_file).is_ok());
    if moved {
        let _ = std::fs::remove_file(&backup);
        Ok(())
    } else {
        // Roll back so the install is never left without a binary.
        let _ = std::fs::rename(&backup, target);
        anyhow::bail!("cannot install {}", target.display())
    }
}

/// Download `tag` and install it over the running install dir.
async fn install_tag(tag: &str) -> Result<Vec<String>> {
    let asset = asset_name(tag).context(
        "no prebuilt binary for this platform — update with the installer script instead \
         (or build with --from-source / -FromSource)",
    )?;
    let dir = install_dir()?;
    // Work dir lives *inside* the install dir so every move below stays on
    // one filesystem (`std::env::temp_dir()` is often a separate mount —
    // tmpfs /tmp vs. /home — where rename fails with EXDEV).
    let work = dir.join(format!(".lb-update-{}", std::process::id()));
    if work.exists() {
        std::fs::remove_dir_all(&work).ok();
    }
    std::fs::create_dir_all(&work)?;
    let result = install_tag_inner(tag, &asset, &dir, &work).await;
    if result.is_err() {
        std::fs::remove_dir_all(&work).ok();
    }
    result
}

async fn install_tag_inner(tag: &str, asset: &str, dir: &Path, work: &Path) -> Result<Vec<String>> {
    let mut log = vec![];
    let archive = work.join(asset);
    println!("downloading {asset}…");
    download(
        &format!("https://github.com/{REPO}/releases/download/{tag}/{asset}"),
        &archive,
    )
    .await?;
    println!("unpacking…");
    unpack(&archive, work)?;
    // Archives wrap everything in one top-level `live-browser-<tag>-…/` dir.
    let inner = std::fs::read_dir(work)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.is_dir())
        .context("unexpected archive layout")?;
    let exe_name = if cfg!(windows) { "lb.exe" } else { "lb" };
    let new_bin = inner.join(exe_name);
    anyhow::ensure!(
        new_bin.exists(),
        "archive has no {exe_name} — refusing a partial update"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&new_bin, std::fs::Permissions::from_mode(0o755)).ok();
    }
    replace_file(&new_bin, &dir.join(exe_name))?;
    log.push(format!("updated binary to {tag}"));
    if inner.join("web").exists() {
        copy_dir_recursive(&inner.join("web"), &dir.join("web"))?;
        log.push("updated web UI".to_string());
    }
    for extra in ["README.md", "install.sh", "install.ps1"] {
        if inner.join(extra).exists() {
            std::fs::copy(inner.join(extra), dir.join(extra)).ok();
        }
    }
    std::fs::remove_dir_all(work).ok();
    Ok(log)
}

/// `lb update [--force]`: stop daemons, fetch the latest release (or say
/// we're already there) and install it over this install dir.
pub async fn update(force: bool) -> Result<()> {
    let latest = latest_tag().await?;
    if !force && !is_newer(CURRENT, &latest) {
        println!("{}", status_line(&latest));
        return Ok(());
    }
    // Fail fast on platforms without a prebuilt binary — before stopping
    // anything, so a doomed update never takes the running daemon down.
    asset_name(&latest).context(
        "no prebuilt binary for this platform — update with the installer script instead \
         (or build with --from-source / -FromSource)",
    )?;
    if force && !is_newer(CURRENT, &latest) {
        println!("reinstalling {latest} (--force)…");
    } else {
        println!("updating v{CURRENT} → {latest}…");
    }
    for msg in stop_all_daemons() {
        println!("  {msg}");
    }
    for msg in install_tag(&latest).await? {
        println!("  {msg}");
    }
    println!("done — start the server again with `lb web`");
    Ok(())
}

/// Fire-and-forget reminder used at `lb web` startup: never blocks, never
/// fails. Skipped with LB_NO_UPDATE_CHECK=1.
pub fn spawn_update_reminder() {
    if std::env::var("LB_NO_UPDATE_CHECK").ok().as_deref() == Some("1") {
        return;
    }
    tokio::spawn(async {
        // Short leash: slow networks must not leave a lingering task.
        let latest = tokio::time::timeout(Duration::from_secs(8), latest_tag()).await;
        if let Ok(Ok(tag)) = latest {
            if is_newer(CURRENT, &tag) {
                println!("update available: {tag} (you have v{CURRENT}) — run `lb update`");
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_compare() {
        assert!(is_newer("0.2.5", "v0.2.6"));
        assert!(is_newer("v0.2.5", "0.2.6"));
        assert!(is_newer("0.2.9", "0.2.10"));
        assert!(is_newer("0.2.5", "0.3.0"));
        assert!(!is_newer("0.2.6", "v0.2.6"));
        assert!(!is_newer("v0.2.6", "0.2.5"));
        assert!(!is_newer("0.2.10", "0.2.9"));
        assert!(!is_newer("1.0.0", "1.0"));
    }

    #[test]
    fn asset_name_matches_release() {
        // Exact per-platform mapping — a wrong ARCH string (e.g. `arm64`
        // instead of Rust's `aarch64`) silently disables updates, so assert
        // precisely, on every platform CI runs.
        let expected: Option<(&str, &str)> = if cfg!(target_os = "linux") {
            if cfg!(target_arch = "x86_64") {
                Some(("linux-x86_64", "tar.gz"))
            } else {
                None
            }
        } else if cfg!(target_os = "macos") {
            if cfg!(target_arch = "aarch64") {
                Some(("darwin-arm64", "tar.gz"))
            } else {
                Some(("darwin-x86_64", "tar.gz"))
            }
        } else if cfg!(windows) {
            Some(("windows-x86_64", "zip"))
        } else {
            None
        };
        assert_eq!(asset_stem(), expected);
        if let Some((platform, ext)) = asset_stem() {
            assert_eq!(
                asset_name("v0.2.5"),
                Some(format!("live-browser-v0.2.5-{platform}.{ext}"))
            );
        } else {
            assert_eq!(asset_name("v0.2.5"), None);
        }
    }

    /// The release `.zip` layout must extract with the in-crate reader on
    /// every host (regression test for PowerShell 5.1 `Expand-Archive`
    /// failing on our archives with `Join-Path ... drive is null`).
    /// Runs on all CI platforms — it calls `unpack_zip` directly, not the
    /// cfg-gated `unpack`.
    #[test]
    fn unpack_zip_extracts_release_layout() {
        let dir = std::env::temp_dir().join(format!("lb-ziptest-{}", std::process::id()));
        if dir.exists() {
            std::fs::remove_dir_all(&dir).unwrap();
        }
        std::fs::create_dir_all(&dir).unwrap();
        let archive = dir.join("live-browser-v9.9.9-windows-x86_64.zip");
        {
            let f = std::fs::File::create(&archive).unwrap();
            let mut zip = zip::ZipWriter::new(f);
            let opts = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            for name in [
                "live-browser-v9.9.9-windows-x86_64/lb.exe",
                "live-browser-v9.9.9-windows-x86_64/web/dist/index.html",
                "live-browser-v9.9.9-windows-x86_64/README.md",
            ] {
                zip.start_file(name, opts).unwrap();
                use std::io::Write;
                zip.write_all(b"fake").unwrap();
            }
            zip.finish().unwrap();
        }
        let dest = dir.join("out");
        unpack_zip(&archive, &dest).unwrap();
        assert!(dest
            .join("live-browser-v9.9.9-windows-x86_64/lb.exe")
            .is_file());
        assert!(dest
            .join("live-browser-v9.9.9-windows-x86_64/web/dist/index.html")
            .is_file());
        std::fs::remove_dir_all(&dir).ok();
    }
}
