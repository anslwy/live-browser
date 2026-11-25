//! Cross-origin (out-of-process) iframe support.
//!
//! Chrome renders cross-site `<iframe>`s in their own renderer process
//! ("OOPIF"). Their documents are invisible to the top frame's JS and to the
//! tab's CDP session, so `snapshot`/`click`/`type`/`select`/`wait` need a CDP
//! session *per frame*. chromiumoxide auto-attaches those child sessions
//! internally but doesn't expose them, so this module opens its own
//! short-lived CDP websocket (chromiumoxide's public [`Connection`]):
//!
//! 1. `Target.attachToTarget {flatten}` to the tab's page target,
//! 2. `Target.setAutoAttach {flatten, waitForDebuggerOnStart: false}` on that
//!    session (and recursively on each child) — Chrome synchronously replies
//!    with one `Target.attachedToTarget` per existing child frame target,
//! 3. `DOM.getFrameOwner` + `DOM.getBoxModel` in the parent session to find
//!    where each `<iframe>`'s content box sits in the parent viewport.
//!
//! Dropping [`Oopifs`] closes the websocket, which detaches every session it
//! created — nothing leaks into the browser, and every call is timeout-bound
//! so a hung or closing frame can't stall an action.

use std::collections::VecDeque;
use std::time::Duration;

use chromiumoxide::cdp::browser_protocol::target::{EventAttachedToTarget, SessionId};
use chromiumoxide::cdp::{CdpEvent, CdpEventMessage};
use chromiumoxide::conn::Connection;
use chromiumoxide::types::Message;
use futures::StreamExt;
use serde_json::{json, Value};

/// Per-CDP-call budget. A frame that doesn't answer within this is skipped.
const CALL_TIMEOUT: Duration = Duration::from_secs(4);
/// Hard cap on frames walked (ad-heavy pages can nest dozens of iframes).
const MAX_FRAMES: usize = 32;

/// Minimal flat-mode CDP client over a dedicated websocket.
pub struct Cdp {
    conn: Connection<CdpEventMessage>,
    /// `(session that emitted it, event)` for every attach seen so far.
    attached: Vec<(Option<String>, Box<EventAttachedToTarget>)>,
}

impl Cdp {
    pub async fn connect(ws: &str) -> Result<Self, String> {
        let conn = tokio::time::timeout(CALL_TIMEOUT, Connection::<CdpEventMessage>::connect(ws))
            .await
            .map_err(|_| "CDP connect timed out".to_string())?
            .map_err(|e| format!("CDP connect failed: {e}"))?;
        Ok(Self { conn, attached: Vec::new() })
    }

    /// Send one command and wait for its response, buffering attach events.
    pub async fn call(&mut self, method: &str, session: Option<&str>, params: Value) -> Result<Value, String> {
        let id = self
            .conn
            .submit_command(method.to_string().into(), session.map(|s| SessionId::from(s.to_string())), params)
            .map_err(|e| format!("{method}: {e}"))?;
        let conn = &mut self.conn;
        let attached = &mut self.attached;
        let wait = async {
            loop {
                match conn.next().await {
                    None => return Err(format!("{method}: CDP connection closed")),
                    Some(Ok(Message::Response(r))) if r.id == id => {
                        if let Some(e) = r.error {
                            return Err(format!("{method}: {}", e.message));
                        }
                        return Ok(r.result.unwrap_or(Value::Null));
                    }
                    Some(Ok(Message::Event(ev))) => {
                        if let CdpEvent::TargetAttachedToTarget(a) = ev.params {
                            attached.push((ev.session_id, a));
                        }
                    }
                    // Other responses / undecodable events: keep reading.
                    Some(_) => {}
                }
            }
        };
        tokio::time::timeout(CALL_TIMEOUT, wait)
            .await
            .map_err(|_| format!("{method}: timed out"))?
    }

    /// `Runtime.evaluate` in a session's main world, returning the value.
    pub async fn eval(&mut self, session: &str, expr: &str) -> Result<Value, String> {
        let r = self
            .call(
                "Runtime.evaluate",
                Some(session),
                json!({ "expression": expr, "returnByValue": true, "awaitPromise": true }),
            )
            .await?;
        if let Some(ex) = r.get("exceptionDetails") {
            let msg = ex
                .pointer("/exception/description")
                .and_then(|v| v.as_str())
                .or_else(|| ex.get("text").and_then(|v| v.as_str()))
                .unwrap_or("exception");
            return Err(format!("frame script error: {msg}"));
        }
        Ok(r.pointer("/result/value").cloned().unwrap_or(Value::Null))
    }
}

/// One attached cross-origin frame.
#[derive(Debug, Clone, PartialEq)]
pub struct Frame {
    /// CDP session for this frame's target (on our private connection).
    pub session: String,
    /// Session that owns the `<iframe>` element (parent frame / tab).
    pub parent_session: String,
    /// Index of the parent OOPIF in [`Oopifs::frames`] (None = tab session).
    pub parent: Option<usize>,
    /// Frame/target id (for OOPIFs they are the same string).
    pub target_id: String,
    pub url: String,
    /// `<iframe>` element in the parent session (scroll it into view).
    pub owner_backend: Option<i64>,
    /// Iframe content-box origin in top-level viewport CSS px.
    pub offset: (f64, f64),
    /// Snapshot refs `[first_ref, first_ref + ref_count)` live in this frame.
    pub first_ref: u32,
    pub ref_count: u32,
}

/// Attached OOPIFs of one tab (keeps the private CDP connection open).
pub struct Oopifs {
    pub cdp: Cdp,
    pub frames: Vec<Frame>,
}

/// Only `iframe` targets are frames (auto-attach also reports workers).
pub fn is_frame_target(kind: &str) -> bool {
    kind == "iframe"
}

/// Top-left of a CDP quad (`[x1,y1, x2,y2, x3,y3, x4,y4]`).
pub fn quad_origin(quad: &[f64]) -> Option<(f64, f64)> {
    if quad.len() < 8 || quad.iter().any(|v| !v.is_finite()) {
        return None;
    }
    let x = quad.iter().step_by(2).cloned().fold(f64::INFINITY, f64::min);
    let y = quad.iter().skip(1).step_by(2).cloned().fold(f64::INFINITY, f64::min);
    Some((x, y))
}

/// Iframe-local CSS point -> top-level viewport CSS point.
pub fn translate(local: (f64, f64), offset: (f64, f64)) -> (f64, f64) {
    (local.0 + offset.0, local.1 + offset.1)
}

/// Index of the frame owning snapshot ref `r`, if it's an OOPIF ref.
pub fn owner_of(frames: &[Frame], r: u32) -> Option<usize> {
    frames
        .iter()
        .position(|f| f.ref_count > 0 && r >= f.first_ref && r < f.first_ref + f.ref_count)
}

/// Snapshot `frame` label for elements of OOPIF `i`: `oopif[i] <origin>`.
pub fn frame_label(i: usize, url: &str) -> String {
    let origin = match url.find("://") {
        Some(p) => {
            let rest = &url[p + 3..];
            let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
            &url[..p + 3 + end]
        }
        None => url,
    };
    format!("oopif[{i}] {origin}")
}

/// Frames from the tab root down to `idx` (inclusive), top-most first.
pub fn chain(frames: &[Frame], idx: usize) -> Vec<usize> {
    let mut out = vec![idx];
    let mut cur = frames[idx].parent;
    while let Some(p) = cur {
        if out.contains(&p) {
            break; // defensive: malformed parent links
        }
        out.push(p);
        cur = frames[p].parent;
    }
    out.reverse();
    out
}

/// `(backendNodeId of <iframe>, content-box origin in the parent viewport)`.
async fn frame_offset(cdp: &mut Cdp, parent_session: &str, frame_id: &str) -> Option<(i64, (f64, f64))> {
    let owner = match cdp.call("DOM.getFrameOwner", Some(parent_session), json!({ "frameId": frame_id })).await {
        Ok(v) => v,
        Err(_) => {
            // Some builds want the DOM agent enabled first.
            cdp.call("DOM.enable", Some(parent_session), json!({})).await.ok()?;
            cdp.call("DOM.getFrameOwner", Some(parent_session), json!({ "frameId": frame_id })).await.ok()?
        }
    };
    let backend = owner.get("backendNodeId")?.as_i64()?;
    let model = cdp
        .call("DOM.getBoxModel", Some(parent_session), json!({ "backendNodeId": backend }))
        .await
        .ok()?;
    let quad: Vec<f64> = serde_json::from_value(model.pointer("/model/content")?.clone()).ok()?;
    Some((backend, quad_origin(&quad)?))
}

/// Attach to every OOPIF (recursively) of the page target `page_target`.
pub async fn discover(ws: &str, page_target: &str) -> Result<Oopifs, String> {
    let mut cdp = Cdp::connect(ws).await?;
    let root = cdp
        .call("Target.attachToTarget", None, json!({ "targetId": page_target, "flatten": true }))
        .await?
        .get("sessionId")
        .and_then(|v| v.as_str())
        .ok_or("attachToTarget returned no sessionId")?
        .to_string();
    let mut frames: Vec<Frame> = Vec::new();
    let mut queue: VecDeque<(String, Option<usize>)> = VecDeque::from([(root, None)]);
    while let Some((sess, parent)) = queue.pop_front() {
        let auto = json!({ "autoAttach": true, "waitForDebuggerOnStart": false, "flatten": true });
        if cdp.call("Target.setAutoAttach", Some(&sess), auto).await.is_err() {
            continue;
        }
        // Round-trip once more so any attach events are surely buffered.
        let _ = cdp.eval(&sess, "0").await;
        let kids: Vec<Box<EventAttachedToTarget>> = {
            let (mine, rest): (Vec<_>, Vec<_>) =
                std::mem::take(&mut cdp.attached).into_iter().partition(|(s, _)| s.as_deref() == Some(sess.as_str()));
            cdp.attached = rest;
            mine.into_iter()
                .map(|(_, ev)| ev)
                .filter(|ev| is_frame_target(&ev.target_info.r#type))
                .collect()
        };
        for kid in kids {
            if frames.len() >= MAX_FRAMES {
                break;
            }
            let tid: String = kid.target_info.target_id.as_ref().to_string();
            let base = parent.map(|i| frames[i].offset).unwrap_or((0.0, 0.0));
            // Unrendered (display:none / detached) frames have no box: skip.
            let Some((backend, off)) = frame_offset(&mut cdp, &sess, &tid).await else { continue };
            let session: String = kid.session_id.as_ref().to_string();
            frames.push(Frame {
                session: session.clone(),
                parent_session: sess.clone(),
                parent,
                target_id: tid,
                url: kid.target_info.url.clone(),
                owner_backend: Some(backend),
                offset: translate(off, base),
                first_ref: 0,
                ref_count: 0,
            });
            queue.push_back((session, Some(frames.len() - 1)));
        }
    }
    Ok(Oopifs { cdp, frames })
}

impl Oopifs {
    /// Run the snapshot collector (`snapshot_fn(start, max, dx, dy, label)`)
    /// in every frame, continuing ref numbering at `next_ref`. Frames past
    /// the `budget` still run with max=0 so stale tags get cleared.
    pub async fn snapshot(&mut self, snapshot_fn: &str, mut next_ref: u32, mut budget: usize) -> Vec<Value> {
        let mut out = Vec::new();
        for i in 0..self.frames.len() {
            let f = &self.frames[i];
            let label = frame_label(i, &f.url);
            let expr = format!(
                "{snapshot_fn}({next_ref}, {budget}, {}, {}, {})",
                f.offset.0,
                f.offset.1,
                serde_json::to_string(&label).unwrap()
            );
            let session = f.session.clone();
            let (els, next) = match self.cdp.eval(&session, &expr).await {
                Ok(v) => (
                    v.get("elements").and_then(|e| e.as_array()).cloned().unwrap_or_default(),
                    v.get("next").and_then(|n| n.as_u64()).map(|n| n as u32).unwrap_or(next_ref),
                ),
                Err(e) => {
                    tracing::debug!("oopif snapshot in {} failed: {e}", f.url);
                    (Vec::new(), next_ref)
                }
            };
            let f = &mut self.frames[i];
            f.first_ref = next_ref;
            f.ref_count = next.saturating_sub(next_ref);
            next_ref = next.max(next_ref);
            budget = budget.saturating_sub(els.len());
            out.extend(els);
        }
        out
    }

    /// Evaluate `expr` in frame `idx`.
    pub async fn eval(&mut self, idx: usize, expr: &str) -> Result<Value, String> {
        let session = self.frames[idx].session.clone();
        self.cdp.eval(&session, expr).await
    }

    /// Scroll ref `r` of frame `idx` into view (iframe chain first, then the
    /// element inside it) and return its center in top-level viewport CSS px.
    /// `locate_js` is `(ref) => [x, y] | null` in frame-local coordinates.
    pub async fn element_point(&mut self, idx: usize, locate_js: &str, r: u32) -> Result<(f64, f64), String> {
        let path = chain(&self.frames, idx);
        for &i in &path {
            if let Some(backend) = self.frames[i].owner_backend {
                let parent = self.frames[i].parent_session.clone();
                let _ = self
                    .cdp
                    .call("DOM.scrollIntoViewIfNeeded", Some(&parent), json!({ "backendNodeId": backend }))
                    .await;
            }
        }
        let local: Option<Vec<f64>> = serde_json::from_value(self.eval(idx, &format!("{locate_js}({r})")).await?).ok();
        let local = match local {
            Some(p) if p.len() == 2 && p.iter().all(|v| v.is_finite()) => (p[0], p[1]),
            _ => return Err(format!("element not found for ref {r} inside cross-origin iframe (run `lb snapshot` again)")),
        };
        // Scrolling moved things: recompute each iframe offset top-down.
        let mut base = (0.0, 0.0);
        for &i in &path {
            let (parent, tid) = (self.frames[i].parent_session.clone(), self.frames[i].target_id.clone());
            if let Some((_, off)) = frame_offset(&mut self.cdp, &parent, &tid).await {
                self.frames[i].offset = translate(off, base);
            }
            base = self.frames[i].offset;
        }
        Ok(translate(local, base))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(parent: Option<usize>, first: u32, count: u32) -> Frame {
        Frame {
            session: "s".into(),
            parent_session: "p".into(),
            parent,
            target_id: "t".into(),
            url: "https://x.test/".into(),
            owner_backend: None,
            offset: (0.0, 0.0),
            first_ref: first,
            ref_count: count,
        }
    }

    #[test]
    fn quad_origin_takes_min_corner() {
        assert_eq!(quad_origin(&[10.0, 20.0, 110.0, 20.0, 110.0, 70.0, 10.0, 70.0]), Some((10.0, 20.0)));
        // Rotated/unordered quads still yield the top-left extent.
        assert_eq!(quad_origin(&[110.0, 70.0, 10.0, 70.0, 10.0, 20.0, 110.0, 20.0]), Some((10.0, 20.0)));
        assert_eq!(quad_origin(&[1.0, 2.0]), None);
        assert_eq!(quad_origin(&[f64::NAN, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]), None);
    }

    #[test]
    fn translate_adds_nested_offsets() {
        // Outer iframe content box at (100, 50); inner at (12, 8) inside it.
        let outer = translate((100.0, 50.0), (0.0, 0.0));
        let inner = translate((12.0, 8.0), outer);
        assert_eq!(translate((30.5, 10.0), inner), (142.5, 68.0));
    }

    #[test]
    fn owner_of_routes_refs_by_range() {
        let frames = vec![frame(None, 5, 3), frame(Some(0), 8, 0), frame(None, 8, 2)];
        assert_eq!(owner_of(&frames, 4), None); // main-frame ref
        assert_eq!(owner_of(&frames, 5), Some(0));
        assert_eq!(owner_of(&frames, 7), Some(0));
        assert_eq!(owner_of(&frames, 8), Some(2)); // empty frame never owns
        assert_eq!(owner_of(&frames, 9), Some(2));
        assert_eq!(owner_of(&frames, 10), None);
    }

    #[test]
    fn chain_walks_to_root() {
        let frames = vec![frame(None, 0, 0), frame(Some(0), 0, 0), frame(Some(1), 0, 0)];
        assert_eq!(chain(&frames, 2), vec![0, 1, 2]);
        assert_eq!(chain(&frames, 0), vec![0]);
    }

    #[test]
    fn frame_filter_and_labels() {
        assert!(is_frame_target("iframe"));
        assert!(!is_frame_target("worker"));
        assert!(!is_frame_target("service_worker"));
        assert!(!is_frame_target("page"));
        assert_eq!(frame_label(0, "https://js-eu1.hsforms.net/embed/v3?x=1"), "oopif[0] https://js-eu1.hsforms.net");
        assert_eq!(frame_label(2, "http://127.0.0.1:9000"), "oopif[2] http://127.0.0.1:9000");
        assert_eq!(frame_label(1, "about:blank"), "oopif[1] about:blank");
    }
}
