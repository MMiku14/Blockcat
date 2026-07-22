# RIAPS realtime audio microkernel

Rust workspace implementing the physical crate boundaries and core invariants
from the RIAPS 4.8.1 architecture document. MSRV is Rust 1.82.

## Workspace

```text
riaps-core   no_std + alloc   SPSC, RCU primitive, deferred destruction
    ^
riaps-sys                     FPU guard, monotonic probe, thread provisioning
    ^
    +-- riaps-host            fixed-capacity block adapter, host backends
    +-- riaps-dsp             DAG runtime, graph publication, event bus
```

The dependency graph is enforced by the four manifests: `core` has no
dependencies, `sys` depends only on `core`, and `host`/`dsp` never depend on
each other.

## Verify

```bash
bash scripts/verify.sh

# Equivalent individual commands:
cargo fmt --all --check
cargo check --workspace --all-targets
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo bench --workspace --no-run
```

Workspace lints use `warnings = "deny"`; every crate explicitly inherits the
policy. This includes libraries, tests, and benchmark targets.

## Benchmarks

```bash
cargo bench -p riaps-core
cargo bench -p riaps-sys
cargo bench -p riaps-host
cargo bench -p riaps-dsp
```

The benchmark runner has no third-party dependency. It measures batches and
divides by the number of logical operations, so timer-call overhead does not
dominate nanosecond primitives. Queue setup, refill, draining, graph building,
allocation, and correctness assertions are outside the timed regions.

Results include mean, median, p99, minimum, and maximum. Reference budgets are
reported but do not fail ordinary CI: shared runners, frequency scaling, and
CPU migration make hard performance gates noisy. Enforce budgets only on a
pinned, fixed-frequency performance host.

| Crate | Bench coverage |
|---|---|
| `riaps-core` | successful SPSC push/pop, batch pop, EmergencySlot, RCU |
| `riaps-sys` | FPU guard, platform tick read, enabled/disabled probe |
| `riaps-host` | aligned, mismatched, small/large host blocks, shutdown flush |
| `riaps-dsp` | event emit, graph publication, 4-node graph, parameter queue |

## Realtime guarantees represented in code

- `AdapterBuffer` uses fixed-capacity `Box<[f32]>` rings. Its callback path
  cannot grow a `VecDeque` or allocate.
- Non-integer host/DSP block ratios receive the minimum fixed-quantum reserve
  `D - gcd(H, D)`, preventing periodic underruns. Runtime quantum changes use
  a conservative reserve and report added latency.
- `AdapterProcessReport` exposes input drops, output drops, underruns, processed
  blocks, and newly added latency without allocation.
- `DspGraphInner` executes the compiled DAG rather than silently serializing
  every node. Predecessor and sink mixing use buffers allocated at compile time.
- `DspGraphHandle::try_pin` publishes a hazard pointer for one audio callback.
  `RetiredDspGraph::try_reclaim` refuses reclamation while that graph is pinned.
- EventBus reserves its final eight slots for fatal events and never lets the
  producer cross into the consumer side.

## Intentional boundaries

- ALSA and CoreAudio modules are compile-time backend skeletons; they do not
  link system audio libraries yet.
- Linux rtkit/portal and Windows MMCSS integration remain platform integration
  work. The current provisioner implements direct Linux scheduling, macOS QoS,
  strict refusal, and explicit fallback state.
- `AdapterBuffer::flush` zero-pads an internal final block but returns only the
  output corresponding to real input. Rendering an IIR/reverb tail is a DSP
  node policy and is not inferred by the host adapter.
- Cross-platform 32-bit atomics still require the documented
  `portable-atomic` integration before enabling those targets.

## Copy to `~/src`

```bash
mkdir -p ~/src
cp -r riaps-workspace ~/src/
cd ~/src/riaps-workspace
cargo test --workspace
```