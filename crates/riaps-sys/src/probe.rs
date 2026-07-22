//! 无锁性能探针（架构文档 §4.3.1）。
//!
//! 时钟源（§2.2.2 / 不变式 #15）：
//! - x86_64:  invariant TSC（`rdtsc`）
//! - AArch64: `CNTVCT_EL0`
//! - 其他:    `std::time::Instant` 单调时钟回退
//!
//! 目标时延：探针本体 ~10ns（§4.6）。

use core::mem::MaybeUninit;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use riaps_core::spsc::SpscRing;

/// 读取平台单调时钟 tick（非阻塞指令，允许在音频回调内使用，不变式 #2）。
#[inline(always)]
pub fn read_monotonic_tick() -> u64 {
    #[cfg(target_arch = "x86_64")]
    // SAFETY: rdtsc 无内存副作用
    return unsafe { core::arch::x86_64::_rdtsc() };

    #[cfg(target_arch = "aarch64")]
    {
        let v: u64;
        // SAFETY: CNTVCT_EL0 为 EL0 可读的虚拟计数器
        unsafe {
            core::arch::asm!("mrs {}, cntvct_el0", out(reg) v, options(nomem, nostack));
        }
        return v;
    }

    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    {
        use std::sync::OnceLock;
        use std::time::Instant;
        static START: OnceLock<Instant> = OnceLock::new();
        START.get_or_init(Instant::now).elapsed().as_nanos() as u64
    }
}

/// 单次测量记录。
#[derive(Debug, Clone, Copy)]
pub struct ProbeRecord {
    pub node_id: u32,
    pub start_tick: u64,
    pub end_tick: u64,
}

const PROBE_RING_CAP: usize = 1024;

/// 无锁性能探针。
///
/// 并发契约：`measure` 仅由音频线程调用（生产者侧）；
/// `drain` 仅由监控线程调用（消费者侧）。
///
/// 不变式 #20：嵌套测量防御（`nesting_depth`）+ 溢出计数。
pub struct PerformanceProbe {
    ring: SpscRing<ProbeRecord, PROBE_RING_CAP>,
    enabled: AtomicBool,
    nesting_depth: AtomicU32,
    overflow_counter: AtomicU32,
}

impl PerformanceProbe {
    pub fn new() -> Self {
        Self {
            ring: SpscRing::new(),
            enabled: AtomicBool::new(false),
            nesting_depth: AtomicU32::new(0),
            overflow_counter: AtomicU32::new(0),
        }
    }

    pub fn set_enabled(&self, on: bool) {
        self.enabled.store(on, Ordering::Relaxed);
    }

    pub fn overflow_count(&self) -> u32 {
        self.overflow_counter.load(Ordering::Relaxed)
    }

    /// 音频线程侧：包裹测量闭包。禁用或嵌套时零测量开销直通。
    #[inline(always)]
    pub fn measure<F, R>(&self, node_id: u32, f: F) -> R
    where
        F: FnOnce() -> R,
    {
        if !self.enabled.load(Ordering::Relaxed) {
            return f();
        }
        // 嵌套测量防御：只测最外层，内层直通
        if self.nesting_depth.fetch_add(1, Ordering::Relaxed) > 0 {
            let result = f();
            self.nesting_depth.fetch_sub(1, Ordering::Relaxed);
            return result;
        }
        let start = read_monotonic_tick();
        let result = f();
        let end = read_monotonic_tick();

        // SAFETY: measure 仅由音频线程调用（单生产者契约）
        let mut producer = unsafe { self.ring.producer() };
        if producer
            .push(ProbeRecord {
                node_id,
                start_tick: start,
                end_tick: end,
            })
            .is_err()
        {
            // 队列满：丢弃记录并计数，绝不阻塞音频线程（不变式 #7）
            self.overflow_counter.fetch_add(1, Ordering::Relaxed);
        }
        self.nesting_depth.fetch_sub(1, Ordering::Relaxed);
        result
    }

    /// 监控线程侧：排空记录。
    pub fn drain(&self, mut sink: impl FnMut(ProbeRecord)) {
        // SAFETY: drain 仅由监控线程调用（单消费者契约）
        let mut consumer = unsafe { self.ring.consumer() };
        while let Some(record) = consumer.pop() {
            sink(record);
        }
    }

    /// 便捷接口：批量拷贝导出（演示 §2.1.1 的 pop_slice_copy 用法）。
    pub fn drain_batch<const M: usize>(&self) -> Option<[ProbeRecord; M]> {
        // SAFETY: 单消费者契约同上
        let mut consumer = unsafe { self.ring.consumer() };
        let mut dst = [MaybeUninit::<ProbeRecord>::uninit(); M];
        consumer.pop_slice_copy(&mut dst).ok()?;
        // 稳定版替代 `MaybeUninit::array_assume_init`：逐元素读出
        let mut out = [ProbeRecord {
            node_id: 0,
            start_tick: 0,
            end_tick: 0,
        }; M];
        for (o, s) in out.iter_mut().zip(dst.iter()) {
            // SAFETY: pop_slice_copy 成功 ⟹ 全部 M 个元素已初始化
            *o = unsafe { s.assume_init() };
        }
        Some(out)
    }
}

impl Default for PerformanceProbe {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tick_is_monotonic_enough() {
        let a = read_monotonic_tick();
        let mut x = 0u64;
        for i in 0..10_000u64 {
            x = x.wrapping_add(i);
        }
        std::hint::black_box(x);
        let b = read_monotonic_tick();
        assert!(b >= a, "时钟不得回退: {a} -> {b}");
    }

    #[test]
    fn probe_disabled_is_passthrough() {
        let probe = PerformanceProbe::new();
        let v = probe.measure(1, || 42);
        assert_eq!(v, 42);
        let mut n = 0;
        probe.drain(|_| n += 1);
        assert_eq!(n, 0, "禁用状态不得产生记录");
    }

    #[test]
    fn probe_records_and_drains() {
        let probe = PerformanceProbe::new();
        probe.set_enabled(true);
        for id in 0..8u32 {
            probe.measure(id, || std::hint::black_box(id * 2));
        }
        let mut ids = Vec::new();
        probe.drain(|r| {
            assert!(r.end_tick >= r.start_tick);
            ids.push(r.node_id);
        });
        assert_eq!(ids, (0..8).collect::<Vec<_>>());
        assert_eq!(probe.overflow_count(), 0);
    }
}
