//! DspNode trait 与节点专属无锁参数句柄（架构文档 §2.3.4）。
//!
//! 参数更新原则：避免在音频热路径使用 `Any::downcast_ref`；
//! 每个节点自带小容量 SPSC `ParameterQueue`，UI 线程 push，
//! 音频线程在 `process` 开头 drain（只保留最新值）。

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
}

impl<T: Copy + Send, const N: usize> Default for ParameterQueue<T, N> {
    fn default() -> Self {
        Self::new()
    }
}

#[inline(always)]
fn db_to_linear(db: f32) -> f32 {
    10.0f32.powf(db / 20.0)
}

/// 示例节点：增益（对应 §1.3 的 `AudioCommand::SetGain`）。
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
        for (o, i) in outputs.iter_mut().zip(inputs.iter()) {
            *o = *i * g;
        }
    }

    fn reset(&mut self) {
        // 增益无内部状态
    }

    fn cpu_budget_us(&self) -> u32 {
        50
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn gain_node_applies_pushed_param() {
        let mut node = GainNode::new(0.0);
        let ctx = ProcessContext {
            sample_rate: 96_000,
            block_size: 4,
        };
        let input = [1.0f32; 4];
        let mut output = [0.0f32; 4];

        node.process(&input, &mut output, &ctx);
        assert_eq!(output, [1.0; 4], "0 dB = 直通");

        node.param_db.try_push(-6.0);
        node.process(&input, &mut output, &ctx);
        let expected = 10.0f32.powf(-6.0 / 20.0);
        for &o in &output {
            assert!((o - expected).abs() < 1e-6);
        }
    }
}
