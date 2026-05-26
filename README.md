# tracing-timetree

A streaming `tracing` layer that prints one indented line per closed span,
with elapsed and self-time.

## Usage

```rust
use std::time::Duration;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

tracing_subscriber::registry()
    .with(tracing_timetree::layer().with_min_ms(1))
    .init();
```

## Example output

```
INFO   20.1ms   20.1ms    fetch       url=https://example.invalid/

TRACE   8.1ms    8.1ms      tokenize
WARN   12.1ms   12.1ms      typecheck
DEBUG  25.2ms    5.1ms    parse
INFO    4.6ms    4.0ms    hot_loop    (500 skipped)  iters=500
INFO   49.9ms    0.1ms  root          note=demo
```

Columns: level, total elapsed, self-time, indented span name, fields. Spans
shorter than `min` are filtered; their elapsed still bubbles into the
parent's children-total, so self-time stays honest and a `(N skipped)`
summary surfaces the hidden count.
