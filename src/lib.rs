// TODO(ai-review): review for style and correctness
//! Streaming `tracing` layer that prints one indented line per closed span:
//! `elapsed=…ms self=…ms  <indent><name> {fields=…}`.
//!
//! - **Self-time** is computed by subtracting the sum of direct children's
//!   elapsed time from this span's own elapsed.
//! - A configurable **minimum elapsed** filter drops short spans. Filtered
//!   children still bubble their time into the parent's children-total, so a
//!   parent's self-time isn't inflated by the noise we hid.
//! - Output is streamed on `on_close` (no end-of-run dump), so you can watch
//!   long runs live.
//!
//! ```no_run
//! use std::time::Duration;
//! use tracing_subscriber::layer::SubscriberExt;
//! use tracing_subscriber::util::SubscriberInitExt;
//!
//! tracing_subscriber::registry()
//!     .with(tracing_timetree::layer().with_min(Duration::from_micros(500)))
//!     .init();
//! ```

use std::fmt::Write as _;
use std::io::{self, Stderr, Write};
use std::time::{Duration, Instant};

use tracing::Subscriber;
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id};
use tracing_subscriber::layer::{Context, Layer};
use tracing_subscriber::registry::LookupSpan;

/// Trait for "give me a fresh writer each time I want to emit a line".
/// Mirrors `tracing_subscriber::fmt::MakeWriter` but kept local so this crate
/// doesn't depend on the `fmt` feature.
pub trait MakeWriter: 'static {
    type Writer: Write;
    fn make_writer(&self) -> Self::Writer;
}

impl<F, W> MakeWriter for F
where
    F: Fn() -> W + 'static,
    W: Write,
{
    type Writer = W;
    fn make_writer(&self) -> W {
        (self)()
    }
}

/// Default writer: `io::stderr()`.
pub fn stderr_writer() -> Stderr {
    io::stderr()
}

/// Free constructor mirroring `tracing_subscriber::fmt::layer()`.
pub fn layer() -> TimingLayer {
    TimingLayer::default()
}

/// Prints one line per closed span: depth-indented name, field values, total
/// elapsed and self-time. Spans shorter than `min` are dropped so micro-spans
/// don't drown out the parents we actually want to attribute time to.
pub struct TimingLayer<W: MakeWriter = fn() -> Stderr> {
    min: Duration,
    make_writer: W,
}

impl Default for TimingLayer {
    fn default() -> Self {
        Self {
            min: Duration::ZERO,
            make_writer: stderr_writer,
        }
    }
}

impl TimingLayer {
    pub fn new() -> Self {
        Self::default()
    }
}

impl<W: MakeWriter> TimingLayer<W> {
    /// Drop spans whose total elapsed is below `min`. Filtered children still
    /// count toward their parent's children-total, so the parent's self-time
    /// isn't inflated by the hidden noise.
    pub fn with_min(mut self, min: Duration) -> Self {
        self.min = min;
        self
    }

    /// Swap the writer. Mirrors `fmt::Layer::with_writer`.
    pub fn with_writer<W2: MakeWriter>(self, make_writer: W2) -> TimingLayer<W2> {
        TimingLayer {
            min: self.min,
            make_writer,
        }
    }
}

struct OpenedAt(Instant);
struct Fields(String);
/// Sum of elapsed time of direct children that have closed so far.
/// Subtract from this span's elapsed to get its self-time.
#[derive(Default)]
struct ChildrenElapsed(Duration);

struct FieldVisitor<'a>(&'a mut String);
impl Visit for FieldVisitor<'_> {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if !self.0.is_empty() {
            self.0.push(' ');
        }
        let _ = write!(self.0, "{}={:?}", field.name(), value);
    }
    fn record_str(&mut self, field: &Field, value: &str) {
        if !self.0.is_empty() {
            self.0.push(' ');
        }
        let _ = write!(self.0, "{}={}", field.name(), value);
    }
}

impl<S, W> Layer<S> for TimingLayer<W>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    W: MakeWriter,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        let span = ctx.span(id).expect("span just created");
        let mut fields = String::new();
        attrs.record(&mut FieldVisitor(&mut fields));
        let mut ext = span.extensions_mut();
        ext.insert(OpenedAt(Instant::now()));
        ext.insert(Fields(fields));
        ext.insert(ChildrenElapsed::default());
    }

    fn on_close(&self, id: Id, ctx: Context<'_, S>) {
        let span = ctx.span(&id).expect("span on close");
        let elapsed = span
            .extensions()
            .get::<OpenedAt>()
            .map(|o| o.0.elapsed())
            .unwrap_or_default();
        // Add our elapsed to the parent's children total so its self-time can
        // subtract us. Done before the early-return so filtered children still
        // count toward the parent — otherwise the parent's self-time would
        // inflate by all the noise we hid.
        if let Some(parent) = span.parent()
            && let Some(c) = parent.extensions_mut().get_mut::<ChildrenElapsed>()
        {
            c.0 += elapsed;
        }
        if elapsed < self.min {
            return;
        }
        let children_elapsed = span
            .extensions()
            .get::<ChildrenElapsed>()
            .map(|c| c.0)
            .unwrap_or_default();
        let self_time = elapsed.saturating_sub(children_elapsed);
        let depth = span.scope().skip(1).count();
        let fields = span
            .extensions()
            .get::<Fields>()
            .map(|f| f.0.clone())
            .unwrap_or_default();
        let name = span.metadata().name();
        let indent = "  ".repeat(depth);
        let elapsed_ms = elapsed.as_secs_f64() * 1000.0;
        let self_ms = self_time.as_secs_f64() * 1000.0;
        let mut w = self.make_writer.make_writer();
        let _ = if fields.is_empty() {
            writeln!(
                w,
                "elapsed={elapsed_ms:>6.1}ms self={self_ms:>6.1}ms  {indent}{name}"
            )
        } else {
            writeln!(
                w,
                "elapsed={elapsed_ms:>6.1}ms self={self_ms:>6.1}ms  {indent}{name} {{{fields}}}"
            )
        };
    }
}

