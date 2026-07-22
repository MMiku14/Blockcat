//! Benchmarks for host block adaptation.
//!
//! Run with: `cargo bench -p riaps-host`

#[path = "../../riaps-core/benches/support/mod.rs"]
mod support;

use std::time::Instant;

use riaps_host::AdapterBuffer;
use support::{benchmark, benchmark_samples, print_header, report};

const SAMPLES: usize = 80;

fn main() {
    print_header("riaps-host adapter buffer");
    bench_stream("adapter_aligned_512_to_512", 512, 512);
    bench_stream("adapter_mismatch_384_to_512", 384, 512);
    bench_stream("adapter_small_128_to_512", 128, 512);
    bench_stream("adapter_large_1024_to_512", 1_024, 512);
    bench_flush();
}

fn bench_stream(name: &'static str, host_block: usize, dsp_block: usize) {
    let capacity = (host_block + dsp_block).next_power_of_two() * 2;
    let mut adapter = AdapterBuffer::new(dsp_block, capacity);
    let input = vec![0.5_f32; host_block];
    let mut output = vec![0.0_f32; host_block];

    let result = benchmark(name, SAMPLES, 256, || {
        let _report = adapter.process_host_block(&input, &mut output, |source, destination| {
            destination.copy_from_slice(source);
        });
        std::hint::black_box(&output);
    });

    // Full block movement is bandwidth- and block-size-dependent. Reporting a
    // universal 50 ns budget here would be misleading, so this is descriptive.
    report(&result, None);
}

fn bench_flush() {
    const DSP_BLOCK: usize = 512;
    const REMAINDER: usize = 257;
    let mut adapter = AdapterBuffer::new(DSP_BLOCK, 2_048);
    let input = vec![0.25_f32; REMAINDER];
    let mut no_output = [];

    let result = benchmark_samples("adapter_flush_257_samples", SAMPLES, 1, || {
        let report = adapter.process_host_block(&input, &mut no_output, |source, destination| {
            destination.copy_from_slice(source);
        });
        assert_eq!(report.dropped_input_samples, 0);
        assert_eq!(adapter.accumulated(), REMAINDER);

        let started = Instant::now();
        let tail = adapter.flush(|source, destination| {
            destination.copy_from_slice(source);
        });
        let elapsed = started.elapsed();

        assert_eq!(tail.len(), REMAINDER);
        std::hint::black_box(tail);
        elapsed
    });
    report(&result, None);
}