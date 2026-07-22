//! Fixed-capacity host-to-DSP block adaptation.
//!
//! All storage is allocated during construction. The realtime path never
//! grows a collection and never allocates. For a fixed host quantum `H` and
//! DSP quantum `D`, the adapter primes `D - gcd(H, D)` silence samples when
//! the quanta are not aligned. This is the minimum reserve that prevents
//! periodic output underruns while preserving zero added latency for aligned
//! block sizes.

/// Per-callback diagnostics. The audio thread can copy this value into an
/// event queue without allocation.
#[must_use = "inspect drops, underruns, and latency changes"]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AdapterProcessReport {
    pub accepted_input_samples: usize,
    pub dropped_input_samples: usize,
    pub processed_dsp_blocks: usize,
    pub dropped_output_samples: usize,
    pub underrun_samples: usize,
    pub latency_added_samples: usize,
}

/// Host block adapter with fixed-capacity input and output FIFOs.
pub struct AdapterBuffer {
    input_fifo: SampleFifo,
    output_fifo: SampleFifo,
    scratch_in: Box<[f32]>,
    scratch_out: Box<[f32]>,
    target_block_size: usize,
    host_quantum: Option<usize>,
    latency_samples: usize,
}

impl AdapterBuffer {
    /// Create an adapter and allocate all storage.
    ///
    /// `capacity` must hold at least two DSP blocks: one processing block and
    /// one block of phase reserve for arbitrary host/DSP block relationships.
    pub fn new(target_block_size: usize, capacity: usize) -> Self {
        assert!(target_block_size > 0, "target block size must be non-zero");
        assert!(
            capacity >= target_block_size.saturating_mul(2),
            "capacity must hold at least two DSP blocks"
        );

        Self {
            input_fifo: SampleFifo::new(capacity),
            output_fifo: SampleFifo::new(capacity),
            scratch_in: vec![0.0; target_block_size].into_boxed_slice(),
            scratch_out: vec![0.0; target_block_size].into_boxed_slice(),
            target_block_size,
            host_quantum: None,
            latency_samples: 0,
        }
    }

    pub fn target_block_size(&self) -> usize {
        self.target_block_size
    }

    /// Current startup/reconfiguration latency in interleaved samples.
    pub fn latency_samples(&self) -> usize {
        self.latency_samples
    }

    pub fn accumulated(&self) -> usize {
        self.input_fifo.len()
    }

    pub fn pending_output(&self) -> usize {
        self.output_fifo.len()
    }

    /// Process one host callback without allocation or blocking.
    #[inline]
    pub fn process_host_block<F>(
        &mut self,
        host_inputs: &[f32],
        host_outputs: &mut [f32],
        mut dsp_callback: F,
    ) -> AdapterProcessReport
    where
        F: FnMut(&[f32], &mut [f32]),
    {
        let mut report = AdapterProcessReport::default();
        report.latency_added_samples = self.prime_for_quantum(host_outputs.len());

        report.accepted_input_samples = self.input_fifo.push_slice(host_inputs);
        report.dropped_input_samples = host_inputs.len() - report.accepted_input_samples;

        let (blocks, dropped) = self.drain_complete_blocks(&mut dsp_callback);
        report.processed_dsp_blocks = blocks;
        report.dropped_output_samples = dropped;

        for output in host_outputs {
            match self.output_fifo.pop() {
                Some(value) => *output = value,
                None => {
                    *output = 0.0;
                    report.underrun_samples += 1;
                }
            }
        }

        report
    }

    /// Graceful-shutdown drain. This is a non-realtime API and may allocate
    /// the returned `Vec`.
    pub fn flush<F>(&mut self, mut dsp_callback: F) -> Vec<f32>
    where
        F: FnMut(&[f32], &mut [f32]),
    {
        let remainder = self.input_fifo.len();
        if remainder > 0 {
            debug_assert!(remainder < self.target_block_size);
            for slot in &mut self.scratch_in[..remainder] {
                *slot = self.input_fifo.pop().expect("remainder was measured");
            }
            self.scratch_in[remainder..].fill(0.0);
            dsp_callback(&self.scratch_in, &mut self.scratch_out);

            // Only the output corresponding to real input belongs to the
            // stream. Node-specific tail rendering is a separate DSP concern.
            self.output_fifo.push_slice(&self.scratch_out[..remainder]);
        }

        let mut output = Vec::with_capacity(self.output_fifo.len());
        while let Some(sample) = self.output_fifo.pop() {
            output.push(sample);
        }
        output
    }

    #[inline]
    fn drain_complete_blocks<F>(&mut self, dsp_callback: &mut F) -> (usize, usize)
    where
        F: FnMut(&[f32], &mut [f32]),
    {
        let mut blocks = 0;
        let mut dropped_output = 0;
        while self.input_fifo.len() >= self.target_block_size {
            for slot in &mut self.scratch_in {
                *slot = self.input_fifo.pop().expect("a complete block is available");
            }
            dsp_callback(&self.scratch_in, &mut self.scratch_out);
            let written = self.output_fifo.push_slice(&self.scratch_out);
            dropped_output += self.target_block_size - written;
            blocks += 1;
        }
        (blocks, dropped_output)
    }

    /// Establish enough output reserve for the negotiated host quantum.
    ///
    /// A runtime quantum change cannot preserve both the old phase and minimum
    /// latency. In that exceptional path we conservatively raise the reserve
    /// to one DSP block minus one sample and report the added silence.
    fn prime_for_quantum(&mut self, host_quantum: usize) -> usize {
        if host_quantum == 0 {
            return 0;
        }

        let required_reserve = match self.host_quantum {
            None => {
                let gcd = greatest_common_divisor(host_quantum, self.target_block_size);
                self.target_block_size - gcd
            }
            Some(previous) if previous == host_quantum => return 0,
            Some(_) => self.target_block_size - 1,
        };

        self.host_quantum = Some(host_quantum);
        self.latency_samples = self.latency_samples.max(required_reserve);
        let needed = required_reserve.saturating_sub(self.output_fifo.len());
        let inserted = self.output_fifo.push_zeros(needed);
        debug_assert_eq!(inserted, needed, "constructor capacity guarantees priming");
        inserted
    }
}

fn greatest_common_divisor(mut left: usize, mut right: usize) -> usize {
    while right != 0 {
        let remainder = left % right;
        left = right;
        right = remainder;
    }
    left
}

/// A single-threaded, fixed-capacity ring for interleaved samples.
struct SampleFifo {
    storage: Box<[f32]>,
    read: usize,
    write: usize,
    len: usize,
}

impl SampleFifo {
    fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "FIFO capacity must be non-zero");
        Self {
            storage: vec![0.0; capacity].into_boxed_slice(),
            read: 0,
            write: 0,
            len: 0,
        }
    }

    #[inline(always)]
    fn len(&self) -> usize {
        self.len
    }

    #[inline]
    fn push_slice(&mut self, samples: &[f32]) -> usize {
        let accepted = samples.len().min(self.storage.len() - self.len);
        for &sample in &samples[..accepted] {
            self.storage[self.write] = sample;
            self.write += 1;
            if self.write == self.storage.len() {
                self.write = 0;
            }
        }
        self.len += accepted;
        accepted
    }

    #[inline]
    fn push_zeros(&mut self, count: usize) -> usize {
        let accepted = count.min(self.storage.len() - self.len);
        for _ in 0..accepted {
            self.storage[self.write] = 0.0;
            self.write += 1;
            if self.write == self.storage.len() {
                self.write = 0;
            }
        }
        self.len += accepted;
        accepted
    }

    #[inline(always)]
    fn pop(&mut self) -> Option<f32> {
        if self.len == 0 {
            return None;
        }
        let sample = self.storage[self.read];
        self.read += 1;
        if self.read == self.storage.len() {
            self.read = 0;
        }
        self.len -= 1;
        Some(sample)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mismatched_blocks_preserve_the_complete_stream() {
        const HOST_BLOCK: usize = 384;
        const DSP_BLOCK: usize = 512;
        const TOTAL: usize = DSP_BLOCK * 6;
        let mut adapter = AdapterBuffer::new(DSP_BLOCK, 4_096);
        let input: Vec<f32> = (1..=TOTAL).map(|value| value as f32).collect();
        let mut output = Vec::new();

        for chunk in input.chunks(HOST_BLOCK) {
            let mut host_output = vec![f32::NAN; chunk.len()];
            let report = adapter.process_host_block(chunk, &mut host_output, passthrough);
            assert_eq!(report.dropped_input_samples, 0);
            assert_eq!(report.dropped_output_samples, 0);
            assert_eq!(report.underrun_samples, 0);
            output.extend_from_slice(&host_output);
        }
        output.extend(adapter.flush(passthrough));

        let latency = DSP_BLOCK - greatest_common_divisor(HOST_BLOCK, DSP_BLOCK);
        assert_eq!(adapter.latency_samples(), latency);
        assert_eq!(&output[..latency], vec![0.0; latency]);
        assert_eq!(&output[latency..], input.as_slice());
        assert_eq!(adapter.accumulated(), 0);
        assert_eq!(adapter.pending_output(), 0);
    }

    #[test]
    fn aligned_blocks_add_no_latency() {
        let mut adapter = AdapterBuffer::new(256, 1_024);
        let input: Vec<f32> = (1..=256).map(|value| value as f32).collect();
        let mut output = vec![0.0; input.len()];
        let report = adapter.process_host_block(&input, &mut output, passthrough);

        assert_eq!(output, input);
        assert_eq!(adapter.latency_samples(), 0);
        assert_eq!(report.underrun_samples, 0);
    }

    #[test]
    fn non_multiple_large_host_blocks_do_not_underrun() {
        const HOST_BLOCK: usize = 600;
        const DSP_BLOCK: usize = 512;
        let mut adapter = AdapterBuffer::new(DSP_BLOCK, 4_096);
        let input = vec![1.0; HOST_BLOCK];
        let mut output = vec![0.0; HOST_BLOCK];

        for _ in 0..32 {
            let report = adapter.process_host_block(&input, &mut output, passthrough);
            assert_eq!(report.underrun_samples, 0);
            assert_eq!(report.dropped_input_samples, 0);
            assert_eq!(report.dropped_output_samples, 0);
        }
        assert_eq!(adapter.latency_samples(), DSP_BLOCK - 8);
    }

    #[test]
    fn quantum_change_is_reported_and_remains_safe() {
        let mut adapter = AdapterBuffer::new(512, 4_096);
        let mut aligned_output = vec![0.0; 512];
        let aligned_input = vec![1.0; 512];
        let first = adapter.process_host_block(&aligned_input, &mut aligned_output, passthrough);
        assert_eq!(first.latency_added_samples, 0);

        let mut changed_output = vec![0.0; 384];
        let changed_input = vec![1.0; 384];
        let changed = adapter.process_host_block(&changed_input, &mut changed_output, passthrough);
        assert!(changed.latency_added_samples > 0);
        assert_eq!(changed.underrun_samples, 0);
    }

    #[test]
    fn overflow_is_reported_without_allocation_or_panic() {
        let mut adapter = AdapterBuffer::new(64, 256);
        let input = vec![1.0; 1_000];
        let mut output = vec![0.0; 1_000];
        let report = adapter.process_host_block(&input, &mut output, passthrough);

        assert_eq!(report.accepted_input_samples, 256);
        assert_eq!(report.dropped_input_samples, 744);
    }

    #[test]
    fn flush_zero_pads_only_the_internal_processing_block() {
        let mut adapter = AdapterBuffer::new(256, 1_024);
        let input: Vec<f32> = (1..=100).map(|value| value as f32).collect();
        let mut no_output = [];
        let report = adapter.process_host_block(&input, &mut no_output, passthrough);
        assert_eq!(report.accepted_input_samples, input.len());

        let tail = adapter.flush(passthrough);
        assert_eq!(tail, input);
        assert_eq!(adapter.accumulated(), 0);
    }

    fn passthrough(input: &[f32], output: &mut [f32]) {
        output.copy_from_slice(input);
    }
}