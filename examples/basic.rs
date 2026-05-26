// TODO(ai-review): review for style and correctness
//! Tiny demo: a nested span graph with some sleeps + a noisy hot loop that
//! creates many micro-spans. With `min = 1ms` the loop's per-iter spans are
//! filtered, but their elapsed still bubbles into the loop parent's children
//! total — so the loop's reported self-time stays close to zero, not inflated.
//!
//! Run with:
//! ```sh
//! cargo run --example basic
//! ```

use std::thread::sleep;
use std::time::Duration;

use tracing::{debug_span, info_span, trace_span, warn_span};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

fn main() {
    tracing_subscriber::registry()
        .with(
            tracing_timetree::layer()
                .with_min(Duration::from_millis(1))
                .with_target(true),
        )
        .init();

    let _root = info_span!("root", note = "demo").entered();

    {
        let _s = info_span!("fetch", url = "https://example.invalid/").entered();
        sleep(Duration::from_millis(20));
    }

    {
        let _s = debug_span!("parse").entered();
        sleep(Duration::from_millis(5));
        {
            let _s = trace_span!("tokenize").entered();
            sleep(Duration::from_millis(8));
        }
        {
            let _s = warn_span!("typecheck").entered();
            sleep(Duration::from_millis(12));
        }
    }

    {
        let _s = info_span!("hot_loop", iters = 500u32).entered();
        for i in 0u32..500 {
            let _s = trace_span!("iter", i).entered();
            // Each iter is well under 1ms — gets filtered out, but its elapsed
            // still feeds the parent's children total so `hot_loop`'s
            // self-time stays honest.
            std::hint::black_box(i.wrapping_mul(31));
        }
        sleep(Duration::from_millis(3));
    }
}
