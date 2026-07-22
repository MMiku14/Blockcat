//! # riaps-core — 核心无锁原语库
//!
//! `no_std` 兼容（需要 `alloc`，用于 Deferred Drop 的物理释放路径）。
//!
//! 本 crate 保持最高纯洁度：不接触任何 OS 特定 API（架构文档 §1.6）。
//!
//! 模块划分：
//! - [`spsc`]   缓存行对齐的 SPSC 调度器（§2.1）
//! - [`rcu`]    读-拷贝-更新指针交换（§2.3 / §2.4）
//! - [`memory`] Provenance 保护与 Deferred Drop / Emergency Slots（§2.2）

#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

extern crate alloc;

#[cfg(test)]
extern crate std;

pub mod memory;
pub mod rcu;
pub mod spsc;

pub use memory::{DropVTable, EmergencySlot, GcContext};
pub use rcu::RcuHandle;
pub use spsc::{CachePadded, Consumer, Producer, SpscError, SpscRing};
