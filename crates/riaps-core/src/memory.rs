//! 异步无损回收系统：Provenance 保护、DropVTable、Emergency Slots、
//! DeferredDropQueue（架构文档 §2.2）。
//!
//! - **Logical Drop**：音频线程将所有权转移至 Deferred Drop Queue（O(1) 无界等待）
//! - **Physical Drop**：GC 线程执行 `drop_in_place` + `dealloc`
//! - **Emergency Slots**：GC 队列满时的崩溃兜底；Watchdog 线程负责超时扫描
//!   （音频线程不维护超时状态机，§2.2.2）
//! - **DeferredDropQueue**：基于 SPSC 的类型擦除回收队列
//!
//! **回收协议（双层隔离，§2.3.2）**：
//! 音频线程与 UI 线程严禁共享同一个 Deferred Drop 入口。
//! 音频线程使用 audio-LIFO（`EmergencySlot` 数组），
//! UI 线程使用 `DeferredDropQueue`（可阻塞等待）。
//! GC 线程分别消费两个队列。

use core::alloc::Layout;
use core::cell::UnsafeCell;
use core::mem::MaybeUninit;
use core::ptr;
use core::sync::atomic::{fence, AtomicBool, AtomicPtr, AtomicU32, Ordering};

use alloc::alloc::dealloc;
use alloc::boxed::Box;

use crate::spsc::SpscRing;

/// 类型擦除的析构描述符。
///
/// 不变式 #10：`layout` 在编译期由 `DropVTable::of::<T>()` 固定，
/// GC 线程凭此安全释放，杜绝 Layout 不一致导致的堆损坏。
#[derive(Debug, Clone, Copy)]
pub struct DropVTable {
    pub drop_in_place: unsafe fn(*mut ()),
    pub layout: Layout,
}

impl DropVTable {
    /// 为具体类型 `T` 生成 vtable（单态化，零运行时开销）。
    pub fn of<T>() -> Self {
        unsafe fn drop_impl<T>(p: *mut ()) {
            // SAFETY: 由 drop_and_dealloc 的契约保证 p 是有效的 *mut T
            unsafe { ptr::drop_in_place(p.cast::<T>()) };
        }
        Self {
            drop_in_place: drop_impl::<T> as unsafe fn(*mut ()),
            layout: Layout::new::<T>(),
        }
    }
}

/// 类型擦除的待回收条目（(指针, vtable) 对）。
#[derive(Debug, Clone, Copy)]
pub struct DropEntry {
    pub ptr: *mut (),
    pub vtable: DropVTable,
}

// SAFETY: DropEntry 内的裸指针来自 Box::into_raw，
// 其所有权已经过 Send 边界审查（不变式 #4）。
unsafe impl Send for DropEntry {}

/// GC 线程上下文：执行 Physical Drop。
pub struct GcContext {
    reclaimed: u64,
}

impl GcContext {
    pub const fn new() -> Self {
        Self { reclaimed: 0 }
    }

    /// 将 Box 转换为可推入 Deferred Drop Queue 的 (指针, vtable) 对。
    ///
    /// Provenance 保护（不变式 #9）：返回的指针来自 `Box::into_raw`，
    /// 在回收前**禁止任何偏移运算**。
    pub fn defer_box<T>(value: Box<T>) -> (*mut (), DropVTable) {
        (Box::into_raw(value).cast::<()>(), DropVTable::of::<T>())
    }

    /// 将 `Box::into_raw` 产生的指针包装为 `DropEntry`。
    pub fn defer_box_entry<T>(value: Box<T>) -> DropEntry {
        let (ptr, vtable) = Self::defer_box(value);
        DropEntry { ptr, vtable }
    }

    /// 执行 Physical Drop（§2.2.1）。
    ///
    /// # Safety
    /// - `ptr` 必须是通过 `Box::into_raw` 产生的裸指针
    /// - `vtable.layout` 必须与分配时的 `Layout` 严格一致
    /// - `ptr` 必须未被释放过
    pub unsafe fn drop_and_dealloc(&mut self, ptr: *mut (), vtable: DropVTable) {
        debug_assert!(!ptr.is_null());
        // 稳定版对齐检查（避开 unstable 的 `pointer::is_aligned_to`）
        debug_assert!((ptr as usize) % vtable.layout.align() == 0);
        // SAFETY: 契约由调用者保证
        unsafe {
            (vtable.drop_in_place)(ptr);
            if vtable.layout.size() != 0 {
                dealloc(ptr.cast::<u8>(), vtable.layout);
            }
        }
        self.reclaimed += 1;
    }

    /// 从 `DropEntry` 执行 Physical Drop。
    ///
    /// # Safety
    /// 同 [`Self::drop_and_dealloc`]。
    pub unsafe fn reclaim(&mut self, entry: DropEntry) {
        // SAFETY: 契约由调用者传递
        unsafe { self.drop_and_dealloc(entry.ptr, entry.vtable) };
    }

    pub fn reclaimed(&self) -> u64 {
        self.reclaimed
    }
}

impl Default for GcContext {
    fn default() -> Self {
        Self::new()
    }
}

/// 基于 SPSC 的类型擦除回收队列（架构文档 §2.2 / §2.3.2 回收协议）。
///
/// UI 线程通过 `push` 提交待回收指针；GC 线程通过 `drain` 消费。
///
/// 并发契约：`push` 仅 UI 线程调用（单生产者）；
/// `drain` 仅 GC 线程调用（单消费者）。
pub struct DeferredDropQueue<const N: usize> {
    ring: SpscRing<DropEntry, N>,
    overflow_counter: AtomicU32,
}

impl<const N: usize> DeferredDropQueue<N> {
    pub fn new() -> Self {
        Self {
            ring: SpscRing::new(),
            overflow_counter: AtomicU32::new(0),
        }
    }

    /// UI 线程提交待回收条目。队列满时返回 `false`
    /// （调用者应走 Emergency Slot 兜底）。
    pub fn push(&self, entry: DropEntry) -> bool {
        // SAFETY: 单生产者契约
        let mut producer = unsafe { self.ring.producer() };
        if producer.push(entry).is_ok() {
            true
        } else {
            self.overflow_counter.fetch_add(1, Ordering::Relaxed);
            false
        }
    }

    /// GC 线程消费并释放所有待回收条目。
    ///
    /// # Safety
    /// - 所有条目的 ptr/vtable 必须合法（由 push 端契约保证）
    /// - 单消费者契约
    pub unsafe fn drain(&self, gc: &mut GcContext) -> u32 {
        let mut consumer = unsafe { self.ring.consumer() };
        let mut count = 0u32;
        while let Some(entry) = consumer.pop() {
            // SAFETY: 条目由 defer_box / defer_box_entry 产生
            unsafe { gc.reclaim(entry) };
            count += 1;
        }
        count
    }

    pub fn len(&self) -> usize {
        self.ring.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ring.is_empty()
    }

    pub fn overflow_count(&self) -> u32 {
        self.overflow_counter.load(Ordering::Relaxed)
    }
}

impl<const N: usize> Default for DeferredDropQueue<N> {
    fn default() -> Self {
        Self::new()
    }
}

/// 紧急回收槽位（§2.2.2）。GC LIFO 满时的最后兜底。
///
/// 缓存行对齐避免与相邻槽位 false sharing。
/// 超时字段已移除 —— 由 Watchdog 线程统一管理（时钟源见 §2.2.2）。
///
/// 目标时延：push < 15ns（§4.6）。
#[repr(align(128))]
pub struct EmergencySlot {
    ptr: AtomicPtr<()>,
    vtable: UnsafeCell<MaybeUninit<DropVTable>>,
    vtable_ready: AtomicBool,
}

// SAFETY: vtable 字段的写/读被 vtable_ready + ptr 的 Release/Acquire
// 协议串行化：写者先写 vtable 再发布 ptr；读者先读 ptr 再消费 vtable。
unsafe impl Sync for EmergencySlot {}
unsafe impl Send for EmergencySlot {}

impl EmergencySlot {
    pub const fn new() -> Self {
        Self {
            ptr: AtomicPtr::new(ptr::null_mut()),
            vtable: UnsafeCell::new(MaybeUninit::uninit()),
            vtable_ready: AtomicBool::new(false),
        }
    }

    /// 槽位是否空闲。
    #[inline(always)]
    pub fn is_vacant(&self) -> bool {
        self.ptr.load(Ordering::Acquire).is_null()
    }

    /// 音频线程侧：尝试占用槽位（失败即允许泄漏，不变式 #7）。
    ///
    /// # Safety
    /// - `obj` 必须来自 `Box::into_raw` 且与 `vtable` 匹配
    /// - 单写者契约：同一槽位同一时刻至多一个线程 push
    pub unsafe fn push(&self, obj: *mut (), vtable: DropVTable) -> bool {
        if !self.is_vacant() {
            return false;
        }
        // 协议顺序：vtable 载荷 → Release fence → ready 标志 → ptr 发布
        // SAFETY: 槽位空闲时 vtable 字段对写者独占
        unsafe { (*self.vtable.get()).write(vtable) };
        fence(Ordering::Release);
        self.vtable_ready.store(true, Ordering::Release);
        self.ptr.store(obj, Ordering::Release);
        true
    }

    /// GC / Watchdog 线程侧：排空槽位。
    ///
    /// # Safety
    /// 单读者契约：同一槽位同一时刻至多一个线程 drain。
    pub unsafe fn drain(&self) -> Option<(*mut (), DropVTable)> {
        let p = self.ptr.load(Ordering::Acquire);
        if p.is_null() {
            return None;
        }
        if !self.vtable_ready.load(Ordering::Acquire) {
            return None; // 写者尚未完成发布，下轮扫描再来
        }
        // SAFETY: ptr 非空 + ready 置位 ⟹ vtable 已完整写入且对本读者可见
        let vtable = unsafe { (*self.vtable.get()).assume_init_read() };
        self.vtable_ready.store(false, Ordering::Release);
        self.ptr.store(ptr::null_mut(), Ordering::Release);
        Some((p, vtable))
    }

    /// 便捷接口：drain + 立即释放。
    ///
    /// # Safety
    /// 同 [`Self::drain`] + [`GcContext::drop_and_dealloc`]。
    pub unsafe fn drain_and_reclaim(&self, gc: &mut GcContext) -> bool {
        // SAFETY: 契约由调用者传递
        if let Some((ptr, vtable)) = unsafe { self.drain() } {
            unsafe { gc.drop_and_dealloc(ptr, vtable) };
            true
        } else {
            false
        }
    }
}

impl Default for EmergencySlot {
    fn default() -> Self {
        Self::new()
    }
}

/// EmergencySlot 数组：音频线程的无锁 LIFO 兜底池。
///
/// 音频线程按顺序扫描空闲槽位写入；Watchdog/GC 线程反向扫描排空。
/// 极端异常下允许少量泄漏（不变式 #7）。
pub struct EmergencyPool<const SLOTS: usize> {
    slots: [EmergencySlot; SLOTS],
}

impl<const SLOTS: usize> EmergencyPool<SLOTS> {
    pub fn new() -> Self {
        Self {
            slots: core::array::from_fn(|_| EmergencySlot::new()),
        }
    }

    /// 音频线程侧：找到第一个空闲槽位并写入。O(SLOTS) 最坏。
    ///
    /// # Safety
    /// 同 [`EmergencySlot::push`]。
    pub unsafe fn push(&self, ptr: *mut (), vtable: DropVTable) -> Option<u8> {
        for (i, slot) in self.slots.iter().enumerate() {
            // SAFETY: 单写者契约由音频线程独占保证
            if unsafe { slot.push(ptr, vtable) } {
                return Some(i as u8);
            }
        }
        None // 所有槽位占满 → 允许泄漏
    }

    /// GC/Watchdog 线程侧：drain 所有占用的槽位。
    ///
    /// # Safety
    /// 同 [`EmergencySlot::drain_and_reclaim`]。
    pub unsafe fn drain_all(&self, gc: &mut GcContext) -> u32 {
        let mut count = 0u32;
        for slot in &self.slots {
            // SAFETY: 单读者契约由 GC/Watchdog 线程独占保证
            if unsafe { slot.drain_and_reclaim(gc) } {
                count += 1;
            }
        }
        count
    }

    /// 非占用槽位数量。
    pub fn vacant_count(&self) -> usize {
        self.slots.iter().filter(|s| s.is_vacant()).count()
    }

    pub fn total_slots(&self) -> usize {
        SLOTS
    }
}

impl<const SLOTS: usize> Default for EmergencyPool<SLOTS> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::boxed::Box;
    use core::sync::atomic::AtomicU32;

    static DROPPED: AtomicU32 = AtomicU32::new(0);

    #[allow(dead_code)]
    struct Tracked(u32);
    impl Drop for Tracked {
        fn drop(&mut self) {
            DROPPED.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn deferred_drop_roundtrip() {
        let before = DROPPED.load(Ordering::SeqCst);
        let (ptr, vtable) = GcContext::defer_box(Box::new(Tracked(1)));
        let mut gc = GcContext::new();
        // SAFETY: ptr/vtable 刚由 defer_box 产生
        unsafe { gc.drop_and_dealloc(ptr, vtable) };
        assert_eq!(DROPPED.load(Ordering::SeqCst), before + 1);
        assert_eq!(gc.reclaimed(), 1);
    }

    #[test]
    fn emergency_slot_push_drain() {
        let slot = EmergencySlot::new();
        assert!(slot.is_vacant());

        let (ptr, vtable) = GcContext::defer_box(Box::new(42u64));
        // SAFETY: 单线程测试满足单写者/单读者契约
        unsafe {
            assert!(slot.push(ptr, vtable));
            assert!(!slot.is_vacant());
            // 占用中的槽位拒绝二次 push
            let (p2, vt2) = GcContext::defer_box(Box::new(7u64));
            assert!(!slot.push(p2, vt2));
            let mut gc = GcContext::new();
            gc.drop_and_dealloc(p2, vt2);

            let (drained, dvt) = slot.drain().expect("槽位应有内容");
            assert_eq!(drained, ptr);
            gc.drop_and_dealloc(drained, dvt);
            assert!(slot.is_vacant());
            assert!(slot.drain().is_none());
        }
    }

    #[test]
    fn deferred_drop_queue_roundtrip() {
        let before = DROPPED.load(Ordering::SeqCst);
        let queue = DeferredDropQueue::<32>::new();

        for i in 0..8u32 {
            let entry = GcContext::defer_box_entry(Box::new(Tracked(i)));
            assert!(queue.push(entry));
        }
        assert_eq!(queue.len(), 8);

        let mut gc = GcContext::new();
        // SAFETY: 单消费者契约（测试线程独占）
        let count = unsafe { queue.drain(&mut gc) };
        assert_eq!(count, 8);
        assert_eq!(gc.reclaimed(), 8);
        assert_eq!(DROPPED.load(Ordering::SeqCst), before + 8);
        assert!(queue.is_empty());
    }

    #[test]
    fn deferred_drop_queue_overflow() {
        let queue = DeferredDropQueue::<4>::new();
        for _ in 0..4 {
            let entry = GcContext::defer_box_entry(Box::new(0u8));
            assert!(queue.push(entry));
        }
        // 第 5 个推不进去
        let overflow_entry = GcContext::defer_box_entry(Box::new(0u8));
        assert!(!queue.push(overflow_entry));
        assert_eq!(queue.overflow_count(), 1);

        // 手动回收溢出的条目（避免泄漏）
        let mut gc = GcContext::new();
        unsafe { gc.reclaim(overflow_entry) };
        unsafe { queue.drain(&mut gc) };
    }

    #[test]
    fn emergency_pool_push_drain() {
        let before = DROPPED.load(Ordering::SeqCst);
        let pool = EmergencyPool::<4>::new();
        assert_eq!(pool.vacant_count(), 4);

        // push 3 个条目
        for i in 0..3u32 {
            let (ptr, vtable) = GcContext::defer_box(Box::new(Tracked(i)));
            unsafe { pool.push(ptr, vtable).expect("应有空闲槽位") };
        }
        assert_eq!(pool.vacant_count(), 1);

        // drain 全部
        let mut gc = GcContext::new();
        let count = unsafe { pool.drain_all(&mut gc) };
        assert_eq!(count, 3);
        assert_eq!(gc.reclaimed(), 3);
        assert_eq!(DROPPED.load(Ordering::SeqCst), before + 3);
        assert_eq!(pool.vacant_count(), 4);
    }
}
