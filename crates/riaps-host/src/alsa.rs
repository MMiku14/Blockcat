//! ALSA 后端（Linux，架构文档 §3.2.4）—— 参考骨架。
//!
//! 生产实现要点：
//! - 自建 Poll 线程（`snd_pcm_wait`），线程需经 `riaps-sys::ThreadProvisioner`
//!   申请 SCHED_FIFO（Direct → rtkit → Portal → nice 降级链，§3.3.3）
//! - 支持动态块大小：period size 变化时经 `AdapterBuffer` 适配并发射
//!   `HostAdapterBlockSizeChanged` 事件
//! - 本骨架不链接 libasound，`start` 返回 `Unsupported`，仅用于
//!   编译期验证模块拓扑
use crate::{HostAdapter, HostConfig, HostError, HostHandle};

pub struct AlsaAdapter {
    handle: Option<HostHandle>,
}

impl AlsaAdapter {
    pub fn new() -> Self {
        Self { handle: None }
    }
}

impl Default for AlsaAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl HostAdapter for AlsaAdapter {
    fn initialize(&mut self, config: HostConfig) -> Result<HostHandle, HostError> {
        // 生产实现：snd_pcm_open + hw_params 协商采样率/period
        let handle = HostHandle {
            sample_rate: config.preferred_sample_rate,
            block_size: config.preferred_block_size,
        };
        self.handle = Some(handle);
        Ok(handle)
    }

    fn start(&mut self) -> Result<(), HostError> {
        Err(HostError::Backend(
            "参考骨架未链接 libasound；生产实现见 §3.2.4",
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
        true // ALSA period 可动态变化（§3.2.4）
    }
}
