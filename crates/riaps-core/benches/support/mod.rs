//! Small, dependency-free benchmark runner shared by the workspace benches.
//!
//! A sample times a batch of operations, then divides by the operation count.
//! This amortizes `Instant::now()` overhead instead of pretending that a
//! single timer call can resolve a 5-25 ns primitive.

use std::time::Duration;

const WARMUP_SAMPLES: usize = 8;

#[derive(Debug)]
pub struct Measurement {
    name: &'static str,
    samples_ns_per_op: Vec<f64>,
}

impl Measurement {
    fn mean_ns(&self) -> f64 {
        self.samples_ns_per_op.iter().sum::<f64>() / self.samples_ns_per_op.len() as f64
    }

    fn percentile_ns(&self, percentile: f64) -> f64 {
        let last = self.samples_ns_per_op.len() - 1;
        let index = ((last as f64) * percentile).round() as usize;
        self.samples_ns_per_op[index.min(last)]
    }

    fn min_ns(&self) -> f64 {
        self.samples_ns_per_op[0]
    }

    fn max_ns(&self) -> f64 {
        self.samples_ns_per_op[self.samples_ns_per_op.len() - 1]
    }
}

/// Measure a caller-defined sample.
///
/// `sample` must execute exactly `operations_per_sample` logical operations
/// and return only the duration of the timed region. Setup, refill, draining,
/// and validation belong outside that region.
pub fn benchmark_samples<F>(
    name: &'static str,
    sample_count: usize,
    operations_per_sample: u64,
    mut sample: F,
) -> Measurement
where
    F: FnMut() -> Duration,
{
    assert!(sample_count >= 20, "at least 20 samples are required");
    assert!(operations_per_sample > 0, "a sample must contain operations");

    for _ in 0..WARMUP_SAMPLES {
        std::hint::black_box(sample());
    }

    let mut values = Vec::with_capacity(sample_count);
    for _ in 0..sample_count {
        let elapsed = sample();
        values.push(elapsed.as_secs_f64() * 1e9 / operations_per_sample as f64);
    }
    values.sort_by(f64::total_cmp);

    Measurement {
        name,
        samples_ns_per_op: values,
    }
}

/// Measure an operation in batches with no per-sample setup.
pub fn benchmark<F>(
    name: &'static str,
    sample_count: usize,
    operations_per_sample: u64,
    mut operation: F,
) -> Measurement
where
    F: FnMut(),
{
    benchmark_samples(name, sample_count, operations_per_sample, || {
        let started = std::time::Instant::now();
        for _ in 0..operations_per_sample {
            operation();
        }
        started.elapsed()
    })
}

pub fn print_header(title: &str) {
    println!();
    println!("=== {title} ===");
    println!(
        "{:<38} {:>10} {:>10} {:>10} {:>10} {:>10}  result",
        "benchmark", "mean", "median", "p99", "min", "max"
    );
    println!("{}", "-".repeat(108));
}

/// Report a descriptive reference budget without making the benchmark flaky.
///
/// CI correctness must not depend on host frequency scaling or shared-runner
/// noise. Enforce budgets in a dedicated, pinned performance environment.
pub fn report(measurement: &Measurement, reference_budget_ns: Option<f64>) {
    let mean = measurement.mean_ns();
    let result = match reference_budget_ns {
        Some(budget) if mean <= budget => format!("PASS (reference < {budget:.1} ns)"),
        Some(budget) => format!("MISS (reference < {budget:.1} ns)"),
        None => "INFO (no portable budget)".to_owned(),
    };

    println!(
        "{:<38} {:>8.2}ns {:>8.2}ns {:>8.2}ns {:>8.2}ns {:>8.2}ns  {}",
        measurement.name,
        mean,
        measurement.percentile_ns(0.50),
        measurement.percentile_ns(0.99),
        measurement.min_ns(),
        measurement.max_ns(),
        result
    );
}
