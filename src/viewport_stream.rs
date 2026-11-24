use axum::body::{Body, Bytes};
use base64::Engine;
use chromiumoxide::cdp::browser_protocol::page::{
    EventScreencastFrame, EventScreencastVisibilityChanged, ScreencastFrameAckParams,
    StartScreencastFormat, StartScreencastParams, StopScreencastParams,
};
use chromiumoxide::page::Page;
use futures::StreamExt;
use std::convert::Infallible;
use std::sync::{Arc, Weak};
use std::time::Duration;
use tokio::sync::{oneshot, watch};

const CDP_BUDGET: Duration = Duration::from_secs(3);
const HEARTBEAT: Duration = Duration::from_secs(5);
const FRAME_INTERVAL: Duration = Duration::from_millis(50);
const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

/// One screencast per tab. Dropping the tab cancels capture and closes its
/// viewers, including when a browser reset reuses the numeric tab id.
#[derive(Default)]
pub(crate) struct ViewportStream {
    frames: Weak<watch::Sender<Bytes>>,
    stop: Option<oneshot::Sender<()>>,
}

impl ViewportStream {
    pub(crate) fn subscribe(&mut self, page: Page, shutdown: watch::Receiver<bool>) -> Body {
        let mut frames = if let Some(active) = self.frames.upgrade() {
            active.subscribe()
        } else {
            let (sender, receiver) = watch::channel(Bytes::new());
            let sender = Arc::new(sender);
            self.frames = Arc::downgrade(&sender);
            let (stop, stopped) = oneshot::channel();
            self.stop = Some(stop);
            tokio::spawn(capture(page, sender, stopped));
            receiver
        };
        frames.mark_changed(); // New viewers can show a cached frame immediately.
        Body::from_stream(futures::stream::unfold(
            (frames, shutdown, tokio::time::Instant::now()),
            |(mut frames, mut shutdown, mut next_frame)| async move {
                loop {
                    if *shutdown.borrow() {
                        return None;
                    }
                    let packet = tokio::select! {
                        biased;
                        _ = shutdown.changed() => return None,
                        result = frames.changed() => {
                            result.ok()?;
                            tokio::select! {
                                _ = shutdown.changed() => return None,
                                _ = tokio::time::sleep_until(next_frame) => {}
                            }
                            let frame = frames.borrow_and_update().clone();
                            if frame.is_empty() {
                                continue;
                            }
                            next_frame = tokio::time::Instant::now() + FRAME_INTERVAL;
                            frame
                        }
                        _ = tokio::time::sleep(HEARTBEAT) => Bytes::from_static(&[0; 4]),
                    };
                    return Some((Ok::<_, Infallible>(packet), (frames, shutdown, next_frame)));
                }
            },
        ))
    }
}

async fn capture(
    page: Page,
    frames: Arc<watch::Sender<Bytes>>,
    mut stopped: oneshot::Receiver<()>,
) {
    let stream = async {
        // Listen before starting so we keep the initial frame of a static page.
        let mut events = page.event_listener::<EventScreencastFrame>().await?;
        let visibility = page.event_listener::<EventScreencastVisibilityChanged>().await?;
        let params = StartScreencastParams::builder()
            .format(StartScreencastFormat::Jpeg)
            .quality(70)
            .max_width(1920)
            .max_height(1440)
            .build();
        page.execute(params).await?;
        Ok::<_, chromiumoxide::error::CdpError>((events.next().await, events, visibility))
    };
    let first = tokio::select! {
        _ = &mut stopped => None,
        _ = frames.closed() => None,
        result = tokio::time::timeout(CDP_BUDGET, stream) => match result {
            Ok(Ok((Some(frame), events, visibility))) => Some((frame, events, visibility)),
            _ => None,
        },
    };

    if let Some((mut frame, mut events, visibility)) = first {
        let mut hidden = visibility.filter(|event| std::future::ready(!event.visible));
        loop {
            // Chrome limits outstanding frames until they are acknowledged.
            // Acknowledgements depend only on capture, never on viewer speed.
            let ack = page.execute(ScreencastFrameAckParams::new(frame.session_id));
            if !matches!(tokio::time::timeout(CDP_BUDGET, ack).await, Ok(Ok(_))) {
                break;
            }
            if let Some(packet) = packet(frame.data.as_ref()) {
                frames.send_replace(packet);
            }
            tokio::select! {
                _ = &mut stopped => break,
                _ = frames.closed() => break,
                _ = hidden.next() => break, // Use screenshots for background targets.
                next = events.next() => match next {
                    Some(next) => frame = next,
                    None => break,
                }
            }
        }
    }
    let _ = tokio::time::timeout(CDP_BUDGET, page.execute(StopScreencastParams::default())).await;
}

fn packet(encoded: &str) -> Option<Bytes> {
    if encoded.len() > MAX_FRAME_BYTES.div_ceil(3) * 4 {
        return None;
    }
    let mut packet = vec![0; 4];
    base64::engine::general_purpose::STANDARD.decode_vec(encoded, &mut packet).ok()?;
    let len = packet.len() - 4;
    if len == 0 || len > MAX_FRAME_BYTES {
        return None;
    }
    packet[..4].copy_from_slice(&(len as u32).to_be_bytes());
    Some(Bytes::from(packet))
}
