//! 缓存行对齐的 SPSC（单生产者单消费者）环形缓冲区（架构文档 §2.1）。
//!
//! 设计要点：
//! - head/tail 为单调递增的无界索引，槽位 = idx % N，满判定 = tail - head == N
//! - 跨线程可见性仅通过 Acquire/Release（不变式 #3）
//! - 生产者/消费者侧各自缓存行对齐，消除 false sharing（§2.1.2）
//! - `push`/`pop` 热路径零分配、零系统调用、wait-free

use core::cell::UnsafeCell;
use core::mem::MaybeUninit;
use core::sync::atomic::{AtomicUsize, Ordering};

/// 缓存行对齐容器。128 字节覆盖 Apple Silicon（128B 缓存行）与
/// x86_64 相邻行预取器（adjacent line prefetcher）两种场景。
///
/// 工业级替代方案：`crossbeam_utils::CachePadded<T>`，它处理了
/// 编译器布局优化的所有边缘场景（§2.1.2）。
#[repr(align(128))]
#[derive(Debug)]
pub struct CachePadded<T> {
    pub value: T,
}

impl<T> CachePadded<T> {
    #[inline(always)]
    pub const fn new(value: T) -> Self {
        Self { value }
    }
}

/// SPSC 操作错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpscError {
    /// 队列已满（生产者侧）
    Full,
    /// 队列为空（消费者侧）
    Empty,
    /// 批量读取时可用元素不足 M 个
    InsufficientData,
}

/// SPSC 环形缓冲区本体。
///
/// # 并发契约
/// 任意时刻至多存在一个 [`Producer`] 与一个 [`Consumer`]。
/// 通过 [`SpscRing::split`]（unsafe）获取两端句柄，调用者负责
/// 保证两端分别只被一个线程持有（§4.1：严禁生产者跨界消费）。
pub struct SpscRing<T, const N: usize> {
    /// 消费者索引（仅消费者写入）— 独占缓存行
    head: CachePadded<AtomicUsize>,
    /// 生产者索引（仅生产者写入）— 独占缓存行
    tail: CachePadded<AtomicUsize>,
    buf: [UnsafeCell<MaybeUninit<T>>; N],
}

// SAFETY: 单生产者/单消费者契约下，每个槽位在任一时刻只被一侧访问；
// head/tail 通过 Acquire/Release 建立 happens-before。
unsafe impl<T: Send, const N: usize> Sync for SpscRing<T, N> {}
unsafe impl<T: Send, const N: usize> Send for SpscRing<T, N> {}

impl<T, const N: usize> SpscRing<T, N> {
    pub fn new() -> Self {
        Self {
            head: CachePadded::new(AtomicUsize::new(0)),
            tail: CachePadded::new(AtomicUsize::new(0)),
            buf: core::array::from_fn(|_| UnsafeCell::new(MaybeUninit::uninit())),
        }
    }

    #[inline(always)]
    pub const fn capacity(&self) -> usize {
        N
    }

    /// 当前元素数量（跨线程观测值，仅供监控/背压使用）。
    #[inline(always)]
    pub fn len(&self) -> usize {
        let tail = self.tail.value.load(Ordering::Acquire);
        let head = self.head.value.load(Ordering::Acquire);
        tail.wrapping_sub(head)
    }

    #[inline(always)]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 剩余容量（EventBus 保留槽位机制依赖此接口，§4.1）。
    #[inline(always)]
    pub fn capacity_left(&self) -> usize {
        N - self.len().min(N)
    }

    /// 获取生产者句柄。
    ///
    /// # Safety
    /// 调用者必须保证任意时刻至多一个线程通过 Producer 写入。
    #[inline(always)]
    pub unsafe fn producer(&self) -> Producer<'_, T, N> {
        Producer { ring: self }
    }

    /// 获取消费者句柄。
    ///
    /// # Safety
    /// 调用者必须保证任意时刻至多一个线程通过 Consumer 读取。
    #[inline(always)]
    pub unsafe fn consumer(&self) -> Consumer<'_, T, N> {
        Consumer { ring: self }
    }

    /// 一次性拆分为生产者/消费者两端。
    ///
    /// # Safety
    /// 同 [`Self::producer`] / [`Self::consumer`] 的并发契约。
    #[inline(always)]
    pub unsafe fn split(&self) -> (Producer<'_, T, N>, Consumer<'_, T, N>) {
        // SAFETY: 契约由调用者传递
        unsafe { (self.producer(), self.consumer()) }
    }
}

impl<T, const N: usize> Default for SpscRing<T, N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T, const N: usize> Drop for SpscRing<T, N> {
    fn drop(&mut self) {
        // &mut self 独占访问，无需原子序
        let head = *self.head.value.get_mut();
        let tail = *self.tail.value.get_mut();
        let mut i = head;
        while i != tail {
            // SAFETY: [head, tail) 区间内的槽位均已初始化
            unsafe { (*self.buf[i % N].get()).assume_init_drop() };
            i = i.wrapping_add(1);
        }
    }
}

/// 生产者句柄（单线程持有）。
pub struct Producer<'a, T, const N: usize> {
    ring: &'a SpscRing<T, N>,
}

// SAFETY: Producer 可以移交给另一个线程（但同一时刻只有一个）
unsafe impl<T: Send, const N: usize> Send for Producer<'_, T, N> {}

impl<T, const N: usize> Producer<'_, T, N> {
    /// wait-free push。目标时延 < 25ns（§4.6）。
    #[inline(always)]
    pub fn push(&mut self, value: T) -> Result<(), T> {
        let ring = self.ring;
        let tail = ring.tail.value.load(Ordering::Relaxed);
        let head = ring.head.value.load(Ordering::Acquire);
        if tail.wrapping_sub(head) == N {
            return Err(value);
        }
        // SAFETY: 槽位 tail % N 当前对生产者独占（未被消费者持有）
        unsafe { (*ring.buf[tail % N].get()).write(value) };
        ring.tail.value.store(tail.wrapping_add(1), Ordering::Release);
        Ok(())
    }
}

/// 消费者句柄（单线程持有）。
pub struct Consumer<'a, T, const N: usize> {
    ring: &'a SpscRing<T, N>,
}

unsafe impl<T: Send, const N: usize> Send for Consumer<'_, T, N> {}

impl<T, const N: usize> Consumer<'_, T, N> {
    /// wait-free pop。
    #[inline(always)]
    pub fn pop(&mut self) -> Option<T> {
        let ring = self.ring;
        let head = ring.head.value.load(Ordering::Relaxed);
        let tail = ring.tail.value.load(Ordering::Acquire);
        if tail == head {
            return None;
        }
        // SAFETY: [head, tail) 内的槽位已由生产者初始化并 Release 发布
        let value = unsafe { (*ring.buf[head % N].get()).assume_init_read() };
        ring.head.value.store(head.wrapping_add(1), Ordering::Release);
        Some(value)
    }

    /// 批量拷贝弹出（§2.1.1）。目标时延 pop_slice(16) < 35ns（§4.6）。
    ///
    /// 不变式 #3 附加约束：SPSC 容量必须大于最大突发写入量与读取周期
    /// 的乘积以实现物理防重写；否则必须退化为逐元素 Acquire 读取
    /// （本实现即逐元素读取形式，天然满足约束）。
    #[inline]
    pub fn pop_slice_copy<const M: usize>(
        &mut self,
        dst: &mut [MaybeUninit<T>; M],
    ) -> Result<(), SpscError>
    where
        T: Copy,
    {
        let ring = self.ring;
        let head = ring.head.value.load(Ordering::Relaxed);
        let tail = ring.tail.value.load(Ordering::Acquire);
        if tail.wrapping_sub(head) < M {
            return Err(SpscError::InsufficientData);
        }
        for (i, slot) in dst.iter_mut().enumerate() {
            // SAFETY: [head, head+M) ⊆ [head, tail)，均已初始化
            let v = unsafe { (*ring.buf[head.wrapping_add(i) % N].get()).assume_init() };
            slot.write(v);
        }
        ring.head.value.store(head.wrapping_add(M), Ordering::Release);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spsc_push_pop_roundtrip() {
        let ring = SpscRing::<u32, 8>::new();
        // SAFETY: 单线程测试，两端各持有一份
        let (mut p, mut c) = unsafe { ring.split() };
        for i in 0..8u32 {
            assert!(p.push(i).is_ok());
        }
        assert_eq!(p.push(99), Err(99), "满队列必须拒绝写入");
        for i in 0..8u32 {
            assert_eq!(c.pop(), Some(i));
        }
        assert_eq!(c.pop(), None);
    }

    #[test]
    fn spsc_wraparound() {
        let ring = SpscRing::<u64, 4>::new();
        let (mut p, mut c) = unsafe { ring.split() };
        for round in 0..100u64 {
            assert!(p.push(round).is_ok());
            assert_eq!(c.pop(), Some(round));
        }
        assert!(ring.is_empty());
    }

    #[test]
    fn spsc_pop_slice_copy() {
        let ring = SpscRing::<f32, 32>::new();
        let (mut p, mut c) = unsafe { ring.split() };
        for i in 0..16 {
            p.push(i as f32).unwrap();
        }
        let mut dst = [MaybeUninit::<f32>::uninit(); 16];
        c.pop_slice_copy(&mut dst).unwrap();
        for (i, slot) in dst.iter().enumerate() {
            // SAFETY: pop_slice_copy 成功后 dst 全部初始化
            assert_eq!(unsafe { slot.assume_init() }, i as f32);
        }
        // 数据不足时必须报错
        let mut dst2 = [MaybeUninit::<f32>::uninit(); 4];
        assert_eq!(c.pop_slice_copy(&mut dst2), Err(SpscError::InsufficientData));
    }

    #[test]
    fn spsc_capacity_left() {
        let ring = SpscRing::<u8, 16>::new();
        let (mut p, _c) = unsafe { ring.split() };
        assert_eq!(ring.capacity_left(), 16);
        for i in 0..10 {
            p.push(i).unwrap();
        }
        assert_eq!(ring.capacity_left(), 6);
    }
}
