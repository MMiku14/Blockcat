//! CoreAudio 后端（macOS，架构文档 §3.2.4）—— 参考骨架。
//!
//! 生产实现要点：
//! - CoreAudio IOProc 自身即系统实时线程，无需自建线程；仍应通过
//!   `thread_policy_set(THREAD_TIME_CONSTRAINT_POLICY)` 声明时间约束
//!   （§3.3.4），并按附录 C.2 防止被调度到 E-Core
//! - 块大小固定（不支持动态变化）
//! - 本骨架不链接 CoreAudio framework，仅用于编译期验证模块拓扑
use crate::{HostAdapter, HostConfig, HostError, HostHandle};

pub struct CoreAudioAdapter {
    handle: Option<HostHandle>,
}

impl CoreAudioAdapter {
    pub fn new() -> Self {
        Self { handle: None }
    }
}

impl Default for CoreAudioAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl HostAdapter for CoreAudioAdapter {
    fn initialize(&mut self, config: HostConfig) -> Result<HostHandle, HostError> {
        // 生产实现：AudioUnit (kAudioUnitSubType_HALOutput) 协商格式
        let handle = HostHandle {
            sample_rate: config.preferred_sample_rate,
            block_size: config.preferred_block_size,
        };
        self.handle = Some(handle);
        Ok(handle)
    }

    fn start(&mut self) -> Result<(), HostError> {
        Err(HostError::Backend(
            "参考骨架未链接 CoreAudio；生产实现见 §3.2.4",
        ))
    }

    fn stop(&mut self) -> Result<(), HostError> {
        Err(HostError::NotRunning)
    }

    fn current_block_size(&self) -> usize {
        self.handle.map(|h| h.block_size).unwrap_or(0)
    }

    fn current_sample_rate(&self) -> u32 {
        self.handle.map(|h| h.sample_rate).unwrap_or(0)
    }

    fn supports_dynamic_block_size(&self) -> bool {
        false // CoreAudio 块大小固定（§3.2.4）
    }
}
