//! # riaps-dsp — 图拓扑与运行时（架构文档 §2.3 / §4.1）
//!
//! - [`node`]      `DspNode` trait + 无锁 `ParameterQueue`（§2.3.4）
//! - [`graph`]     动态图构建 / 拓扑验证 / RCU 热替换句柄（§2.3.2-2.3.3）
//! - [`event_bus`] 跨模块 SPSC 事件总线 + 保留槽位机制（§4.1）
//!
//! 依赖规则：可依赖 core/sys，**严禁**反向依赖 riaps-host。

#![deny(unsafe_op_in_unsafe_fn)]

pub mod event_bus;
pub mod graph;
pub mod node;

pub use event_bus::{EventBus, KernelEvent, StopReason};
pub use graph::{DspGraphHandle, DspGraphInner, DynamicGraphBuilder, GraphError, NodeId};
pub use node::{DspNode, GainNode, ParameterQueue, ProcessContext};
