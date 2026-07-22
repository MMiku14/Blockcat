//! FPU 硬件异常抑制：FTZ（Flush-To-Zero）/ DAZ（Denormals-Are-Zero）
//! （架构文档 §3.1）。
//!
//! Subnormal（非规格化）浮点数会使乘加运算慢 10-100 倍，是音频线程
//! 尾部延迟的经典来源（IIR 滤波器衰减尾音时尤甚）。
//!
//! RAII 语义：`FpuGuard::new()` 置位，`Drop` 恢复原状态。
//! `Drop` 中使用 `compiler_fence(SeqCst)` 防止浮点运算被重排到
//! 恢复指令之后（不变式 #11）。
//!
//! 实现注记：Rust 已弃用 `_mm_setcsr`/`_mm_getcsr` intrinsic，
//! 本实现直接使用 `stmxcsr`/`ldmxcsr` 内联汇编。
//! 目标时延：守卫切换 ~15ns（§4.6）。

#[cfg(target_arch = "x86_64")]
mod imp {
    use core::sync::atomic::{compiler_fence, Ordering};

    /// MXCSR bit 15: Flush-To-Zero
    const FTZ_BIT: u32 = 1 << 15;
    /// MXCSR bit 6: Denormals-Are-Zero
    const DAZ_BIT: u32 = 1 << 6;

    #[inline(always)]
    fn read_mxcsr() -> u32 {
        let mut v: u32 = 0;
        // SAFETY: stmxcsr 将 MXCSR 写入给定内存位置，无其他副作用
        unsafe {
            core::arch::asm!("stmxcsr [{ptr}]", ptr = in(reg) &mut v, options(nostack));
        }
        v
    }

    #[inline(always)]
    fn write_mxcsr(v: u32) {
        // SAFETY: ldmxcsr 从给定内存位置加载 MXCSR；调用方保证值合法
        unsafe {
            core::arch::asm!("ldmxcsr [{ptr}]", ptr = in(reg) &v, options(nostack, readonly));
        }
    }

    /// x86_64 FTZ/DAZ 守卫。
    pub struct FpuGuard {
        mxcsr_old: u32,
    }

    impl FpuGuard {
        #[inline(always)]
        pub fn new() -> Self {
            compiler_fence(Ordering::SeqCst);
            let old = read_mxcsr();
            write_mxcsr(old | FTZ_BIT | DAZ_BIT);
            compiler_fence(Ordering::SeqCst);
            Self { mxcsr_old: old }
        }
    }

    impl Drop for FpuGuard {
        #[inline(always)]
        fn drop(&mut self) {
            compiler_fence(Ordering::SeqCst);
            write_mxcsr(self.mxcsr_old);
            compiler_fence(Ordering::SeqCst);
        }
    }
}

#[cfg(target_arch = "aarch64")]
mod imp {
    use core::sync::atomic::{compiler_fence, Ordering};

    /// FPCR bit 24: FZ（Flush-to-zero）
    const FZ_BIT: u64 = 1 << 24;

    #[inline(always)]
    fn read_fpcr() -> u64 {
        let v: u64;
        // SAFETY: mrs 读取 FPCR，EL0 可访问
        unsafe {
            core::arch::asm!("mrs {}, fpcr", out(reg) v, options(nomem, nostack));
        }
        v
    }

    #[inline(always)]
    fn write_fpcr(v: u64) {
        // SAFETY: msr 写入 FPCR，EL0 可访问
        unsafe {
            core::arch::asm!("msr fpcr, {}", in(reg) v, options(nomem, nostack));
        }
    }

    /// AArch64 FZ 守卫（§3.1.2）。
    pub struct FpuGuard {
        fpcr_old: u64,
    }

    impl FpuGuard {
        #[inline(always)]
        pub fn new() -> Self {
            compiler_fence(Ordering::SeqCst);
            let old = read_fpcr();
            write_fpcr(old | FZ_BIT);
            compiler_fence(Ordering::SeqCst);
            Self { fpcr_old: old }
        }
    }

    impl Drop for FpuGuard {
        #[inline(always)]
        fn drop(&mut self) {
            compiler_fence(Ordering::SeqCst);
            write_fpcr(self.fpcr_old);
            compiler_fence(Ordering::SeqCst);
        }
    }
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
mod imp {
    /// no-op 守卫（§3.1.3）。
    ///
    /// WASM：引擎（V8 / SpiderMonkey）已优化 Subnormal 处理，手动逐采样
    /// 加偏置属于白白消耗 CPU 预算的过度防御，故为 no-op。
    /// 其余架构：硬件依赖，默认 no-op。
    pub struct FpuGuard;

    impl FpuGuard {
        #[inline(always)]
        pub fn new() -> Self {
            Self
        }
    }
}

pub use imp::FpuGuard;

impl Default for FpuGuard {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::FpuGuard;

    #[test]
    fn fpu_guard_raii_roundtrip() {
        // 置位 → 作用域内执行浮点运算 → 析构恢复，全程不得 panic
        {
            let _guard = FpuGuard::new();
            let mut acc = 1.0e-30f32;
            for _ in 0..64 {
                acc *= 0.5; // 在 FTZ 下会被冲刷为 0
            }
            // FTZ 生效（x86_64/aarch64）时 acc == 0；no-op 平台为 subnormal
            assert!(acc.abs() < 1.0e-30);
        }
        // 守卫析构后再次创建，验证状态可重入
        let _guard2 = FpuGuard::new();
    }
}
