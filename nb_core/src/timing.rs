//! Phase timings for profiling: with `NEXTBREW_TIMINGS` set, each [`mark`]
//! prints the milliseconds since [`start`] (the top of `main`) to stderr.

use std::sync::LazyLock;
use std::time::Instant;

static START: LazyLock<Option<Instant>> =
    LazyLock::new(|| std::env::var_os("NEXTBREW_TIMINGS").map(|_| Instant::now()));

/// Start the clock; call first thing in `main`.
pub fn start() {
    LazyLock::force(&START);
}

/// Print `label` with the time since [`start`], when timings are on.
pub fn mark(label: impl std::fmt::Display) {
    if let Some(start) = *START {
        eprintln!(
            "nb-timing {:>9.2}ms {label}",
            start.elapsed().as_secs_f64() * 1000.0
        );
    }
}
