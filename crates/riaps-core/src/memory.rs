//! 异步无损回收系统：Provenance 保护、DropVTable、Emergency Slots
//! （架构文档 §2.2）。
//!
//! - **Logical Drop**：音频线程将所有权转移至 Deferred Drop Queue（O(1) 无界等待）
//! - **Physical Drop**：GC 线程执行 `drop_in_place` + `dealloc`
//! - **Emergency Slots**：GC 队列满时的崩溃兜底；Watchdog 线程负责超时扫描
//!   （音频线程不维护超时状态机，§2.2.2）

use core::alloc::Layout;
use core::cell::UnsafeCell;
use core::mem::MaybeUninit;
use core::ptr;
use core::sync::atomic::{fence, AtomicBool, AtomicPtr, Ordering};

use alloc::alloc::dealloc;
use alloc::boxed::Box;

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
    #[inline(always)]
    pub fn of<T>() -> Self {
        unsafe fn drop_impl<T>(p: *mut ()) {
            // SAFETY: 由 drop_and_dealloc 的契约保证 p 是有效的 *mut T
            unsafe { ptr::drop_in_place(p.cast::<T>()) };
        }
        Self {
            drop_in_place: drop_impl::<T>,
            layout: Layout::new::<T>(),
        }
    }
}

/// GC 线程上下文：执行 Physical Drop。
pub struct GcContext {
    reclaimed: u64,
}

impl GcContext {
    #[inline(always)]
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

    #[inline(always)]
    pub fn reclaimed(&self) -> u64 {
        self.reclaimed
    }
}

impl Default for GcContext {
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
}

impl Default for EmergencySlot {
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

    // 单元结构体，无字段，避免 dead_code 警告
    struct Tracked;
    impl Drop for Tracked {
        fn drop(&mut self) {
            DROPPED.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn deferred_drop_roundtrip() {
        let before = DROPPED.load(Ordering::SeqCst);
        let (ptr, vtable) = GcContext::defer_box(Box::new(Tracked));
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
}
