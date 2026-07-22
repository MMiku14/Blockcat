//! Benchmarks for DSP graph and event primitives.
//!
//! Run with: `cargo bench -p riaps-dsp`

#[path = "../../riaps-core/benches/support/mod.rs"]
mod support;

use std::time::Instant;

use riaps_dsp::{
    DspGraphHandle, DspGraphInner, DynamicGraphBuilder, EventBus, GainNode, KernelEvent,
    ParameterQueue, ProcessContext, StopReason,
};
use support::{benchmark, benchmark_samples, print_header, report};

const SAMPLES: usize = 100;

fn main() {
    print_header("riaps-dsp graph and events");
    bench_event_bus_non_fatal();
    bench_event_bus_fatal();
    bench_graph_rcu_swap();
    bench_graph_process();
    bench_parameter_queue();
}

fn bench_event_bus_non_fatal() {
    const OPS: u64 = 512;
    let bus = EventBus::<1_024>::new();
    let event = KernelEvent::PerformanceProbeOverload {
        node_id: 3,
        elapsed_us: 1_500,
        budget_us: 1_000,
    };

    let result = benchmark_samples("event_bus_emit_non_fatal", SAMPLES, OPS, || {
        let started = Instant::now();
        for _ in 0..OPS {
            bus.emit(event);
        }
        let elapsed = started.elapsed();

        let mut drained = 0_u64;
        bus.drain(|_| drained += 1);
        assert_eq!(drained, OPS);
        elapsed
    });
    report(&result, Some(30.0));
}

fn bench_event_bus_fatal() {
    const OPS: u64 = 512;
    let bus = EventBus::<1_024>::new();
    let event = KernelEvent::AudioThreadStopped {
        reason: StopReason::HostError,
    };

    let result = benchmark_samples("event_bus_emit_fatal", SAMPLES, OPS, || {
        let started = Instant::now();
        for _ in 0..OPS {
            bus.emit(event);
        }
        let elapsed = started.elapsed();

        let mut drained = 0_u64;
        bus.drain(|_| drained += 1);
        assert_eq!(drained, OPS);
        elapsed
    });
    report(&result, Some(30.0));
}

fn bench_graph_rcu_swap() {
    let handle = DspGraphHandle::new(build_graph(4));
    let mut standby = Some(build_graph(4));

    let result = benchmark("dsp_graph_publish_and_recover", SAMPLES, 4_096, || {
        let next = standby.take().expect("standby graph is available");
        let retired = handle.swap(next);
        standby = Some(
            retired
                .try_into_box()
                .unwrap_or_else(|_| panic!("the benchmark has no graph reader")),
        );
        std::hint::black_box(&standby);
    });
    // Includes Box provenance transfer and construction of a hazard-domain
    // retirement token; the raw atomic swap alone is covered by riaps-core.
    report(&result, None);

    drop(handle);
    drop(standby);
}

fn bench_graph_process() {
    let mut graph = build_graph(4);
    let inner = &mut *graph;
    let context = ProcessContext {
        sample_rate: 96_000,
        block_size: 512,
    };
    let input = vec![1.0_f32; 512];
    let mut output = vec![0.0_f32; 512];

    let result = benchmark("dsp_graph_process_4x512", SAMPLES, 128, || {
        inner.process(&input, &mut output, &context);
        std::hint::black_box(&output);
    });
    report(&result, Some(2_000_000.0));
}

fn bench_parameter_queue() {
    let queue = ParameterQueue::<f32, 16>::new();
    let mut current = 0.0_f32;
    let result = benchmark("parameter_queue_push_and_drain", SAMPLES, 4_096, || {
        assert!(queue.try_push(0.75));
        assert!(queue.drain_to(&mut current));
        std::hint::black_box(current);
    });
    report(&result, None);
}

fn build_graph(node_count: usize) -> Box<DspGraphInner> {
    let mut builder = DynamicGraphBuilder::new(512);
    let mut previous = None;
    for _ in 0..node_count {
        let current = builder.add_node(Box::new(GainNode::new(-3.0)));
        if let Some(previous) = previous {
            builder
                .connect(previous, current)
                .expect("the generated chain is acyclic");
        }
        previous = Some(current);
    }
    builder.compile().expect("the generated chain is valid")
}