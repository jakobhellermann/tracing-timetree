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
use std::io::{self, IsTerminal, Stderr, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use tracing::{Level, Subscriber};
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

/// How to colorize output.
#[derive(Clone, Copy, Debug, Default)]
pub enum Color {
    /// ANSI colors only when the writer is a TTY. Default.
    #[default]
    Auto,
    /// Always emit ANSI escapes.
    Always,
    /// Never emit ANSI escapes.
    Never,
}

/// Prints one line per closed span: depth-indented name, field values, total
/// elapsed and self-time. Spans shorter than `min` are dropped so micro-spans
/// don't drown out the parents we actually want to attribute time to.
pub struct TimingLayer<W: MakeWriter = fn() -> Stderr> {
    min: Duration,
    color: Color,
    show_level: bool,
    show_target: bool,
    make_writer: W,
    /// Depth of the last emitted line. `usize::MAX` means "nothing emitted
    /// yet" — used to suppress the leading blank line.
    last_depth: AtomicUsize,
}

impl Default for TimingLayer {
    fn default() -> Self {
        Self {
            min: Duration::ZERO,
            color: Color::Auto,
            show_level: false,
            show_target: false,
            make_writer: stderr_writer,
            last_depth: AtomicUsize::new(usize::MAX),
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

    /// Choose when to emit ANSI color escapes. Defaults to [`Color::Auto`].
    pub fn with_color(mut self, color: Color) -> Self {
        self.color = color;
        self
    }

    /// Prefix every line with the span's level (`INFO `, `WARN `, …). Off by
    /// default.
    pub fn with_level(mut self, show: bool) -> Self {
        self.show_level = show;
        self
    }

    /// Append the span's target (typically its module path) at the end of the
    /// line. Off by default.
    pub fn with_target(mut self, show: bool) -> Self {
        self.show_target = show;
        self
    }

    /// Swap the writer. Mirrors `fmt::Layer::with_writer`.
    pub fn with_writer<W2: MakeWriter>(self, make_writer: W2) -> TimingLayer<W2> {
        TimingLayer {
            min: self.min,
            color: self.color,
            show_level: self.show_level,
            show_target: self.show_target,
            make_writer,
            last_depth: self.last_depth,
        }
    }
}

/// Target width of the name column. Long names just push fields further right
/// — we don't truncate.
const NAME_COL: usize = 12;

/// Format a duration into a compact, fixed-width-ish string with a smart unit.
///
/// Always emits the smallest sensible unit so values stay short:
/// `123.4ms`, `12.3s`, `2m05s`. Sub-millisecond durations keep millisecond
/// precision (e.g. `0.1ms`).
fn fmt_duration(d: Duration) -> String {
    let ms = d.as_secs_f64() * 1000.0;
    if ms < 1000.0 {
        format!("{ms:.1}ms")
    } else if ms < 60_000.0 {
        format!("{:.1}s", ms / 1000.0)
    } else {
        let total_secs = d.as_secs();
        let mins = total_secs / 60;
        let secs = total_secs % 60;
        format!("{mins}m{secs:02}s")
    }
}

/// Fallback width when no terminal is attached (piped output, custom writer).
const FALLBACK_WIDTH: usize = 120;

/// Best-effort terminal width. Falls back to [`FALLBACK_WIDTH`] when we can't
/// detect one (piped output, non-TTY writer).
fn term_width() -> usize {
    terminal_size::terminal_size()
        .map(|(terminal_size::Width(w), _)| w as usize)
        .unwrap_or(FALLBACK_WIDTH)
}

/// Visible character count, ignoring ANSI CSI sequences. Good enough for our
/// own output where escapes only appear as `\x1b[…m`.
fn visible_len(s: &str) -> usize {
    let mut len = 0;
    let mut bytes = s.bytes();
    while let Some(b) = bytes.next() {
        if b == 0x1b {
            // Skip until the SGR terminator 'm' (or any other final byte).
            for nb in bytes.by_ref() {
                if nb.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            len += 1;
        }
    }
    len
}

/// ANSI sequences. Empty when color is disabled.
struct Style {
    dim: &'static str,
    bold: &'static str,
    cyan: &'static str,
    reset: &'static str,
}

impl Style {
    const ON: Self = Self {
        dim: "\x1b[2m",
        bold: "\x1b[1m",
        cyan: "\x1b[36m",
        reset: "\x1b[0m",
    };
    const OFF: Self = Self {
        dim: "",
        bold: "",
        cyan: "",
        reset: "",
    };

    fn level_color(&self, level: Level) -> &'static str {
        // Off when colors are disabled; otherwise mirror tracing-subscriber's
        // conventional palette.
        if self.reset.is_empty() {
            return "";
        }
        match level {
            Level::ERROR => "\x1b[31m", // red
            Level::WARN => "\x1b[33m",  // yellow
            Level::INFO => "\x1b[32m",  // green
            Level::DEBUG => "\x1b[34m", // blue
            Level::TRACE => "\x1b[2m",  // dim
        }
    }
}

fn resolve_style<W: Write>(color: Color, writer: &W) -> &'static Style {
    let on = match color {
        Color::Always => true,
        Color::Never => false,
        // `IsTerminal` is on `&W` for stdio handles; for arbitrary writers we
        // can't tell, so Auto falls back to off.
        Color::Auto => is_terminal_writer(writer),
    };
    if on { &Style::ON } else { &Style::OFF }
}

/// Best-effort TTY check. Specialized for the common stdio writers; everything
/// else is treated as non-TTY.
fn is_terminal_writer<W: ?Sized>(_w: &W) -> bool {
    // We can't downcast through a generic `Write`, so probe stderr — the
    // default writer — and assume custom writers are non-interactive. Users
    // who want color on a custom writer can call `with_color(Color::Always)`.
    io::stderr().is_terminal()
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
        let metadata = span.metadata();
        let name = metadata.name();
        let indent = "  ".repeat(depth);
        let elapsed_str = fmt_duration(elapsed);
        let self_str = fmt_duration(self_time);
        let mut w = self.make_writer.make_writer();
        let s = resolve_style(self.color, &w);

        let mut line = String::new();
        // Spans close child-first, so the depth sequence walks bottom-up:
        // child (deep), then parent (shallow). When the next line's depth
        // jumps *back down* (deeper than the previous), we're starting a new
        // subtree — insert a blank line so the new group reads as belonging
        // to the parent that follows, not the one that just closed.
        // `Relaxed` is fine: cross-thread ordering doesn't need to be exact.
        let prev_depth = self.last_depth.swap(depth, Ordering::Relaxed);
        if prev_depth != usize::MAX && depth > prev_depth {
            line.push('\n');
        }
        if self.show_level {
            let lvl = *metadata.level();
            let _ = write!(
                line,
                "{color}{lvl:<5}{reset} ",
                color = s.level_color(lvl),
                reset = s.reset,
            );
        }
        let _ = write!(
            line,
            "{dim}{elapsed_str:>7}{reset}  {dim}{self_str:>7}{reset}  {indent}",
            dim = s.dim,
            reset = s.reset,
        );
        // Pad the name column so siblings line up. Long names just push what
        // follows further right.
        let name_width = NAME_COL.saturating_sub(indent.len());
        let name_pad = name_width.saturating_sub(name.len());
        let name_color = if fields.is_empty() { "" } else { s.cyan };
        let _ = write!(
            line,
            "{bold}{name_color}{name}{reset}{pad:width$}",
            bold = s.bold,
            reset = s.reset,
            pad = "",
            width = name_pad,
        );
        if !fields.is_empty() {
            let _ = write!(line, "  {dim}{fields}{reset}", dim = s.dim, reset = s.reset);
        }
        if self.show_target {
            let target = metadata.target();
            // Right-align the target near the terminal edge, leaving a small
            // gutter on the right so it doesn't kiss the border. If the line
            // is already too long, just leave a single space separator —
            // better wrapped than dropped.
            const RIGHT_GUTTER: usize = 2;
            let width = term_width();
            let used = visible_len(&line);
            let needed = target.len() + 2 + RIGHT_GUTTER;
            let gap = width
                .saturating_sub(used + target.len() + RIGHT_GUTTER)
                .max(2);
            let _ = if used + needed <= width {
                write!(line, "{pad:gap$}{dim}{target}{reset}", pad = "", dim = s.dim, reset = s.reset)
            } else {
                write!(line, "  {dim}{target}{reset}", dim = s.dim, reset = s.reset)
            };
        }
        line.push('\n');
        let _ = w.write_all(line.as_bytes());
    }
}

