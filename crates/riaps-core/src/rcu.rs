//! RCU（Read-Copy-Update）原子指针交换（架构文档 §2.3 / §2.4）。
//!
//! 读者（音频线程）无阻塞 `Acquire` 读取；写者（UI 线程）复制修改后
//! 原子交换指针，旧数据推入 Deferred Drop Queue 延迟释放。
//!
//! 目标时延：RCU swap ~5ns（§4.6）。
//!
//! 注：32-bit 平台的 `AtomicU64` 应替换为 `portable-atomic::AtomicU64`
//! （不变式 #12）。参考实现为零依赖，直接使用 core 原子类型。

use core::ptr;
use core::sync::atomic::{AtomicPtr, AtomicU64, Ordering};

/// 通用 RCU 句柄。`DspGraphHandle`（§2.3.2）与 `SampleBankHandle`
/// （§2.4.2）均为本类型的领域特化。
pub struct RcuHandle<T> {
    current: AtomicPtr<T>,
    version: AtomicU64,
}

// SAFETY: 指针本身的交换是原子的；被指向数据的跨线程访问安全性
// 由 swap 的 Safety 契约（旧指针立即推入 Deferred Drop）保证。
unsafe impl<T: Send + Sync> Send for RcuHandle<T> {}
unsafe impl<T: Send + Sync> Sync for RcuHandle<T> {}

impl<T> RcuHandle<T> {
    /// 空句柄（尚未发布任何数据）。
    pub const fn empty() -> Self {
        Self {
            current: AtomicPtr::new(ptr::null_mut()),
            version: AtomicU64::new(0),
        }
    }

    /// 以初始指针构造。`initial` 应通过 `Arc::into_raw` / `Box::into_raw`
    /// 产生（Provenance 保护，不变式 #9）。
    pub fn new(initial: *mut T) -> Self {
        Self {
            current: AtomicPtr::new(initial),
            version: AtomicU64::new(1),
        }
    }

    /// 音频线程热路径读取。永不阻塞。
    #[inline(always)]
    pub fn load(&self) -> *const T {
        self.current.load(Ordering::Acquire)
    }

    /// 单调递增版本号（每次 swap +1）。
    #[inline(always)]
    pub fn version(&self) -> u64 {
        self.version.load(Ordering::Acquire)
    }

    /// 原子交换当前指针，返回旧指针。
    ///
    /// # Safety
    /// - 必须在**非音频线程**调用（§2.3.2 契约）
    /// - `new_ptr` 必须非 null，且通过 `Arc::into_raw` / `Box::into_raw` 产生
    /// - 调用者必须在返回后**立即**将旧指针推入 Deferred Drop Queue
    /// - 禁止在旧指针被回收前再次调用 swap
    pub unsafe fn swap(&self, new_ptr: *mut T) -> *mut T {
        debug_assert!(!new_ptr.is_null());
        let old = self.current.swap(new_ptr, Ordering::AcqRel);
        self.version.fetch_add(1, Ordering::Release);
        old
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::boxed::Box;

    #[test]
    fn rcu_swap_returns_old_and_bumps_version() {
        let a = Box::into_raw(Box::new(1u32));
        let b = Box::into_raw(Box::new(2u32));
        let handle = RcuHandle::new(a);
        assert_eq!(handle.version(), 1);
        assert_eq!(handle.load(), a as *const u32);

        // SAFETY: 测试线程即"非音频线程"；旧指针随即手动回收
        let old = unsafe { handle.swap(b) };
        assert_eq!(old, a);
        assert_eq!(handle.version(), 2);
        assert_eq!(handle.load(), b as *const u32);

        // 手动回收（生产路径应走 Deferred Drop Queue）
        unsafe {
            drop(Box::from_raw(old));
            drop(Box::from_raw(handle.swap(Box::into_raw(Box::new(0u32)))));
            drop(Box::from_raw(handle.current.load(Ordering::Acquire)));
        }
    }
}
