//! DspNode trait 与节点专属无锁参数句柄（架构文档 §2.3.4）。
//!
//! 参数更新原则：避免在音频热路径使用 `Any::downcast_ref`；
//! 每个节点自带小容量 SPSC `ParameterQueue`，UI 线程 push，
//! 音频线程在 `process` 开头 drain（只保留最新值）。
//!
//! 内置节点：
//! - [`GainNode`]    增益（dB → 线性，含无锁参数队列）
//! - [`PassThrough`] 恒等直通（零开销基准）
//! - [`SumMixer`]    逐采样加法（多输入汇节点用途）
//! - [`DelayNode`]   固定采样延迟（环形缓冲实现）

use riaps_core::spsc::SpscRing;

/// 单块处理上下文。
#[derive(Debug, Clone, Copy)]
pub struct ProcessContext {
    pub sample_rate: u32,
    pub block_size: usize,
}

/// DSP 节点 trait（§2.3.4）。
///
/// 契约：`process` 运行在音频线程 —— 零分配、零锁、零阻塞系统调用
/// （不变式 #1/#2）。
pub trait DspNode: Send + 'static {
    fn process(&mut self, inputs: &[f32], outputs: &mut [f32], ctx: &ProcessContext);

    fn reset(&mut self) {}

    /// 节点引入的算法延迟（采样数），用于图级延迟补偿。
    fn latency_samples(&self) -> usize {
        0
    }

    /// CPU 预算（微秒）。Performance Probe 超预算时发射
    /// `PerformanceProbeOverload` 事件（§4.1）。
    fn cpu_budget_us(&self) -> u32 {
        1000
    }
}

/// 节点专属无锁参数队列（容量 8-16 的 SPSC，§2.3.4）。
///
/// 并发契约：`try_push` 仅 UI 线程调用；`drain_to` 仅音频线程调用。
pub struct ParameterQueue<T, const N: usize> {
    ring: SpscRing<T, N>,
}

impl<T: Copy + Send, const N: usize> ParameterQueue<T, N> {
    pub fn new() -> Self {
        Self {
            ring: SpscRing::new(),
        }
    }

    /// UI 线程侧：投递参数变更。队列满返回 false（丢弃本次更新，
    /// UI 可稍后重试；绝不阻塞）。
    #[inline(always)]
    pub fn try_push(&self, value: T) -> bool {
        // SAFETY: 契约 —— 仅 UI 线程（单生产者）调用
        let mut producer = unsafe { self.ring.producer() };
        producer.push(value).is_ok()
    }

    /// 音频线程侧：排空队列，只保留最新值，丢弃中间状态。
    #[inline(always)]
    pub fn drain_to(&self, dst: &mut T) -> bool {
        // SAFETY: 契约 —— 仅音频线程（单消费者）调用
        let mut consumer = unsafe { self.ring.consumer() };
        let mut updated = false;
        while let Some(v) = consumer.pop() {
            *dst = v;
            updated = true;
        }
        updated
    }

    /// 音频线程侧：查看是否有待处理的更新（不消费）。
    #[inline(always)]
    pub fn has_pending(&self) -> bool {
        !self.ring.is_empty()
    }
}

impl<T: Copy + Send, const N: usize> Default for ParameterQueue<T, N> {
    fn default() -> Self {
        Self::new()
    }
}

// ━━━━━━━━━━━━━━━━━━━━━━━ 内置节点 ━━━━━━━━━━━━━━━━━━━━━━━

#[inline(always)]
fn db_to_linear(db: f32) -> f32 {
    10.0f32.powf(db / 20.0)
}

// ── PassThrough ──

/// 恒等直通节点：输出 = 输入。零开销基准 / 调试占位。
pub struct PassThrough;

impl DspNode for PassThrough {
    #[inline(always)]
    fn process(&mut self, inputs: &[f32], outputs: &mut [f32], _ctx: &ProcessContext) {
        let n = inputs.len().min(outputs.len());
        outputs[..n].copy_from_slice(&inputs[..n]);
    }

    fn cpu_budget_us(&self) -> u32 {
        10
    }
}

// ── SumMixer ──

/// 逐采样加法混合节点。
///
/// 在多输入汇点使用：图引擎已将所有前驱的输出逐采样加法混合后
/// 传入 `inputs`，SumMixer 直通即可。如需额外增益可链接 GainNode。
pub struct SumMixer;

impl DspNode for SumMixer {
    #[inline(always)]
    fn process(&mut self, inputs: &[f32], outputs: &mut [f32], _ctx: &ProcessContext) {
        // 图引擎已完成混合；此处直通
        let n = inputs.len().min(outputs.len());
        outputs[..n].copy_from_slice(&inputs[..n]);
    }

    fn cpu_budget_us(&self) -> u32 {
        10
    }
}

// ── GainNode ──

/// 增益节点（对应 §1.3 的 `AudioCommand::SetGain`）。
///
/// 支持通过 [`ParameterQueue`] 从 UI 线程无锁更新增益（dB）。
pub struct GainNode {
    gain_db: f32,
    gain_lin: f32,
    /// UI 线程持有共享引用即可投递参数（dB 值）
    pub param_db: ParameterQueue<f32, 16>,
}

impl GainNode {
    pub fn new(gain_db: f32) -> Self {
        Self {
            gain_db,
            gain_lin: db_to_linear(gain_db),
            param_db: ParameterQueue::new(),
        }
    }

    pub fn gain_db(&self) -> f32 {
        self.gain_db
    }

    pub fn gain_linear(&self) -> f32 {
        self.gain_lin
    }
}

impl DspNode for GainNode {
    fn process(&mut self, inputs: &[f32], outputs: &mut [f32], _ctx: &ProcessContext) {
        // 热路径开头 drain 参数（§2.3.4）
        let mut db = self.gain_db;
        if self.param_db.drain_to(&mut db) {
            self.gain_db = db;
            self.gain_lin = db_to_linear(db);
        }
        let g = self.gain_lin;
        let n = inputs.len().min(outputs.len());
        for i in 0..n {
            outputs[i] = inputs[i] * g;
        }
    }

    fn reset(&mut self) {
        // 增益无内部状态
    }

    fn cpu_budget_us(&self) -> u32 {
        50
    }
}

// ── DelayNode ──

/// 固定采样延迟节点（环形缓冲实现）。
///
/// 分配发生在 `new`（初始化阶段）；`process` 零分配。
pub struct DelayNode {
    buffer: Box<[f32]>,
    write_pos: usize,
    delay_samples: usize,
}

impl DelayNode {
    /// 创建固定延迟节点。`delay_samples` 必须 > 0 且 ≤ `max_delay`。
    pub fn new(delay_samples: usize, max_delay: usize) -> Self {
        assert!(delay_samples > 0 && delay_samples <= max_delay);
        Self {
            buffer: vec![0.0; max_delay].into_boxed_slice(),
            write_pos: 0,
            delay_samples,
        }
    }
}

impl DspNode for DelayNode {
    fn process(&mut self, inputs: &[f32], outputs: &mut [f32], _ctx: &ProcessContext) {
        let n = inputs.len().min(outputs.len());
        let buf_len = self.buffer.len();
        for i in 0..n {
            // 读取 delay 采样前的数据
            let read_pos = (self.write_pos + buf_len - self.delay_samples) % buf_len;
            outputs[i] = self.buffer[read_pos];
            self.buffer[self.write_pos] = inputs[i];
            self.write_pos = (self.write_pos + 1) % buf_len;
        }
    }

    fn reset(&mut self) {
        self.buffer.fill(0.0);
        self.write_pos = 0;
    }

    fn latency_samples(&self) -> usize {
        self.delay_samples
    }

    fn cpu_budget_us(&self) -> u32 {
        100
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> ProcessContext {
        ProcessContext {
            sample_rate: 96_000,
            block_size: 4,
        }
    }

    #[test]
    fn parameter_queue_keeps_latest_only() {
        let q = ParameterQueue::<f32, 8>::new();
        assert!(q.try_push(1.0));
        assert!(q.try_push(2.0));
        assert!(q.try_push(3.0));
        let mut v = 0.0;
        assert!(q.drain_to(&mut v));
        assert_eq!(v, 3.0, "必须只保留最新值");
        assert!(!q.drain_to(&mut v), "排空后无更新");
    }

    #[test]
    fn parameter_queue_has_pending() {
        let q = ParameterQueue::<f32, 8>::new();
        assert!(!q.has_pending());
        q.try_push(1.0);
        assert!(q.has_pending());
        let mut v = 0.0;
        q.drain_to(&mut v);
        assert!(!q.has_pending());
    }

    #[test]
    fn gain_node_applies_pushed_param() {
        let mut node = GainNode::new(0.0);
        let input = [1.0f32; 4];
        let mut output = [0.0f32; 4];

        node.process(&input, &mut output, &ctx());
        assert_eq!(output, [1.0; 4], "0 dB = 直通");

        node.param_db.try_push(-6.0);
        node.process(&input, &mut output, &ctx());
        let expected = 10.0f32.powf(-6.0 / 20.0);
        for &o in &output {
            assert!((o - expected).abs() < 1e-6);
        }
    }

    #[test]
    fn passthrough_is_identity() {
        let mut node = PassThrough;
        let input = [1.0, 2.0, 3.0, 4.0f32];
        let mut output = [0.0f32; 4];
        node.process(&input, &mut output, &ctx());
        assert_eq!(output, input);
    }

    #[test]
    fn sum_mixer_passes_through() {
        let mut node = SumMixer;
        let input = [3.0, 6.0, 9.0, 12.0f32];
        let mut output = [0.0f32; 4];
        node.process(&input, &mut output, &ctx());
        assert_eq!(output, input);
    }

    #[test]
    fn delay_node_delays_by_n_samples() {
        let mut node = DelayNode::new(3, 16);
        assert_eq!(node.latency_samples(), 3);

        // 输入脉冲 [1, 0, 0, 0, 0, 0, 0, 0]
        let input = [1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0f32];
        let mut output = [0.0f32; 8];
        node.process(&input, &mut output, &ctx());

        // 延迟 3 采样：输出 [0, 0, 0, 1, 0, 0, 0, 0]
        assert_eq!(output[0], 0.0);
        assert_eq!(output[1], 0.0);
        assert_eq!(output[2], 0.0);
        assert_eq!(output[3], 1.0, "脉冲应在第 3 采样后出现");
        assert_eq!(output[4], 0.0);
    }

    #[test]
    fn delay_node_reset_clears_buffer() {
        let mut node = DelayNode::new(2, 8);
        let input = [1.0, 1.0, 1.0, 1.0f32];
        let mut output = [0.0f32; 4];
        node.process(&input, &mut output, &ctx());
        node.reset();
        // reset 后再处理，不应有残留
        let input2 = [0.0; 4];
        let mut output2 = [999.0f32; 4];
        node.process(&input2, &mut output2, &ctx());
        assert_eq!(output2, [0.0; 4], "reset 后不应有残留信号");
    }
}
