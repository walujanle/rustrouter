//! The console-log ring buffer and its fan-out.
//!
//! Every log call in the process should land in the buffer, so the boundary
//! hook is [`ConsoleLogLayer`]: a `tracing` layer that captures every event's
//! `message` field and pushes it through [`append_line`]. It is registered in
//! the binary's `init_tracing` under its own filter, independent of the terminal
//! log level, so a request shows up on the Console Log page without any code in
//! the request path calling the logger explicitly.
//!
//! The buffer is a process-global so the SSE stream and the GET/DELETE routes
//! see the same one.

use std::sync::{LazyLock, Mutex, OnceLock};
use std::time::Duration;

use serde_json::Value;
use tokio::sync::broadcast;

/// Lines kept in the ring buffer; older ones are dropped.
const MAX_LINES: usize = 200;
/// How often the ticker drains the pending batch.
const FLUSH_INTERVAL_MS: u64 = 100;
/// Pending lines that force an immediate flush.
const MAX_BATCH_LINES: usize = 50;

/// `/\x1b\[[0-9;]*m/g`: terminal colors must not bleed into the UI.
static ANSI_RE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"\x1b\[[0-9;]*m").expect("valid ANSI pattern"));

/// What the buffer fans out. Nothing emits a single line today, but the stream
/// route still listens for it, so the variant is kept for that contract.
#[derive(Debug, Clone)]
pub enum ConsoleEvent {
    Line(String),
    Lines(Vec<String>),
    Clear,
}

struct Buf {
    logs: Vec<String>,
    pending: Vec<String>,
    flusher_started: bool,
}

struct ConsoleLogBuffer {
    state: Mutex<Buf>,
    tx: broadcast::Sender<ConsoleEvent>,
}

impl ConsoleLogBuffer {
    fn new() -> Self {
        // Lagging subscribers drop old events rather than blocking the writer.
        let (tx, _) = broadcast::channel(256);
        Self {
            state: Mutex::new(Buf {
                logs: Vec::new(),
                pending: Vec::new(),
                flusher_started: false,
            }),
            tx,
        }
    }

    /// Appends a line, trimming the buffer and flushing when a full batch has
    /// accumulated.
    fn append(self: &std::sync::Arc<Self>, line: String) {
        let flush_now = {
            let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
            s.logs.push(line.clone());
            if s.logs.len() > MAX_LINES {
                let excess = s.logs.len() - MAX_LINES;
                s.logs.drain(..excess);
            }
            s.pending.push(line);
            s.pending.len() >= MAX_BATCH_LINES
        };
        self.ensure_flusher();
        if flush_now {
            self.flush_pending();
        }
    }

    /// Starts the process-wide ticker that drains `pending`, once.
    ///
    /// One ticker gives the same cadence with less bookkeeping than a timer per
    /// batch.
    ///
    /// A plain OS thread, not `tokio::spawn`: `append_line` is called from the
    /// tracing layer, and the layer is installed before the runtime exists, so
    /// the first log line would panic on a spawn with no reactor.
    // ponytail: one detached ticker for the process, never stopped. If a clean
    // shutdown ever matters, hold the JoinHandle and signal it there.
    fn ensure_flusher(self: &std::sync::Arc<Self>) {
        {
            let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if s.flusher_started {
                return;
            }
            s.flusher_started = true;
        }
        let buf = self.clone();
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(Duration::from_millis(FLUSH_INTERVAL_MS));
                buf.flush_pending();
            }
        });
    }

    /// Takes the pending batch and broadcasts it.
    fn flush_pending(&self) {
        let lines = {
            let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
            std::mem::take(&mut s.pending)
        };
        if !lines.is_empty() {
            let _ = self.tx.send(ConsoleEvent::Lines(lines));
        }
    }
}

static BUFFER: OnceLock<std::sync::Arc<ConsoleLogBuffer>> = OnceLock::new();

fn buffer() -> &'static std::sync::Arc<ConsoleLogBuffer> {
    BUFFER.get_or_init(|| std::sync::Arc::new(ConsoleLogBuffer::new()))
}

fn strip_ansi(text: &str) -> String {
    if text.contains('\u{1b}') {
        ANSI_RE.replace_all(text, "").into_owned()
    } else {
        text.to_string()
    }
}

/// Renders one argument: a string is kept verbatim, anything else is
/// JSON-encoded.
pub fn format_arg(value: &Value) -> String {
    match value {
        Value::String(s) => strip_ansi(s),
        other => strip_ansi(&other.to_string()),
    }
}

/// Appends a line with its ANSI codes stripped. `level` is not part of the line,
/// so it is not prefixed here.
pub fn append_line(level: &str, message: &str) {
    let _ = level;
    buffer().append(strip_ansi(message));
}

/// A snapshot of the buffered lines.
pub fn logs() -> Vec<String> {
    buffer()
        .state
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .logs
        .clone()
}

/// Drops the buffered lines and tells subscribers.
pub fn clear() {
    buffer()
        .state
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .logs
        .clear();
    let _ = buffer().tx.send(ConsoleEvent::Clear);
}

/// Subscribes to the buffer's fan-out.
pub fn subscribe() -> broadcast::Receiver<ConsoleEvent> {
    buffer().tx.subscribe()
}

/// Collects the rendered `message` field of one tracing event.
#[derive(Default)]
struct MessageVisitor {
    message: String,
}

impl tracing::field::Visit for MessageVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.message = format!("{value:?}");
        }
    }
}

/// A `tracing` layer that pushes every event's message into the ring buffer.
///
/// Register it with its own filter (see the binary's `init_tracing`) — under the
/// same filter as the terminal layer, a default `RUST_LOG` that omits
/// `router_sse` would drop every chat line and the page would stay empty, which
/// is the bug this layer exists to fix.
#[derive(Debug, Default, Clone, Copy)]
pub struct ConsoleLogLayer;

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for ConsoleLogLayer {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _context: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut visitor = MessageVisitor::default();
        event.record(&mut visitor);
        if !visitor.message.is_empty() {
            append_line("log", &visitor.message);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_arg_keeps_strings_and_jsonifies_the_rest() {
        assert_eq!(format_arg(&serde_json::json!("plain")), "plain");
        assert_eq!(format_arg(&serde_json::json!({"a": 1})), r#"{"a":1}"#);
        assert_eq!(format_arg(&serde_json::json!(null)), "null");
    }

    #[test]
    fn ansi_codes_are_stripped() {
        assert_eq!(strip_ansi("\u{1b}[31mred\u{1b}[0m"), "red");
        assert_eq!(strip_ansi("no codes"), "no codes");
    }

    #[test]
    fn the_layer_captures_a_rendered_message() {
        use tracing_subscriber::layer::SubscriberExt;
        // A fresh subscriber per test, so the shared process-global buffer is
        // read before and after without another test's writes in between.
        clear();
        let subscriber = tracing_subscriber::registry().with(ConsoleLogLayer);
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(target: "router_sse::chat", "🟢 ▶ POST deepseek/x");
        });
        let captured = logs();
        assert_eq!(captured, vec!["🟢 ▶ POST deepseek/x".to_string()]);
        clear();
    }

    /// The proxy line must reach the page under the binary's own filter.
    ///
    /// `prepare_send` builds the client but never connects, so a dead proxy URL
    /// is enough to drive the resolution and its log line with no network. The
    /// filter here is the binary's `DEFAULT_TARGETS` verbatim — a target the
    /// filter misses would leave the page blank, which is the bug this feature
    /// fixes.
    #[tokio::test]
    async fn a_proxy_decision_lands_in_the_buffer() {
        use router_sse::executors::http::{ProxyOptions, prepare_send};
        use tracing_subscriber::EnvFilter;
        use tracing_subscriber::layer::{Layer, SubscriberExt};

        const DEFAULT_TARGETS: &str =
            "rustrouter=info,router_server=info,router_db=info,router_sse=info";

        clear();
        let subscriber = tracing_subscriber::registry()
            .with(ConsoleLogLayer.with_filter(EnvFilter::new(DEFAULT_TARGETS)));
        // `set_default` holds the subscriber across the await; `with_default`
        // takes a sync closure and would need a nested runtime.
        let _guard = tracing::subscriber::set_default(subscriber);
        let opts = ProxyOptions {
            enabled: true,
            url: Some("http://127.0.0.1:1".into()),
            no_proxy: None,
            strict_proxy: false,
            vercel_relay_url: None,
        };
        let _ = prepare_send("https://api.openai.com/v1/chat/completions", &opts).await;
        drop(_guard);
        let captured = logs();
        assert!(
            captured
                .iter()
                .any(|l| l.contains("[ProxyFetch] proxy -> https://api.openai.com:443 via http://127.0.0.1:1 (connection)")),
            "proxy line missing from buffer: {captured:?}"
        );
        clear();
    }
}
