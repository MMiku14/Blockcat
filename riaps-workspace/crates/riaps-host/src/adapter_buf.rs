//! Adapter Buffer：宿主动态块大小 → DSP 固定块大小的适配层
//! （架构文档 §3.2.3）。
//!
//! 关键设计：
//! - `process_host_block` 泛型单态化：编译期将 DSP process 内联进
//!   Host Adapter，消除 trait object 间接跳转并允许 SIMD 自动向量化
//! - 预分配 scratch buffer：避免音频回调中声明栈上大数组，
//!   消除 LLVM 潜在的 memset(0) 开销（100-300ns）
//! - 防御性截断宿主输入；输出不足时填充零（不变式 #18）
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
}

impl AdapterBuffer {
    /// `capacity` 必须 ≥ 宿主最大块 + DSP 目标块，防止突发写入溢出。
    pub fn new(target_block_size: usize, capacity: usize) -> Self {
        assert!(target_block_size > 0);
        assert!(capacity >= target_block_size * 2, "容量不足以吸收块大小抖动");
        Self {
            input_fifo: VecDeque::with_capacity(capacity),
            output_fifo: VecDeque::with_capacity(capacity),
            scratch_in: vec![0.0; target_block_size].into_boxed_slice(),
            scratch_out: vec![0.0; target_block_size].into_boxed_slice(),
            target_block_size,
            capacity,
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

    /// 宿主回调入口（§3.2.3）。
    ///
    /// 泛型 `F` 强迫编译器单态化内联 DSP 回调。
    /// 流程：累积宿主输入 → 每凑满 target_block 调用一次 DSP →
    /// 从输出 FIFO 排出宿主请求的采样数（不足补零）。
    #[inline]
    pub fn process_host_block<F>(
        &mut self,
        host_inputs: &[f32],
        host_outputs: &mut [f32],
        mut dsp_callback: F,
    ) where
        F: FnMut(&[f32], &mut [f32]),
    {
        // 防御性截断：宿主给出的输入超过容量时丢弃尾部（不变式 #18）
        let space = self.capacity.saturating_sub(self.input_fifo.len());
        let take = host_inputs.len().min(space);
        self.input_fifo.extend(host_inputs[..take].iter().copied());

        // 每凑满一个 DSP 块就处理一次
        let n = self.target_block_size;
        while self.input_fifo.len() >= n {
            for slot in self.scratch_in[..n].iter_mut() {
                // unwrap 安全：len >= n 已检查
                *slot = self.input_fifo.pop_front().unwrap();
            }
            {
                // 字段级不相交借用：scratch_in 只读，scratch_out 可写
                let inb = &self.scratch_in[..n];
                let outb = &mut self.scratch_out[..n];
                dsp_callback(inb, outb);
            }
            self.output_fifo.extend(self.scratch_out[..n].iter().copied());
        }

        // 排出宿主请求的输出；未就绪部分填零（不变式 #18：
        // 未初始化输出必须填充零，宁可短暂静音不可播放脏内存）
        for out in host_outputs.iter_mut() {
            *out = self.output_fifo.pop_front().unwrap_or(0.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 宿主块(384) 与 DSP 块(512) 不对齐时，适配层必须保证
    /// 数据完整性（不丢样、不重样）且以零填充未就绪区间。
    #[test]
    fn mismatched_block_sizes_preserve_stream() {
        let mut buf = AdapterBuffer::new(512, 4096);
        let host_block = 384;
        let total = 512 * 6; // 3072 采样
        let input: Vec<f32> = (0..total).map(|i| i as f32).collect();
        let mut output = Vec::new();

        for chunk in input.chunks(host_block) {
            let mut out = vec![999.0f32; chunk.len()];
            // DSP = 恒等直通
            buf.process_host_block(chunk, &mut out, |i, o| o.copy_from_slice(i));
            output.extend_from_slice(&out);
        }

        // 前 512 采样为适配延迟（零填充），其后应恒等还原输入流
        let latency = output.iter().take_while(|&&v| v == 0.0).count();
        assert!(latency >= 1, "块不对齐必然产生适配延迟");
        for (i, &v) in output[latency..].iter().enumerate() {
            assert_eq!(v, i as f32, "采样 {i} 处流断裂");
        }
    }

    #[test]
    fn aligned_blocks_zero_extra_latency_after_first() {
        let mut buf = AdapterBuffer::new(256, 1024);
        let input: Vec<f32> = (0..256).map(|i| i as f32 + 1.0).collect();
        let mut out = vec![0.0f32; 256];
        buf.process_host_block(&input, &mut out, |i, o| o.copy_from_slice(i));
        // 宿主块 == DSP 块：同一回调内即可完整产出
        assert_eq!(out, input);
        assert_eq!(buf.pending_output(), 0);
    }
}
