//! # riaps-host — 宿主适配器矩阵（架构文档 §3.2）
//!
//! 平台矩阵（§3.2.4）：
//!
//! | 平台    | 后端          | 线程模型       | 块大小变化 |
//! |---------|---------------|----------------|-----------|
//! | macOS   | CoreAudio     | 系统实时线程    | 不支持     |
//! | Linux   | ALSA          | 自建 Poll 线程  | 支持       |
//! | Windows | WASAPI/ASIO   | COM/驱动回调    | 支持/不支持 |
//! | WASM    | AudioWorklet  | 固定 128       | 不支持     |
//!
//! 依赖规则：可依赖 core/sys，禁止依赖 riaps-dsp（音频核心路径可移植性）。

#![deny(unsafe_op_in_unsafe_fn)]

pub mod adapter_buf;

#[cfg(target_os = "linux")]
pub mod alsa;
#[cfg(target_os = "macos")]
pub mod coreaudio;

pub use adapter_buf::AdapterBuffer;

/// 宿主初始化配置（§3.2.2）。
#[derive(Debug, Clone)]
pub struct HostConfig {
    pub preferred_sample_rate: u32,
    pub preferred_block_size: usize,
    pub min_block_size: usize,
    pub max_block_size: usize,
    pub num_input_channels: u16,
    pub num_output_channels: u16,
}

impl Default for HostConfig {
    fn default() -> Self {
        Self {
            preferred_sample_rate: 96_000,
            preferred_block_size: 512,
            min_block_size: 64,
            max_block_size: 4096,
            num_input_channels: 2,
            num_output_channels: 2,
        }
    }
}

#[derive(Debug)]
pub enum HostError {
    DeviceUnavailable,
    Unsupported,
    AlreadyRunning,
    NotRunning,
    Backend(&'static str),
}

/// 宿主句柄：实际协商到的运行参数。
#[derive(Debug, Clone, Copy)]
pub struct HostHandle {
    pub sample_rate: u32,
    pub block_size: usize,
}

/// 跨平台音频后端桥接 trait（§3.2.2）。
pub trait HostAdapter: Send + 'static {
    fn initialize(&mut self, config: HostConfig) -> Result<HostHandle, HostError>;
    fn start(&mut self) -> Result<(), HostError>;
    fn stop(&mut self) -> Result<(), HostError>;
    fn current_block_size(&self) -> usize;
    fn current_sample_rate(&self) -> u32;
    fn supports_dynamic_block_size(&self) -> bool;
}

/// 空适配器：离线渲染 / 测试 / CI 环境使用。
pub struct NullAdapter {
    handle: Option<HostHandle>,
    running: bool,
}

impl NullAdapter {
    pub fn new() -> Self {
        Self {
            handle: None,
            running: false,
        }
    }
}

impl Default for NullAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl HostAdapter for NullAdapter {
    fn initialize(&mut self, config: HostConfig) -> Result<HostHandle, HostError> {
        let handle = HostHandle {
            sample_rate: config.preferred_sample_rate,
            block_size: config.preferred_block_size,
        };
        self.handle = Some(handle);
        Ok(handle)
    }

    fn start(&mut self) -> Result<(), HostError> {
        if self.handle.is_none() {
            return Err(HostError::NotRunning);
        }
        if self.running {
            return Err(HostError::AlreadyRunning);
        }
        self.running = true;
        Ok(())
    }

    fn stop(&mut self) -> Result<(), HostError> {
        if !self.running {
            return Err(HostError::NotRunning);
        }
        self.running = false;
        Ok(())
    }

    fn current_block_size(&self) -> usize {
        self.handle.map(|h| h.block_size).unwrap_or(0)
    }

    fn current_sample_rate(&self) -> u32 {
        self.handle.map(|h| h.sample_rate).unwrap_or(0)
    }

    fn supports_dynamic_block_size(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_adapter_lifecycle() {
        let mut a = NullAdapter::new();
        let h = a.initialize(HostConfig::default()).unwrap();
        assert_eq!(h.sample_rate, 96_000);
        assert_eq!(h.block_size, 512);
        a.start().unwrap();
        assert!(matches!(a.start(), Err(HostError::AlreadyRunning)));
        a.stop().unwrap();
        assert!(matches!(a.stop(), Err(HostError::NotRunning)));
    }
}
