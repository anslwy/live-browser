//! Stealth-Chromium inventory + download for the Settings → Browser UI.
//!
//! Two browser kinds are offered:
//! - `system`: the normal Chrome/Chromium (`--chrome` default / auto-detect).
//! - `cloak`: CloakBrowser's patched Chromium (free build), cached at
//!   `~/.cloakbrowser/chromium-{version}/chrome` — the wrapper's layout, so a
//!   binary fetched via `npx cloakbrowser install` is picked up as-is.
//!
//! Free-build distribution (mirrors the cloakbrowser wrapper):
//! primary `https://cloakbrowser.dev/chromium-v{version}/{archive}`, fallback
//! to the GitHub release asset of the same name. The newest free version is
//! discovered via the GitHub releases API (first non-draft `chromium-v*` tag
//! shipping this platform's tarball); a pinned fallback covers offline API.
//!
//! Integrity: the archive's SHA-256 is checked against the release SHA256SUMS
//! fetched from the same origin. (The wrapper additionally verifies an Ed25519
//! signature — not replicated here; a checksum mismatch still aborts and is
//! never silently retried against another binary.)

use anyhow::{Context, Result};
use serde::Serialize;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

/// Pinned free version used when the GitHub releases API is unreachable.
pub const FALLBACK_FREE_VERSION: &str = "146.0.7680.177.5";

/// `linux-x64` / `linux-arm64` / `darwin-arm64` / `darwin-x64` / `windows-x64`.
pub fn platform_tag() -> Result<&'static str> {
    if cfg!(target_os = "linux") {
        if cfg!(target_arch = "x86_64") {
            return Ok("linux-x64");
        }
        if cfg!(target_arch = "aarch64") {
            return Ok("linux-arm64");
        }
    } else if cfg!(target_os = "macos") {
        if cfg!(target_arch = "aarch64") {
            return Ok("darwin-arm64");
        }
        if cfg!(target_arch = "x86_64") {
            return Ok("darwin-x64");
        }
    } else if cfg!(target_os = "windows") && cfg!(target_arch = "x86_64") {
        return Ok("windows-x64");
    }
    anyhow::bail!("no prebuilt CloakBrowser binary for this platform (see https://github.com/CloakHQ/CloakBrowser)")
}

pub fn archive_ext() -> &'static str {
    if cfg!(target_os = "windows") {
        ".zip"
    } else {
        ".tar.gz"
    }
}

/// `cloakbrowser-linux-x64.tar.gz` etc.
pub fn archive_name() -> Result<String> {
    Ok(format!("cloakbrowser-{}{}", platform_tag()?, archive_ext()))
}

fn binary_file_name() -> &'static str {
    if cfg!(target_os = "windows") {
        "chrome.exe"
    } else {
        "chrome"
    }
}

/// `~/.cloakbrowser` (honors `CLOAKBROWSER_CACHE_DIR`, like the wrapper).
pub fn cache_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("CLOAKBROWSER_CACHE_DIR") {
        if !dir.trim().is_empty() {
            return PathBuf::from(dir);
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home).join(".cloakbrowser")
}

pub fn binary_path(version: &str) -> PathBuf {
    cache_dir()
        .join(format!("chromium-{version}"))
        .join(binary_file_name())
}

fn is_executable(p: &std::path::Path) -> bool {
    if !p.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        p.metadata()
            .map(|m| m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// `<binary> --version` → e.g. `Chromium 146.0.7680.177`. Bounded.
pub async fn binary_version(path: &str) -> Option<String> {
    let out = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        tokio::process::Command::new(path).arg("--version").output(),
    )
    .await
    .ok()?
    .ok()?;
    if !out.status.success() {
        return None;
    }
    let line = String::from_utf8_lossy(&out.stdout).lines().next()?.trim().to_string();
    if line.is_empty() {
        None
    } else {
        Some(line.chars().take(80).collect())
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct InstalledCloak {
    pub version: String,
    pub path: String,
}

/// Every `chromium-*/chrome` in the cache with an executable bit.
pub fn installed() -> Vec<InstalledCloak> {
    let dir = cache_dir();
    let entries = std::fs::read_dir(&dir).map(|r| r.filter_map(|e| e.ok()).collect::<Vec<_>>()).unwrap_or_default();
    let mut out = vec![];
    for e in entries {
        let name = e.file_name().to_string_lossy().to_string();
        let version = match name.strip_prefix("chromium-") {
            Some(v) if !v.starts_with('.') && !v.contains("partial") && !v.contains("stale") => v.to_string(),
            _ => continue,
        };
        let bin = e.path().join(binary_file_name());
        if is_executable(&bin) {
            out.push(InstalledCloak { version, path: bin.to_string_lossy().to_string() });
        }
    }
    out.sort_by(|a, b| b.version.cmp(&a.version));
    out
}

/// Newest free version from the GitHub releases API: first non-draft
/// `chromium-v*` tag shipping this platform's tarball (wrapper's rule).
/// `None` when offline / rate-limited — callers fall back to
/// [`FALLBACK_FREE_VERSION`].
pub async fn latest_free_version() -> Option<String> {
    let archive = archive_name().ok()?;
    // NOTE: GitHub API 403s requests without a User-Agent.
    let client = reqwest::Client::builder().user_agent("live-browser").build().ok()?;
    let resp = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        client.get("https://api.github.com/repos/CloakHQ/CloakBrowser/releases?per_page=20").send(),
    )
    .await
    .ok()?
    .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let releases: Vec<serde_json::Value> = resp.json().await.ok()?;
    for r in releases {
        let tag = r.get("tag_name")?.as_str()?;
        if !tag.starts_with("chromium-v") || r.get("draft")?.as_bool().unwrap_or(true) {
            continue;
        }
        let assets = r.get("assets")?.as_array()?;
        if assets.iter().any(|a| a.get("name").and_then(|n| n.as_str()) == Some(&archive)) {
            return tag.strip_prefix("chromium-v").map(|s| s.to_string());
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Background download with progress
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum DlPhase {
    #[default]
    Idle,
    Downloading,
    Verifying,
    Extracting,
    Done,
    Error,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct DownloadState {
    pub phase: DlPhase,
    pub version: String,
    pub received: u64,
    pub total: Option<u64>,
    pub message: String,
}

impl DownloadState {
    /// 0–100, or None when the total size is unknown.
    pub fn pct(&self) -> Option<u64> {
        match (self.total, self.received) {
            (Some(t), r) if t > 0 => Some((r.min(t) * 100 / t).min(100)),
            _ => None,
        }
    }
}

async fn set(s: &Arc<Mutex<DownloadState>>, f: impl FnOnce(&mut DownloadState)) {
    f(&mut *s.lock().await);
}

/// Fetch `url` → temp file, updating progress. Tries each URL in order.
async fn fetch_to_tmp(urls: &[String], tmp: &std::path::Path, s: &Arc<Mutex<DownloadState>>) -> Result<u64> {
    let mut last_err = anyhow::anyhow!("no download URL");
    for url in urls {
        set(s, |d| d.message = "connecting…".to_string()).await;
        match try_fetch(url, tmp, s).await {
            Ok(n) => return Ok(n),
            Err(e) => {
                tracing::warn!("cloak download failed for {url}: {e:#}");
                last_err = e;
                continue;
            }
        }
    }
    Err(last_err)
}

async fn try_fetch(url: &str, tmp: &std::path::Path, s: &Arc<Mutex<DownloadState>>) -> Result<u64> {
    let resp = reqwest::get(url).await.with_context(|| format!("GET {url}"))?;
    if !resp.status().is_success() {
        anyhow::bail!("{url} → HTTP {}", resp.status());
    }
    let total = resp.content_length();
    set(s, |d| {
        d.total = total;
        d.received = 0;
    })
    .await;
    let mut file = tokio::fs::File::create(tmp).await.with_context(|| format!("create {}", tmp.display()))?;
    let mut received: u64 = 0;
    let mut resp = resp;
    while let Some(c) = resp.chunk().await.with_context(|| format!("read {url}"))? {
        use tokio::io::AsyncWriteExt;
        file.write_all(&c).await.with_context(|| format!("write {}", tmp.display()))?;
        received += c.len() as u64;
        // Coalesce UI updates: lock + serialize per chunk is fine at
        // these sizes, but skip tiny increments anyway.
        if received % 65536 < c.len() as u64 || total.is_none() {
            let r = received;
            set(s, |d| d.received = r).await;
        }
    }
    {
        use tokio::io::AsyncWriteExt;
        file.flush().await.ok();
    }
    let r = received;
    set(s, |d| d.received = r).await;
    Ok(received)
}

/// `SHA256SUMS` from the same origin → expected hex for our archive.
async fn expected_sha256(base: &str, archive: &str) -> Result<String> {
    let url = format!("{base}/SHA256SUMS");
    let text = reqwest::get(&url).await.with_context(|| format!("GET {url}"))?.text().await?;
    for line in text.lines() {
        // `<hash><space><space><filename>` (plus a `version=` line to skip).
        let mut parts = line.split_whitespace();
        if let (Some(hash), Some(name)) = (parts.next(), parts.next()) {
            if name.trim_start_matches('*') == archive && hash.len() == 64 {
                return Ok(hash.to_string());
            }
        }
    }
    anyhow::bail!("SHA256SUMS at {base} has no entry for {archive}")
}

fn sha256_file(path: &std::path::Path) -> Result<String> {
    use sha2::Digest;
    let mut h = sha2::Sha256::new();
    let mut f = std::fs::File::open(path)?;
    std::io::copy(&mut f, &mut h)?;
    Ok(format!("{:x}", h.finalize()))
}

/// Extract into a staging dir, flatten a single top-level subdir (wrapper's
/// layout rule), chmod the binary, then rename into place.
fn extract_staged(archive: &std::path::Path, staging: &std::path::Path, binary_rel: &std::path::Path) -> Result<()> {
    std::fs::create_dir_all(staging)?;
    let name = archive.to_string_lossy().to_string();
    if name.ends_with(".zip") {
        let f = std::fs::File::open(archive)?;
        let mut zip = zip::ZipArchive::new(f)?;
        for i in 0..zip.len() {
            let mut entry = zip.by_index(i)?;
            let rel = entry.enclosed_name().with_context(|| format!("unsafe zip entry #{i}"))?;
            let dest = staging.join(rel);
            if entry.is_dir() {
                std::fs::create_dir_all(&dest)?;
            } else {
                if let Some(p) = dest.parent() {
                    std::fs::create_dir_all(p)?;
                }
                let mut out = std::fs::File::create(&dest)?;
                std::io::copy(&mut entry, &mut out)?;
                #[cfg(unix)]
                if let Some(mode) = entry.unix_mode() {
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(mode))?;
                }
            }
        }
    } else {
        let f = std::fs::File::open(archive)?;
        let gz = flate2::read::GzDecoder::new(f);
        let mut tar = tar::Archive::new(gz);
        tar.unpack(staging)?;
    }
    // Flatten `archive-root/*` → `staging/*` when the archive has exactly one
    // top-level directory (tolerates both layouts).
    let tops: Vec<_> = std::fs::read_dir(staging)?.filter_map(|e| e.ok()).collect();
    if tops.len() == 1 && tops[0].path().is_dir() {
        let inner = tops[0].path();
        let tmp = staging.join(".flatten-tmp");
        std::fs::rename(&inner, &tmp)?;
        for e in std::fs::read_dir(&tmp)?.filter_map(|x| x.ok()) {
            std::fs::rename(e.path(), staging.join(e.file_name()))?;
        }
        std::fs::remove_dir(&tmp)?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let bin = staging.join(binary_rel);
        if bin.is_file() {
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755))?;
        }
    }
    Ok(())
}

/// Download + verify + install the free build. Resolves `latest` via the
/// releases API (pinned fallback offline). Updates `state` throughout; on
/// success returns the chrome binary path.
pub async fn download_free(version: Option<String>, state: Arc<Mutex<DownloadState>>) -> Result<String> {
    {
        let d = state.lock().await;
        if matches!(d.phase, DlPhase::Downloading | DlPhase::Verifying | DlPhase::Extracting) {
            anyhow::bail!("a download is already running ({} {})", format!("{:?}", d.phase).to_lowercase(), d.version);
        }
    }
    let archive = archive_name()?;
    let version = match version {
        Some(v) if !v.trim().is_empty() => v.trim().to_string(),
        _ => latest_free_version().await.unwrap_or_else(|| FALLBACK_FREE_VERSION.to_string()),
    };
    set(&state, |d| {
        *d = DownloadState { phase: DlPhase::Downloading, version: version.clone(), received: 0, total: None, message: "starting…".into() };
    })
    .await;
    let fail = |msg: String| {
        let state = state.clone();
        async move {
            set(&state, |d| {
                d.phase = DlPhase::Error;
                d.message = msg;
            })
            .await;
        }
    };
    let base_primary = format!("https://cloakbrowser.dev/chromium-v{version}");
    let base_fallback = format!("https://github.com/CloakHQ/cloakbrowser/releases/download/chromium-v{version}");
    let cache = cache_dir();
    if let Err(e) = std::fs::create_dir_all(&cache) {
        let m = format!("cannot create {}: {e:#}", cache.display());
        fail(m.clone()).await;
        anyhow::bail!("{m}");
    }
    let tmp = cache.join(format!(".lb-download-{version}{}", archive_ext()));
    // 1. Fetch (primary, then GitHub fallback — never the reverse: a
    // verification failure below is a tamper signal, not a retry trigger).
    let urls = vec![format!("{base_primary}/{archive}"), format!("{base_fallback}/{archive}")];
    if let Err(e) = fetch_to_tmp(&urls, &tmp, &state).await {
        let m = format!("download failed: {e:#}");
        fail(m.clone()).await;
        std::fs::remove_file(&tmp).ok();
        anyhow::bail!("{m}");
    }
    // 2. Verify SHA-256 against the same origin's manifest.
    set(&state, |d| {
        d.phase = DlPhase::Verifying;
        d.message = "verifying checksum…".into();
    })
    .await;
    let bases = [base_primary, base_fallback];
    let mut expected: Option<String> = None;
    for b in &bases {
        match expected_sha256(b, &archive).await {
            Ok(h) => {
                expected = Some(h);
                break;
            }
            Err(e) => tracing::warn!("SHA256SUMS fetch failed for {b}: {e:#}"),
        }
    }
    let expected = match expected {
        Some(h) => h,
        None => {
            let m = "could not fetch SHA256SUMS — refusing unverified binary".to_string();
            fail(m.clone()).await;
            std::fs::remove_file(&tmp).ok();
            anyhow::bail!("{m}");
        }
    };
    let actual = match sha256_file(&tmp) {
        Ok(h) => h,
        Err(e) => {
            let m = format!("checksum read failed: {e:#}");
            fail(m.clone()).await;
            std::fs::remove_file(&tmp).ok();
            anyhow::bail!("{m}");
        }
    };
    if !actual.eq_ignore_ascii_case(&expected) {
        let m = format!("checksum mismatch for {archive} (expected {expected}, got {actual}) — refusing to install");
        fail(m.clone()).await;
        std::fs::remove_file(&tmp).ok();
        anyhow::bail!("{m}");
    }
    // 3. Extract via staging dir, then rename into place.
    set(&state, |d| {
        d.phase = DlPhase::Extracting;
        d.message = "extracting…".into();
    })
    .await;
    let dest = cache.join(format!("chromium-{version}"));
    let staging = cache.join(format!(".lb-staging-{version}"));
    std::fs::remove_dir_all(&staging).ok();
    let rel = PathBuf::from(binary_file_name());
    let extract_ok = tokio::task::spawn_blocking({
        let (tmp, staging, rel) = (tmp.clone(), staging.clone(), rel.clone());
        move || extract_staged(&tmp, &staging, &rel)
    })
    .await;
    std::fs::remove_file(&tmp).ok();
    match extract_ok {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            let m = format!("extract failed: {e:#}");
            fail(m.clone()).await;
            std::fs::remove_dir_all(&staging).ok();
            anyhow::bail!("{m}");
        }
        Err(e) => {
            let m = format!("extract task failed: {e}");
            fail(m.clone()).await;
            std::fs::remove_dir_all(&staging).ok();
            anyhow::bail!("{m}");
        }
    }
    // Replace any incomplete previous install; then promote staging.
    let staged_bin = staging.join(binary_file_name());
    if !is_executable(&staged_bin) {
        let m = "archive extracted but the chrome binary is missing".to_string();
        fail(m.clone()).await;
        std::fs::remove_dir_all(&staging).ok();
        anyhow::bail!("{m}");
    }
    if dest.exists() {
        // Keep a complete install if another caller finished it meanwhile;
        // otherwise clear the way for staging.
        let keep = is_executable(&dest.join(binary_file_name()));
        if keep {
            std::fs::remove_dir_all(&staging).ok();
        } else {
            std::fs::remove_dir_all(&dest).ok();
            if let Err(e) = std::fs::rename(&staging, &dest) {
                let m = format!("install failed: {e:#}");
                fail(m.clone()).await;
                anyhow::bail!("{m}");
            }
        }
    } else if let Err(e) = std::fs::rename(&staging, &dest) {
        let m = format!("install failed: {e:#}");
        fail(m.clone()).await;
        anyhow::bail!("{m}");
    }
    let bin = binary_path(&version).to_string_lossy().to_string();
    set(&state, |d| {
        d.phase = DlPhase::Done;
        d.message = format!("installed {bin}");
    })
    .await;
    Ok(bin)
}
