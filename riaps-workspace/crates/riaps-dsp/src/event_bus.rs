//! 跨模块事件总线（架构文档 §4.1）。
//!
//! 拓扑：SPSC。多生产者场景使用 Sharded SPSC（每个生产者一个独立
//! 通道，消费者轮询）—— 本类型即单个分片。
//!
//! 背压原则：事件总线是监控/诊断通道，不是音频数据通道。队列满时
//! **直接丢弃新事件并计数**；生产者绝不调用 consumer().pop() 跨界
//! 消费（会破坏 SPSC 单写者不变式，导致数据竞争/UB）。
//!
//! 保留槽位机制：最后 [`RESERVED_SLOTS`] 个槽位仅供致命事件使用，
//! 防止海量 `PerformanceProbeOverload` 挤占关键通知通道。
//!
//! 目标时延：emit < 30ns（§4.6）。

use std::sync::atomic::{AtomicU32, Ordering};

use riaps_core::spsc::SpscRing;

/// 为致命事件保留的槽位数。
pub const RESERVED_SLOTS: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    Requested,
    HostError,
    Panic,
}

#[derive(Debug, Clone, Copy)]
pub struct SpscBackpressureEvent {
    /// 队列占用率（百分比）
    pub occupancy_pct: u8,
}

#[derive(Debug, Clone, Copy)]
pub struct GcBackpressureEvent {
    /// GcPolicy::Adaptive 触发的级别（0/1/2）
    pub level: u8,
}

/// 内核事件（§4.1）。全部 `Copy`，emit 路径零分配。
#[derive(Debug, Clone, Copy)]
pub enum KernelEvent {
    SpscBackpressure(SpscBackpressureEvent),
    SpscOverflow { dropped_commands: u32 },
    GcBackpressure(GcBackpressureEvent),
    GcEmergencySlotUsed { slot_index: u8 },
    GcOrphanReclaimed { count: u32 },
    LogQueueOverflow { dropped_records: u32 },
    LogFlushComplete { records_flushed: u32 },
    FpuSubnormalDetected { count: u32 },
    AudioThreadStarted,
    AudioThreadStopped { reason: StopReason },
    ShutdownProgress { step: u8, total: u8 },
    HostAdapterBlockSizeChanged { old: usize, new: usize },
    ThreadProvisionerFallback { requested: i32, granted: i32 },
    PerformanceProbeOverload { node_id: u32, elapsed_us: u32, budget_us: u32 },
}

impl KernelEvent {
    /// 致命事件可使用保留槽位。
    #[inline(always)]
    pub fn is_fatal(&self) -> bool {
        matches!(
            self,
            KernelEvent::AudioThreadStopped { .. }
                | KernelEvent::HostAdapterBlockSizeChanged { .. }
                | KernelEvent::SpscOverflow { .. }
        )
    }
}

/// 单分片事件总线。
///
/// 并发契约：`emit` 仅由一个生产者线程调用；`drain` 仅由消费者
/// 线程（非实时监控平面）调用。
pub struct EventBus<const N: usize> {
    ring: SpscRing<KernelEvent, N>,
    overflow_counter: AtomicU32,
}

impl<const N: usize> EventBus<N> {
    pub fn new() -> Self {
        assert!(N > RESERVED_SLOTS, "容量必须大于保留槽位数");
        Self {
            ring: SpscRing::new(),
            overflow_counter: AtomicU32::new(0),
        }
    }

    /// 生产者线程调用 —— 永不阻塞，永不跨界消费（§4.1）。
    #[inline(always)]
    pub fn emit(&self, event: KernelEvent) {
        let is_fatal = event.is_fatal();
        let capacity_left = self.ring.capacity_left();

        if capacity_left <= RESERVED_SLOTS && !is_fatal {
            // 保留区已满，非致命事件直接丢弃
            self.overflow_counter.fetch_add(1, Ordering::Relaxed);
            return;
        }

        // SAFETY: 契约 —— 单生产者线程
        let mut producer = unsafe { self.ring.producer() };
        if producer.push(event).is_err() {
            // 正确做法：直接丢弃新事件，计数溢出。
            self.overflow_counter.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// 消费者线程调用：排空事件。
    pub fn drain(&self, mut sink: impl FnMut(KernelEvent)) {
        // SAFETY: 契约 —— 单消费者线程
        let mut consumer = unsafe { self.ring.consumer() };
        while let Some(event) = consumer.pop() {
            sink(event);
        }
    }

    pub fn overflow_count(&self) -> u32 {
        self.overflow_counter.load(Ordering::Relaxed)
    }

    pub fn len(&self) -> usize {
        self.ring.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ring.is_empty()
    }
}

impl<const N: usize> Default for EventBus<N> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserved_slots_protect_fatal_events() {
        let bus = EventBus::<16>::new();

        // 用非致命事件填满非保留区（16 - 8 = 8 个）
        for i in 0..8 {
            bus.emit(KernelEvent::PerformanceProbeOverload {
                node_id: i,
                elapsed_us: 2000,
                budget_us: 1000,
            });
        }
        assert_eq!(bus.len(), 8);
        assert_eq!(bus.overflow_count(), 0);

        // 第 9 个非致命事件应被丢弃（保留区触发）
        bus.emit(KernelEvent::GcOrphanReclaimed { count: 1 });
        assert_eq!(bus.len(), 8);
        assert_eq!(bus.overflow_count(), 1);

        // 致命事件仍可进入保留区
        bus.emit(KernelEvent::AudioThreadStopped {
            reason: StopReason::HostError,
        });
        assert_eq!(bus.len(), 9);

        // 消费端验证顺序与内容
        let mut fatal_seen = false;
        bus.drain(|e| {
            if matches!(e, KernelEvent::AudioThreadStopped { .. }) {
                fatal_seen = true;
            }
        });
        assert!(fatal_seen, "致命事件不得丢失");
        assert!(bus.is_empty());
    }
}
