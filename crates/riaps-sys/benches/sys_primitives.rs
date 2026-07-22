//! Benchmarks for platform primitives in `riaps-sys`.
//!
//! Run with: `cargo bench -p riaps-sys`

#[path = "../../riaps-core/benches/support/mod.rs"]
mod support;

use std::time::Instant;

use riaps_sys::{read_monotonic_tick, FpuGuard, PerformanceProbe};
use support::{benchmark, benchmark_samples, print_header, report};

const SAMPLES: usize = 120;

fn main() {
    print_header("riaps-sys platform primitives");

    let result = benchmark("fpu_guard_new_drop", SAMPLES, 4_096, || {
        std::hint::black_box(FpuGuard::new());
    });
    report(&result, Some(15.0));

    let result = benchmark("read_monotonic_tick", SAMPLES, 8_192, || {
        std::hint::black_box(read_monotonic_tick());
    });
    report(&result, None);

    let disabled_probe = PerformanceProbe::new();
    let result = benchmark("probe_disabled", SAMPLES, 8_192, || {
        std::hint::black_box(disabled_probe.measure(7, || 42_u64));
    });
    report(&result, Some(10.0));

    bench_enabled_probe();
}

fn bench_enabled_probe() {
    const OPS: u64 = 512;
    let probe = PerformanceProbe::new();
    probe.set_enabled(true);

    let result = benchmark_samples("probe_enabled", SAMPLES, OPS, || {
        let started = Instant::now();
        for _ in 0..OPS {
            std::hint::black_box(probe.measure(7, || 42_u64));
        }
        let elapsed = started.elapsed();

        let mut drained = 0_u64;
        probe.drain(|record| {
            std::hint::black_box(record);
            drained += 1;
        });
        assert_eq!(drained, OPS);
        elapsed
    });
    report(&result, None);
}