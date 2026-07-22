//! Adapter Buffer：宿主动态块大小 → DSP 固定块大小的适配层
//! （架构文档 §3.2.3）。
//!
//! 关键设计：
//! - `process_host_block` 泛型单态化：编译期将 DSP process 内联进
//!   Host Adapter，消除 trait object 间接跳转并允许 SIMD 自动向量化
//! - 预分配 scratch buffer：避免音频回调中声明栈上大数组，
//!   消除 LLVM 潜在的 memset(0) 开销（100-300ns）
//! - 防御性截断宿主输入；输出不足时填充零（不变式 #18）
//! - **批量 `copy_from_slice`** 替代逐采样 `pop_front`，编译为
//!   `memcpy` → SIMD 流水线，缓存友好度显著提升
//!
//! 参考实现说明：FIFO 使用 `VecDeque`（初始化阶段一次性
//! `with_capacity` 预分配，运行期不扩容）；生产实现应替换为
//! 固定容量环形缓冲以获得硬保证。

use std::collections::VecDeque;

/// 宿主适配缓冲区。所有分配发生在 `new`（初始化阶段，允许分配，
/// 不变式 #1）；`process_host_block` 热路径零分配。
pub struct AdapterBuffer {
    input_fifo: VecDeque<f32>,
    output_fifo: VecDeque<f32>,
    /// DSP 输入连续块（预分配 scratch）
    scratch_in: Box<[f32]>,
    /// DSP 输出连续块（预分配 scratch，§3.2.3）
    scratch_out: Box<[f32]>,
    target_block_size: usize,
    capacity: usize,
    /// 累计因容量不足而丢弃的输入采样数
    dropped_input_samples: u64,
    /// 累计输出零填充采样数（延迟指标）
    zero_filled_samples: u64,
}

impl AdapterBuffer {
    /// `capacity` 必须 ≥ 宿主最大块 + DSP 目标块，防止突发写入溢出。
    pub fn new(target_block_size: usize, capacity: usize) -> Self {
        assert!(target_block_size > 0);
        assert!(
            capacity >= target_block_size * 2,
            "容量不足以吸收块大小抖动"
        );
        Self {
            input_fifo: VecDeque::with_capacity(capacity),
            output_fifo: VecDeque::with_capacity(capacity),
            scratch_in: vec![0.0; target_block_size].into_boxed_slice(),
            scratch_out: vec![0.0; target_block_size].into_boxed_slice(),
            target_block_size,
            capacity,
            dropped_input_samples: 0,
            zero_filled_samples: 0,
        }
    }

    pub fn target_block_size(&self) -> usize {
        self.target_block_size
    }

    /// 已累积但尚未被 DSP 消费的输入采样数。
    pub fn accumulated(&self) -> usize {
        self.input_fifo.len()
    }

    /// 引入的算法延迟（采样数）：输出 FIFO 中待排出的余量。
    pub fn pending_output(&self) -> usize {
        self.output_fifo.len()
    }

    /// 适配器理论延迟（采样数）。
    ///
    /// 当 `host_block_size < target_block_size` 时，前
    /// `(ceil(target/host) - 1) * host` 个输出采样将为静音零填充。
    ///
    /// 当 `host_block_size >= target_block_size` 时，延迟为 0。
    pub fn theoretical_latency(&self, host_block_size: usize) -> usize {
        if host_block_size == 0 || host_block_size >= self.target_block_size {
            return 0;
        }
        let callbacks_to_first_dsp =
            (self.target_block_size + host_block_size - 1) / host_block_size;
        (callbacks_to_first_dsp - 1) * host_block_size
    }

    pub fn dropped_input_samples(&self) -> u64 {
        self.dropped_input_samples
    }

    pub fn zero_filled_samples(&self) -> u64 {
        self.zero_filled_samples
    }

    /// 宿主回调入口（§3.2.3）。
    ///
    /// 泛型 `F` 强迫编译器单态化内联 DSP 回调。
    /// 流程：累积宿主输入 → 每凑满 target_block 调用一次 DSP →
    /// 从输出 FIFO 排出宿主请求的采样数（不足补零）。
    ///
    /// 所有数据搬运使用批量 `copy_from_slice`（编译为 `memcpy`，
    /// 可利用 SIMD 指令），替代原先逐采样 `pop_front` 的 O(1) * n 路径。
    #[inline]
    pub fn process_host_block<F>(
        &mut self,
        host_inputs: &[f32],
        host_outputs: &mut [f32],
        mut dsp_callback: F,
    ) where
        F: FnMut(&[f32], &mut [f32]),
    {
        // ── 1. 批量累积输入 ──
        // 防御性截断：宿主给出的输入超过容量时丢弃尾部（不变式 #18）
        let space = self.capacity.saturating_sub(self.input_fifo.len());
        let take = host_inputs.len().min(space);
        if take < host_inputs.len() {
            self.dropped_input_samples += (host_inputs.len() - take) as u64;
        }
        self.input_fifo.extend(host_inputs[..take].iter().copied());

        // ── 2. DSP 块处理（每凑满 target 个采样就处理一次）──
        let n = self.target_block_size;
        while self.input_fifo.len() >= n {
            // 批量拷贝到 scratch_in：VecDeque 内部可能分两段存储，
            // make_contiguous 将其合并为连续切片后用 copy_from_slice
            // 替代逐元素 pop —— 编译为 memcpy，缓存友好度显著提升。
            {
                let contig = self.input_fifo.make_contiguous();
                self.scratch_in[..n].copy_from_slice(&contig[..n]);
            }
            // drain 推进读指针（VecDeque::drain 内部为 O(1) 的 head 偏移）
            self.input_fifo.drain(..n);

            // 字段级不相交借用：scratch_in 只读，scratch_out 可写
            dsp_callback(&self.scratch_in[..n], &mut self.scratch_out[..n]);
            self.output_fifo
                .extend(self.scratch_out[..n].iter().copied());
        }

        // ── 3. 批量排出输出 ──
        let drain_len = host_outputs.len().min(self.output_fifo.len());
        if drain_len > 0 {
            // 同样的 make_contiguous + copy_from_slice 策略
            let contig = self.output_fifo.make_contiguous();
            host_outputs[..drain_len].copy_from_slice(&contig[..drain_len]);
            self.output_fifo.drain(..drain_len);
        }
        // 未就绪部分填零（不变式 #18：宁可短暂静音不可播放脏内存）
        let zero_count = host_outputs.len() - drain_len;
        if zero_count > 0 {
            host_outputs[drain_len..].fill(0.0);
            self.zero_filled_samples += zero_count as u64;
        }
    }

    /// 重置所有内部状态（切换采样率/块大小后调用）。
    pub fn reset(&mut self) {
        self.input_fifo.clear();
        self.output_fifo.clear();
        self.scratch_in.fill(0.0);
        self.scratch_out.fill(0.0);
        self.dropped_input_samples = 0;
        self.zero_filled_samples = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 宿主块(384) 与 DSP 块(512) 不对齐时：
    /// - 首个回调无法凑满 DSP 块 → 输出零填充（确定性延迟）
    /// - 延迟后的数据流必须逐采样恒等还原输入流
    /// - 无丢样、无重样
    #[test]
    fn mismatched_block_sizes_preserve_stream() {
        let mut buf = AdapterBuffer::new(512, 4096);
        let host_block = 384usize;
        let total = 512 * 6; // 3072 采样

        // 输入从 1.0 起步，使零填充区间与数据流不含歧义
        let input: Vec<f32> = (0..total).map(|i| (i + 1) as f32).collect();
        let mut output = Vec::with_capacity(total);

        for chunk in input.chunks(host_block) {
            let mut out = vec![f32::NAN; chunk.len()];
            buf.process_host_block(chunk, &mut out, |i, o| o.copy_from_slice(i));
            output.extend_from_slice(&out);
        }

        // 确定性延迟 = (ceil(512/384) - 1) * 384 = 384 采样
        let expected_latency = buf.theoretical_latency(host_block);
        assert_eq!(expected_latency, 384);

        // 验证：前 384 个输出全为 0
        for (i, &v) in output[..expected_latency].iter().enumerate() {
            assert_eq!(v, 0.0, "延迟区采样 {i} 应为零，实际 {v}");
        }

        // 验证：延迟后的数据流 = 输入流前缀（恒等直通）
        let data_len = output.len() - expected_latency;
        for i in 0..data_len {
            assert_eq!(
                output[expected_latency + i],
                input[i],
                "采样 {i} 处流断裂"
            );
        }

        // 验证：因尾部数据仍在 FIFO 中而未被排出的采样数
        // = total_input - data_output = total - data_len
        assert_eq!(buf.dropped_input_samples(), 0, "不应丢弃任何输入");
    }

    #[test]
    fn aligned_blocks_zero_extra_latency() {
        let mut buf = AdapterBuffer::new(256, 1024);
        let input: Vec<f32> = (0..256).map(|i| i as f32 + 1.0).collect();
        let mut out = vec![0.0f32; 256];
        buf.process_host_block(&input, &mut out, |i, o| o.copy_from_slice(i));
        // 宿主块 == DSP 块：同一回调内即可完整产出
        assert_eq!(out, input);
        assert_eq!(buf.pending_output(), 0);
        assert_eq!(buf.theoretical_latency(256), 0);
    }

    /// 极端比例：host=64, dsp=512 → 延迟 = 7*64 = 448
    #[test]
    fn extreme_mismatch_ratio() {
        let mut buf = AdapterBuffer::new(512, 8192);
        let host_block = 64usize;
        let total = 512 * 4;
        let input: Vec<f32> = (0..total).map(|i| (i + 1) as f32).collect();
        let mut output = Vec::with_capacity(total);

        for chunk in input.chunks(host_block) {
            let mut out = vec![0.0f32; chunk.len()];
            buf.process_host_block(chunk, &mut out, |i, o| o.copy_from_slice(i));
            output.extend_from_slice(&out);
        }

        let expected_latency = buf.theoretical_latency(host_block);
        assert_eq!(expected_latency, 448, "7 个空回调 × 64 采样");

        for (i, &v) in output[..expected_latency].iter().enumerate() {
            assert_eq!(v, 0.0, "延迟区采样 {i} 应为零");
        }
        let data_len = output.len() - expected_latency;
        for i in 0..data_len {
            assert_eq!(
                output[expected_latency + i],
                input[i],
                "采样 {i} 流断裂"
            );
        }
    }

    /// 宿主块 > DSP 块：单次回调触发多次 DSP 处理
    #[test]
    fn host_larger_than_dsp() {
        let mut buf = AdapterBuffer::new(128, 4096);
        let host_block = 512;
        let input: Vec<f32> = (0..host_block).map(|i| (i + 1) as f32).collect();
        let mut out = vec![0.0f32; host_block];

        buf.process_host_block(&input, &mut out, |i, o| o.copy_from_slice(i));

        // 延迟 = 0（host >= dsp）：全部数据即时产出
        assert_eq!(buf.theoretical_latency(host_block), 0);
        assert_eq!(out, input);
    }

    /// 容量上限触发防御性截断
    #[test]
    fn capacity_overflow_truncates_safely() {
        let mut buf = AdapterBuffer::new(4, 16);
        let huge = vec![1.0f32; 32];
        let mut out = vec![0.0f32; 32];
        buf.process_host_block(&huge, &mut out, |i, o| o.copy_from_slice(i));
        assert!(buf.dropped_input_samples() > 0, "应丢弃溢出输入");
    }

    /// 长时间运行流连续性（回绕压力测试）
    #[test]
    fn long_running_stream_continuity() {
        let mut buf = AdapterBuffer::new(256, 2048);
        let host_block = 192;
        let total_callbacks = 500;
        let mut global_output = Vec::new();

        for cb in 0..total_callbacks {
            let start = cb * host_block;
            let input: Vec<f32> = (start..start + host_block)
                .map(|i| (i + 1) as f32)
                .collect();
            let mut out = vec![0.0f32; host_block];
            buf.process_host_block(&input, &mut out, |i, o| o.copy_from_slice(i));
            global_output.extend_from_slice(&out);
        }

        // 跳过延迟区
        let latency = buf.theoretical_latency(host_block);
        let data = &global_output[latency..];
        for i in 0..data.len() {
            assert_eq!(data[i], (i + 1) as f32, "采样 {i} 流断裂（长压测）");
        }
        assert_eq!(buf.dropped_input_samples(), 0);
    }

    #[test]
    fn reset_clears_state() {
        let mut buf = AdapterBuffer::new(128, 512);
        let input = vec![1.0f32; 64];
        let mut out = vec![0.0f32; 64];
        buf.process_host_block(&input, &mut out, |i, o| o.copy_from_slice(i));
        assert!(buf.accumulated() > 0 || buf.pending_output() > 0 || buf.zero_filled_samples() > 0);
        buf.reset();
        assert_eq!(buf.accumulated(), 0);
        assert_eq!(buf.pending_output(), 0);
        assert_eq!(buf.zero_filled_samples(), 0);
    }
}
