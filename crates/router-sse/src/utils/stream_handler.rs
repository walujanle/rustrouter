//! Disconnect detection and the stall watchdog that wraps the SSE transform.
//!
//! Abort and disconnect both run through one [`CancellationToken`], so the
//! whole surface is [`pipe_with_disconnect`]: it taps the *raw* upstream bytes
//! for stall activity, pipes them through the transform, and emits the caller's
//! terminal bytes when the stream aborts or stalls.
//!
//! Two behaviours are load-bearing and easy to lose in a rewrite:
//!
//! * **The stall watchdog watches raw upstream bytes, not transform output.** A
//!   slow translator that is silent for a long stretch is not a stalled
//!   provider, and measuring the output produced false stalls.
//! * **Terminal bytes are emitted on abort and on a network close, never on a
//!   normal end-of-stream.** A completed stream must not be followed by a
//!   synthetic error frame.

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use bytes::Bytes;
use futures::StreamExt;
use tokio_util::sync::CancellationToken;

use crate::executors::executor::ByteStream;

/// The idle keepalive frame. An SSE comment line is ignored by every
/// spec-compliant parser (including the Anthropic and OpenAI SDKs), so it needs
/// no knowledge of the client's wire format — unlike a data-bearing `ping`
/// event, which only one of them would accept.
const SSE_KEEPALIVE_FRAME: &[u8] = b": keepalive\n\n";

/// Builds the terminal frame emitted after HTTP 200 was already sent, given a
/// human-readable abort reason.
pub type AbortTerminalFn = Arc<dyn Fn(&str) -> Vec<u8> + Send + Sync>;

/// Pipes the raw upstream bytes through the SSE transform with a stall
/// watchdog and a disconnect path.
///
/// `input` is the raw upstream byte stream. `transform` is the
/// `Fn(ByteStream) -> ByteStream` lift of the SSE transform
/// ([`create_sse_stream`](crate::utils::stream::create_sse_stream)), applied
/// once. `cancel` is the
/// disconnect signal — cancelling it ends the stream with the terminal bytes.
/// `stall_timeout_ms` is the inter-chunk budget on raw upstream bytes; `0`
/// disables the watchdog. `keepalive_ms` is the idle interval after which a
/// comment frame is written; `0` disables it.
pub fn pipe_with_disconnect(
    input: ByteStream,
    transform: impl Fn(ByteStream) -> ByteStream,
    cancel: Option<CancellationToken>,
    on_abort_terminal: Option<AbortTerminalFn>,
    stall_timeout_ms: u64,
    keepalive_ms: u64,
) -> ByteStream {
    // The internal token every abort path funnels through: the watchdog
    // cancels it directly, and the caller's disconnect signal is selected
    // against alongside it. Bridging the two with a spawned task would race
    // the first poll, and a disconnect that arrives before the stream starts
    // would leak one upstream chunk.
    let abort = CancellationToken::new();

    // Tap raw upstream bytes: forward each chunk and stamp the activity clock.
    let last_activity = Arc::new(AtomicI64::new(router_db::time::now_ms()));
    let tapped = {
        let last_activity = Arc::clone(&last_activity);
        let stream: ByteStream = Box::pin(async_stream::stream! {
            let mut input = input;
            while let Some(chunk) = input.next().await {
                last_activity.store(router_db::time::now_ms(), Ordering::Relaxed);
                yield chunk;
            }
        });
        stream
    };
    let transformed = transform(tapped);

    if stall_timeout_ms > 0 {
        let watchdog_abort = abort.clone();
        let last_activity = Arc::clone(&last_activity);
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                loop {
                    tokio::select! {
                        _ = watchdog_abort.cancelled() => break,
                        _ = tokio::time::sleep(Duration::from_millis(stall_timeout_ms)) => {
                            let idle = router_db::time::now_ms() - last_activity.load(Ordering::Relaxed);
                            if idle >= stall_timeout_ms as i64 {
                                watchdog_abort.cancel();
                                break;
                            }
                        }
                    }
                }
            });
        }
    }

    let stream: ByteStream = Box::pin(async_stream::stream! {
        let mut transformed = transformed;
        let mut aborted = false;
        loop {
            // An already-cancelled disconnect must win over a ready chunk:
            // `biased` select would otherwise drain one upstream frame first.
            if abort.is_cancelled() || cancel.as_ref().is_some_and(CancellationToken::is_cancelled) {
                aborted = true;
                break;
            }
            tokio::select! {
                biased;
                item = transformed.next() => match item {
                    Some(Ok(bytes)) => yield Ok(bytes),
                    Some(Err(error)) => {
                        // A transport reset is a graceful close: emit the
                        // structured terminal when one is available, else
                        // surface the error. Not marked aborted — the
                        // post-loop terminal would then be emitted twice.
                        if let Some(terminal) = &on_abort_terminal {
                            yield Ok(Bytes::from(terminal("upstream connection lost")));
                        } else {
                            yield Err(error);
                        }
                        break;
                    }
                    None => break,
                },
                _ = abort.cancelled() => {
                    aborted = true;
                    break;
                }
                _ = wait_cancelled(&cancel) => {
                    aborted = true;
                    break;
                }
                // No upstream byte for a full keepalive interval: write a
                // comment frame so the client's idle timeout does not abort a
                // request that is merely waiting on a slow provider. The timer
                // is recreated each pass, so it measures idle time, not total
                // elapsed time.
                _ = keepalive_tick(keepalive_ms) => {
                    yield Ok(Bytes::from_static(SSE_KEEPALIVE_FRAME));
                }
            }
        }
        if aborted && let Some(terminal) = &on_abort_terminal {
            yield Ok(Bytes::from(terminal("stream aborted")));
        }
        abort.cancel();
    });
    stream
}

/// Resolves when the caller's disconnect token fires, and never otherwise.
async fn wait_cancelled(cancel: &Option<CancellationToken>) {
    match cancel {
        Some(token) => token.cancelled().await,
        None => std::future::pending::<()>().await,
    }
}

/// The keepalive timer, or a future that never resolves when keepalive is off.
/// A `select!` arm that never fires is how the `0` case is spelled without a
/// second branch of the loop body.
async fn keepalive_tick(keepalive_ms: u64) {
    match keepalive_ms {
        0 => std::future::pending::<()>().await,
        ms => tokio::time::sleep(Duration::from_millis(ms)).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn byte_stream(chunks: Vec<&'static [u8]>) -> ByteStream {
        Box::pin(futures::stream::iter(
            chunks
                .into_iter()
                .map(|c| Ok(Bytes::from_static(c)))
                .collect::<Vec<_>>(),
        ))
    }

    async fn drain(mut stream: ByteStream) -> Vec<u8> {
        let mut out = Vec::new();
        while let Some(chunk) = stream.next().await {
            out.extend_from_slice(&chunk.unwrap());
        }
        out
    }

    #[tokio::test]
    async fn passthrough_transform_emits_the_upstream_bytes() {
        let out = drain(pipe_with_disconnect(
            byte_stream(vec![b"a", b"b"]),
            |s| s,
            None,
            None,
            0,
            0,
        ))
        .await;
        assert_eq!(out, b"ab");
    }

    #[tokio::test]
    async fn a_cancelled_stream_emits_the_terminal_frame() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let terminal: AbortTerminalFn = Arc::new(|msg| format!("terminal:{msg}").into_bytes());
        let out = drain(pipe_with_disconnect(
            byte_stream(vec![b"a"]),
            |s| s,
            Some(cancel),
            Some(terminal),
            0,
            0,
        ))
        .await;
        assert_eq!(out, b"terminal:stream aborted");
    }

    #[tokio::test]
    async fn a_completed_stream_never_emits_the_terminal_frame() {
        let terminal: AbortTerminalFn = Arc::new(|_| b"SHOULD NOT APPEAR".to_vec());
        let out = drain(pipe_with_disconnect(
            byte_stream(vec![b"done"]),
            |s| s,
            None,
            Some(terminal),
            0,
            0,
        ))
        .await;
        assert_eq!(out, b"done");
    }

    #[tokio::test]
    async fn a_stalled_stream_aborts_after_the_timeout() {
        // A stream that never yields and never ends: the watchdog must fire.
        let terminal: AbortTerminalFn = Arc::new(|_| b"STALL".to_vec());
        let stalled: ByteStream = Box::pin(futures::stream::pending());
        let out = drain(pipe_with_disconnect(
            stalled,
            |s| s,
            None,
            Some(terminal),
            20,
            0,
        ))
        .await;
        assert_eq!(out, b"STALL");
    }

    #[tokio::test]
    async fn an_idle_upstream_emits_keepalive_frames() {
        // A stream that ends after 100ms with no chunks: the keepalive must
        // fire at least twice before the stream ends.
        let slow_end: ByteStream = Box::pin(async_stream::stream! {
            tokio::time::sleep(Duration::from_millis(100)).await;
            yield Ok(Bytes::from_static(b"late"));
        });
        let out = drain(pipe_with_disconnect(slow_end, |s| s, None, None, 0, 30)).await;
        let text = String::from_utf8(out).unwrap();
        let keepalives = text.matches(": keepalive\n\n").count();
        assert!(keepalives >= 2, "expected keepalives, got {text:?}");
        assert!(text.ends_with("late"), "{text:?}");
    }

    #[tokio::test]
    async fn keepalive_is_off_when_the_interval_is_zero() {
        let slow_end: ByteStream = Box::pin(async_stream::stream! {
            tokio::time::sleep(Duration::from_millis(40)).await;
            yield Ok(Bytes::from_static(b"x"));
        });
        let out = drain(pipe_with_disconnect(slow_end, |s| s, None, None, 0, 0)).await;
        assert_eq!(out, b"x");
    }
}
