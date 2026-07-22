//! # riaps-sys — 系统交互抽象层
//!
//! 封装所有需要 OS 特权或硬件寄存器访问的"平台脏活"（架构文档 §1.6）：
//!
//! - [`fpu`]    FTZ/DAZ 硬件异常抑制（§3.1）
//! - [`thread`] Thread Provisioner 实时特权降级策略（§3.3）
//! - [`probe`]  rdtsc / CNTVCT 无锁性能探针（§4.3.1）
//!
//! 依赖规则：仅依赖 `riaps-core`，禁止反向依赖 dsp/host 层。

#![deny(unsafe_op_in_unsafe_fn)]

pub mod fpu;
pub mod probe;
pub mod thread;

pub use fpu::FpuGuard;
pub use probe::{read_monotonic_tick, PerformanceProbe, ProbeRecord};
pub use thread::{
    ProvisionError, ProvisionStrategy, StrictAction, ThreadProvisioner, ThreadProvisioning,
};
