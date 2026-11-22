//! Tab screen recording.
//!
//! The daemon grabs JPEG frames from a tab on a timer (`Page.captureScreenshot`,
//! same path as `/api/screenshot`), keeping real timestamps and writing a
//! frame only when it differs from the previous one. On stop the frames are
//! encoded to MP4 with `ffmpeg` when it is installed, otherwise to a
//! dependency-free MJPEG AVI (plays in VLC/mpv/most desktop players).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use chromiumoxide::Page;

pub const DEFAULT_FPS: f64 = 5.0;
pub const MAX_FPS: f64 = 15.0;
pub const DEFAULT_MAX_SECS: u64 = 300;
pub const HARD_MAX_SECS: u64 = 3600;

/// One captured moment: which file on disk, and when (ms since start).
#[derive(Debug, Clone)]
pub struct Frame {
    pub file: PathBuf,
    pub t_ms: u64,
}

/// Why the capture loop ended on its own (before `stop`).
#[derive(Debug, Clone, Default)]
pub struct LoopEnd {
    pub frames: Vec<Frame>,
    pub reason: Option<String>,
}

pub struct Active {
    pub id: String,
    pub tab: u32,
    pub fps: f64,
    pub dir: PathBuf,
    pub started: Instant,
    pub started_at: String,
    pub stop: Arc<AtomicBool>,
    pub task: tokio::task::JoinHandle<LoopEnd>,
}

/// Finished recording, servable via `GET /api/recording?id=`.
#[derive(Debug, Clone)]
pub struct Done {
    pub path: PathBuf,
    pub mime: &'static str,
}

pub fn root_dir() -> PathBuf {
    std::env::temp_dir().join("lb-recordings")
}

pub fn new_id(tab: u32) -> String {
    format!("tab{tab}-{}", chrono::Local::now().format("%Y%m%d-%H%M%S-%3f"))
}

pub fn clamp_fps(fps: Option<f64>) -> f64 {
    match fps {
        Some(f) if f.is_finite() && f > 0.0 => f.clamp(0.5, MAX_FPS),
        _ => DEFAULT_FPS,
    }
}

pub fn clamp_max_secs(s: Option<u64>) -> u64 {
    match s {
        Some(0) | None => DEFAULT_MAX_SECS,
        Some(s) => s.min(HARD_MAX_SECS),
    }
}

/// Capture until `stop` is set, `max` elapses, or the tab keeps failing.
pub async fn capture_loop(page: Page, dir: PathBuf, fps: f64, max: Duration, stop: Arc<AtomicBool>) -> LoopEnd {
    let mut out = LoopEnd::default();
    let start = Instant::now();
    let mut tick = tokio::time::interval(Duration::from_secs_f64(1.0 / fps));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last: Option<(Vec<u8>, PathBuf)> = None;
    let mut fails = 0u32;
    let mut n = 0u32;
    loop {
        tick.tick().await;
        if stop.load(Ordering::SeqCst) {
            break;
        }
        if start.elapsed() >= max {
            out.reason = Some(format!("max duration {}s reached", max.as_secs()));
            break;
        }
        let params = chromiumoxide::page::ScreenshotParams::builder()
            .format(chromiumoxide::cdp::browser_protocol::page::CaptureScreenshotFormat::Jpeg)
            .quality(70)
            .build();
        let t_ms = start.elapsed().as_millis() as u64;
        match tokio::time::timeout(Duration::from_secs(10), page.screenshot(params)).await {
            Ok(Ok(bytes)) => {
                fails = 0;
                let file = match &last {
                    Some((prev, f)) if *prev == bytes => f.clone(),
                    _ => {
                        n += 1;
                        let f = dir.join(format!("f{n:06}.jpg"));
                        if let Err(e) = tokio::fs::write(&f, &bytes).await {
                            out.reason = Some(format!("write frame failed: {e}"));
                            break;
                        }
                        last = Some((bytes, f.clone()));
                        f
                    }
                };
                out.frames.push(Frame { file, t_ms });
            }
            _ => {
                fails += 1;
                // Tab closed / browser gone: stop rather than spin.
                if fails >= 10 {
                    out.reason = Some("tab stopped responding (closed or browser dead)".into());
                    break;
                }
            }
        }
    }
    out
}

/// Per-frame display durations in seconds (last frame gets one tick).
pub fn durations(frames: &[Frame], fps: f64) -> Vec<f64> {
    let tick = 1.0 / fps;
    frames
        .iter()
        .enumerate()
        .map(|(i, f)| match frames.get(i + 1) {
            Some(next) => (next.t_ms.saturating_sub(f.t_ms) as f64 / 1000.0).max(0.001),
            None => tick,
        })
        .collect()
}

/// Width/height from a baseline or progressive JPEG's SOF marker.
pub fn jpeg_size(b: &[u8]) -> Option<(u32, u32)> {
    if b.len() < 4 || b[0] != 0xFF || b[1] != 0xD8 {
        return None;
    }
    let mut i = 2;
    while i + 9 < b.len() {
        if b[i] != 0xFF {
            i += 1;
            continue;
        }
        let m = b[i + 1];
        if m == 0xFF {
            i += 1;
            continue;
        }
        if m == 0xD8 || m == 0x01 || (0xD0..=0xD7).contains(&m) {
            i += 2;
            continue;
        }
        let len = u16::from_be_bytes([b[i + 2], b[i + 3]]) as usize;
        if matches!(m, 0xC0..=0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF) {
            let h = u16::from_be_bytes([b[i + 5], b[i + 6]]) as u32;
            let w = u16::from_be_bytes([b[i + 7], b[i + 8]]) as u32;
            return Some((w, h));
        }
        i += 2 + len;
    }
    None
}

pub fn find_ffmpeg() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("LB_FFMPEG") {
        if !p.is_empty() {
            return Some(PathBuf::from(p));
        }
    }
    let exe = if cfg!(windows) { "ffmpeg.exe" } else { "ffmpeg" };
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|d| d.join(exe)).find(|p| p.is_file())
}

/// ffmpeg concat-demuxer script (the last file is repeated, per its docs).
pub fn concat_script(frames: &[Frame], durs: &[f64]) -> String {
    let mut s = String::from("ffconcat version 1.0\n");
    for (f, d) in frames.iter().zip(durs) {
        s += &format!("file '{}'\nduration {d:.3}\n", f.file.file_name().unwrap().to_string_lossy());
    }
    if let Some(f) = frames.last() {
        s += &format!("file '{}'\n", f.file.file_name().unwrap().to_string_lossy());
    }
    s
}

async fn encode_mp4(ffmpeg: &Path, dir: &Path, frames: &[Frame], durs: &[f64], fps: f64, size: (u32, u32), out: &Path) -> Result<()> {
    let list = dir.join("frames.ffconcat");
    tokio::fs::write(&list, concat_script(frames, durs)).await?;
    // Even dimensions for yuv420p; letterbox frames from mid-recording resizes.
    let (w, h) = (size.0 & !1, size.1 & !1);
    let vf = format!(
        "scale={w}:{h}:force_original_aspect_ratio=decrease,pad={w}:{h}:(ow-iw)/2:(oh-ih)/2,fps={}",
        fps.ceil().max(1.0)
    );
    let o = tokio::process::Command::new(ffmpeg)
        .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "concat", "-safe", "0", "-i"])
        .arg(&list)
        .args(["-vf", &vf, "-c:v", "libx264", "-pix_fmt", "yuv420p", "-movflags", "+faststart"])
        .arg(out)
        .output()
        .await
        .context("running ffmpeg")?;
    if !o.status.success() {
        anyhow::bail!("ffmpeg failed: {}", String::from_utf8_lossy(&o.stderr).trim());
    }
    Ok(())
}

/// Minimal RIFF/AVI writer with MJPEG frames at a constant `fps`.
pub fn write_avi(jpegs: &[&[u8]], fps: u32, size: (u32, u32)) -> Vec<u8> {
    fn u32le(v: &mut Vec<u8>, x: u32) {
        v.extend_from_slice(&x.to_le_bytes());
    }
    fn u16le(v: &mut Vec<u8>, x: u16) {
        v.extend_from_slice(&x.to_le_bytes());
    }
    fn chunk(v: &mut Vec<u8>, id: &[u8; 4], body: &[u8]) {
        v.extend_from_slice(id);
        u32le(v, body.len() as u32);
        v.extend_from_slice(body);
        if body.len() % 2 == 1 {
            v.push(0);
        }
    }
    fn list(v: &mut Vec<u8>, kind: &[u8; 4], body: &[u8]) {
        let mut b = kind.to_vec();
        b.extend_from_slice(body);
        chunk(v, b"LIST", &b);
    }
    let n = jpegs.len() as u32;
    let (w, h) = size;
    let max_frame = jpegs.iter().map(|j| j.len()).max().unwrap_or(0) as u32;

    let mut avih = vec![];
    u32le(&mut avih, 1_000_000 / fps.max(1)); // us per frame
    u32le(&mut avih, max_frame * fps); // max bytes/sec
    u32le(&mut avih, 0); // padding granularity
    u32le(&mut avih, 0x10); // AVIF_HASINDEX
    u32le(&mut avih, n);
    u32le(&mut avih, 0); // initial frames
    u32le(&mut avih, 1); // streams
    u32le(&mut avih, max_frame);
    u32le(&mut avih, w);
    u32le(&mut avih, h);
    avih.extend_from_slice(&[0; 16]);

    let mut strh = vec![];
    strh.extend_from_slice(b"vidsMJPG");
    u32le(&mut strh, 0); // flags
    u16le(&mut strh, 0); // priority
    u16le(&mut strh, 0); // language
    u32le(&mut strh, 0); // initial frames
    u32le(&mut strh, 1); // scale
    u32le(&mut strh, fps.max(1)); // rate
    u32le(&mut strh, 0); // start
    u32le(&mut strh, n); // length
    u32le(&mut strh, max_frame);
    u32le(&mut strh, u32::MAX); // quality
    u32le(&mut strh, 0); // sample size
    u16le(&mut strh, 0);
    u16le(&mut strh, 0);
    u16le(&mut strh, w as u16);
    u16le(&mut strh, h as u16);

    let mut strf = vec![];
    u32le(&mut strf, 40);
    u32le(&mut strf, w);
    u32le(&mut strf, h);
    u16le(&mut strf, 1);
    u16le(&mut strf, 24);
    strf.extend_from_slice(b"MJPG");
    u32le(&mut strf, w * h * 3);
    strf.extend_from_slice(&[0; 16]);

    let mut strl = vec![];
    chunk(&mut strl, b"strh", &strh);
    chunk(&mut strl, b"strf", &strf);
    let mut hdrl = vec![];
    chunk(&mut hdrl, b"avih", &avih);
    list(&mut hdrl, b"strl", &strl);

    let mut movi = vec![];
    let mut idx = vec![];
    for j in jpegs {
        let off = movi.len() as u32 + 4; // offset from the 'movi' fourcc
        idx.extend_from_slice(b"00dc");
        u32le(&mut idx, 0x10); // keyframe
        u32le(&mut idx, off);
        u32le(&mut idx, j.len() as u32);
        chunk(&mut movi, b"00dc", j);
    }

    let mut body = b"AVI ".to_vec();
    list(&mut body, b"hdrl", &hdrl);
    list(&mut body, b"movi", &movi);
    chunk(&mut body, b"idx1", &idx);
    let mut out = vec![];
    chunk(&mut out, b"RIFF", &body);
    out
}

async fn encode_avi(frames: &[Frame], durs: &[f64], fps: f64, size: (u32, u32), out: &Path) -> Result<()> {
    let fps = fps.round().max(1.0) as u32;
    let mut cache: std::collections::HashMap<PathBuf, Vec<u8>> = Default::default();
    for f in frames {
        if !cache.contains_key(&f.file) {
            cache.insert(f.file.clone(), tokio::fs::read(&f.file).await?);
        }
    }
    // Constant frame rate: repeat each frame for its real duration.
    let mut seq: Vec<&[u8]> = vec![];
    let mut clock = 0.0f64;
    let mut emitted = 0u64;
    for (f, d) in frames.iter().zip(durs) {
        clock += d;
        let target = ((clock * fps as f64).round() as u64).max(emitted + 1);
        for _ in emitted..target {
            seq.push(&cache[&f.file]);
        }
        emitted = target;
    }
    tokio::fs::write(out, write_avi(&seq, fps, size)).await?;
    Ok(())
}

/// Encode frames in `dir`; returns (path, mime, encoder note).
pub async fn encode(dir: &Path, frames: &[Frame], fps: f64) -> Result<(PathBuf, &'static str, String)> {
    let first = frames.first().context("no frames captured")?;
    let head = tokio::fs::read(&first.file).await?;
    let size = jpeg_size(&head).context("could not read frame size")?;
    let durs = durations(frames, fps);
    if let Some(ff) = find_ffmpeg() {
        let out = dir.join("recording.mp4");
        match encode_mp4(&ff, dir, frames, &durs, fps, size, &out).await {
            Ok(()) => return Ok((out, "video/mp4", "ffmpeg (h264)".into())),
            Err(e) => tracing::warn!("mp4 encode failed, falling back to AVI: {e:#}"),
        }
    }
    let out = dir.join("recording.avi");
    encode_avi(frames, &durs, fps, size, &out).await?;
    Ok((out, "video/x-msvideo", "built-in MJPEG AVI (install ffmpeg for MP4)".into()))
}

/// Remove intermediate frames once the video exists.
pub async fn cleanup_frames(dir: &Path) {
    if let Ok(mut rd) = tokio::fs::read_dir(dir).await {
        while let Ok(Some(e)) = rd.next_entry().await {
            let p = e.path();
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if name.ends_with(".jpg") || name.ends_with(".ffconcat") {
                let _ = tokio::fs::remove_file(&p).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 2x1 baseline JPEG header fragment: SOI, APP0, SOF0.
    fn fake_jpeg(w: u16, h: u16) -> Vec<u8> {
        let mut v = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x04, 0x00, 0x00];
        v.extend_from_slice(&[0xFF, 0xC0, 0x00, 0x11, 0x08]);
        v.extend_from_slice(&h.to_be_bytes());
        v.extend_from_slice(&w.to_be_bytes());
        v.extend_from_slice(&[0x03, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xFF, 0xD9]);
        v
    }

    #[test]
    fn reads_jpeg_size() {
        assert_eq!(jpeg_size(&fake_jpeg(1280, 900)), Some((1280, 900)));
        assert_eq!(jpeg_size(b"nope"), None);
    }

    #[test]
    fn durations_follow_timestamps() {
        let f = |t| Frame { file: PathBuf::from("a.jpg"), t_ms: t };
        let d = durations(&[f(0), f(200), f(1200)], 5.0);
        assert_eq!(d, vec![0.2, 1.0, 0.2]);
    }

    #[test]
    fn concat_repeats_last() {
        let f = |n: &str, t| Frame { file: PathBuf::from(format!("/x/{n}")), t_ms: t };
        let s = concat_script(&[f("a.jpg", 0), f("b.jpg", 500)], &[0.5, 0.2]);
        assert!(s.ends_with("file 'b.jpg'\nduration 0.200\nfile 'b.jpg'\n"), "{s}");
    }

    #[test]
    fn avi_structure() {
        let j = fake_jpeg(4, 2);
        let avi = write_avi(&[&j, &j, &j], 5, (4, 2));
        assert_eq!(&avi[0..4], b"RIFF");
        assert_eq!(u32::from_le_bytes(avi[4..8].try_into().unwrap()) as usize, avi.len() - 8);
        assert_eq!(&avi[8..12], b"AVI ");
        assert!(avi.windows(4).any(|w| w == b"idx1"));
        assert_eq!(avi.windows(4).filter(|w| *w == b"00dc").count(), 6); // 3 chunks + 3 index entries
    }

    #[test]
    fn clamps() {
        assert_eq!(clamp_fps(None), DEFAULT_FPS);
        assert_eq!(clamp_fps(Some(100.0)), MAX_FPS);
        assert_eq!(clamp_max_secs(Some(0)), DEFAULT_MAX_SECS);
        assert_eq!(clamp_max_secs(Some(99999)), HARD_MAX_SECS);
    }
}
