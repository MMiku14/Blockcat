//! Thread Provisioner：跨平台实时特权管理与优雅降级（架构文档 §3.3）。
//!
//! 降级链：
//! - Linux:   `sched_setscheduler(SCHED_FIFO)` → rtkit(D-Bus) → portal → nice(-10)
//! - macOS:   `thread_policy_set(TIME_CONSTRAINT)` → QoS USER_INTERACTIVE
//! - Windows: MMCSS "Pro Audio" + TIME_CRITICAL → HIGH_PRIORITY_CLASS
//!
//! 参考实现说明：为保持零外部依赖，rtkit（需 `zbus`）、MMCSS（需
//! `windows` crate）路径以文档化占位实现，直接调用降级链下一级。
//! 现场级（Live FOH）场景必须使用 [`ThreadProvisioning::Strict`]，
//! 拿不到硬实时特权即返回错误，拒绝静默降级。

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProvisionStrategy {
    Unprovisioned,
    /// rlimits / root 直接授予
    Direct,
    /// Linux D-Bus rtkit（参考实现未接入，占位）
    Rtkit,
    /// xdg-desktop-portal（参考实现未接入，占位）
    Portal,
    /// macOS `thread_policy_set` / QoS
    MacOsPolicy,
    /// Windows MMCSS（参考实现未接入，占位）
    WindowsMmcss,
    /// SCHED_OTHER + nice -10 / 无任何保证
    Fallback,
}

/// 特权获取模式（§3.3.2）。
#[derive(Debug, Clone, Copy)]
pub enum ThreadProvisioning {
    /// 允许逐级降级到 Fallback（桌面应用默认）
    BestEffort,
    /// 拿不到硬实时特权则失败（现场级混音台必须启用）
    Strict(StrictAction),
}

#[derive(Debug, Clone, Copy)]
pub enum StrictAction {
    /// 立即返回致命错误（由上层决定 panic/abort）
    FatalError,
    /// 有限重试后失败
    RetryWithBackoff { max_ms: u64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProvisionError {
    /// EPERM：普通用户无 SCHED_FIFO 权限
    PermissionDenied,
    /// 当前平台无实时调度概念
    Unsupported,
    /// Strict 模式下拒绝降级
    StrictModeRefused,
}

/// 跨平台实时特权管理器。
///
/// 不变式 #19：所有平台资源句柄（如 MMCSS handle）必须在 `Drop` 中恢复；
/// 发生降级时必须通过 EventBus 发出 `ThreadProvisionerFallback` 事件
/// （事件发射由上层 kernel 编排，本类型仅暴露 `fallback_active()`）。
pub struct ThreadProvisioner {
    strategy: ProvisionStrategy,
    granted_prio: i32,
    fallback_active: bool,
    #[cfg(target_os = "linux")]
    original_linux_schedule: Option<linux_sched::Schedule>,
    // Realtime scheduling and MMCSS handles belong to the provisioning thread.
    _not_send: std::marker::PhantomData<std::rc::Rc<()>>,
}

impl ThreadProvisioner {
    pub fn new() -> Self {
        Self {
            strategy: ProvisionStrategy::Unprovisioned,
            granted_prio: 0,
            fallback_active: false,
            #[cfg(target_os = "linux")]
            original_linux_schedule: None,
            _not_send: std::marker::PhantomData,
        }
    }

    pub fn strategy(&self) -> ProvisionStrategy {
        self.strategy
    }

    pub fn granted_prio(&self) -> i32 {
        self.granted_prio
    }

    /// 是否发生了降级（上层据此发射 `ThreadProvisionerFallback` 事件）。
    pub fn fallback_active(&self) -> bool {
        self.fallback_active
    }

    /// 为**当前线程**申请实时调度特权。目标耗时 < 1ms（§4.6）。
    ///
    /// 返回实际授予的优先级（Fallback 时为 0）。
    pub fn provision(
        &mut self,
        target_prio: i32,
        mode: ThreadProvisioning,
    ) -> Result<i32, ProvisionError> {
        match self.try_direct(target_prio) {
            Ok((strategy, granted)) => {
                self.strategy = strategy;
                self.granted_prio = granted;
                self.fallback_active = granted < target_prio;
                Ok(granted)
            }
            Err(_) => match mode {
                ThreadProvisioning::Strict(_) => {
                    // 现场级：拒绝静默降级（§3.3.2）
                    Err(ProvisionError::StrictModeRefused)
                }
                ThreadProvisioning::BestEffort => {
                    self.strategy = ProvisionStrategy::Fallback;
                    self.granted_prio = 0;
                    self.fallback_active = true;
                    Ok(0)
                }
            },
        }
    }

    // ---- Linux：sched_setscheduler(SCHED_FIFO) 直接申请（§3.3.3）----
    #[cfg(target_os = "linux")]
    fn try_direct(&mut self, prio: i32) -> Result<(ProvisionStrategy, i32), ProvisionError> {
        let original = linux_sched::current().ok_or(ProvisionError::Unsupported)?;
        let param = linux_sched::SchedParam {
            sched_priority: prio.clamp(1, 99),
        };
        // SAFETY: 参数为有效栈上结构体，pid=0 表示当前线程组主调度实体
        let rc = unsafe { linux_sched::sched_setscheduler(0, linux_sched::SCHED_FIFO, &param) };
        if rc == 0 {
            self.original_linux_schedule = Some(original);
            Ok((ProvisionStrategy::Direct, param.sched_priority))
        } else {
            // 生产实现：此处应继续尝试 rtkit(zbus) → portal，见 §3.3.3
            Err(ProvisionError::PermissionDenied)
        }
    }

    // ---- macOS：QoS USER_INTERACTIVE（§3.3.4 / 附录 C.2）----
    // 生产实现应优先 thread_policy_set(THREAD_TIME_CONSTRAINT_POLICY)，
    // 并防止音频线程被调度到 E-Core。
    #[cfg(target_os = "macos")]
    fn try_direct(&mut self, prio: i32) -> Result<(ProvisionStrategy, i32), ProvisionError> {
        const QOS_CLASS_USER_INTERACTIVE: u32 = 0x21;
        extern "C" {
            fn pthread_set_qos_class_self_np(qos_class: u32, relative_priority: i32) -> i32;
        }
        // SAFETY: 系统调用参数为合法 QoS 常量
        let rc = unsafe { pthread_set_qos_class_self_np(QOS_CLASS_USER_INTERACTIVE, 0) };
        if rc == 0 {
            Ok((ProvisionStrategy::MacOsPolicy, prio))
        } else {
            Err(ProvisionError::PermissionDenied)
        }
    }

    // ---- 其余平台：参考实现不申请特权（Windows 生产实现走 MMCSS，§3.3.5）----
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    fn try_direct(&mut self, _prio: i32) -> Result<(ProvisionStrategy, i32), ProvisionError> {
        Err(ProvisionError::Unsupported)
    }
}

impl Default for ThreadProvisioner {
    fn default() -> Self {
        Self::new()
    }
}

// 不变式 #19：Drop 中恢复平台资源。
// 参考实现无持久句柄；Windows 生产实现须在此调用
// AvRevertMmThreadCharacteristics(mmcss_handle)。
impl Drop for ThreadProvisioner {
    fn drop(&mut self) {
        #[cfg(target_os = "linux")]
        if let Some(original) = self.original_linux_schedule.take() {
            // SAFETY: the provisioner is !Send and therefore drops on the same
            // thread whose policy was captured before SCHED_FIFO was applied.
            unsafe {
                linux_sched::sched_setscheduler(0, original.policy, &original.param);
            }
        }
    }
}

#[cfg(target_os = "linux")]
mod linux_sched {
    #[repr(C)]
    #[derive(Clone, Copy)]
    pub(super) struct SchedParam {
        pub(super) sched_priority: i32,
    }

    #[derive(Clone, Copy)]
    pub(super) struct Schedule {
        pub(super) policy: i32,
        pub(super) param: SchedParam,
    }

    pub(super) const SCHED_FIFO: i32 = 1;

    extern "C" {
        pub(super) fn sched_getscheduler(pid: i32) -> i32;
        pub(super) fn sched_getparam(pid: i32, param: *mut SchedParam) -> i32;
        pub(super) fn sched_setscheduler(
            pid: i32,
            policy: i32,
            param: *const SchedParam,
        ) -> i32;
    }

    pub(super) fn current() -> Option<Schedule> {
        // SAFETY: pid=0 selects the calling thread and `param` is valid output.
        unsafe {
            let policy = sched_getscheduler(0);
            if policy < 0 {
                return None;
            }
            let mut param = SchedParam { sched_priority: 0 };
            if sched_getparam(0, &mut param) != 0 {
                return None;
            }
            Some(Schedule { policy, param })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn best_effort_never_fails() {
        let mut p = ThreadProvisioner::new();
        // 无论是否有特权，BestEffort 都必须返回 Ok
        let granted = p
            .provision(95, ThreadProvisioning::BestEffort)
            .expect("BestEffort 不允许失败");
        assert!(granted >= 0);
        assert_ne!(p.strategy(), ProvisionStrategy::Unprovisioned);
    }

    #[test]
    fn strict_mode_refuses_silent_downgrade() {
        let mut p = ThreadProvisioner::new();
        let r = p.provision(95, ThreadProvisioning::Strict(StrictAction::FatalError));
        // CI 普通用户环境下应拒绝降级；若恰好有实时特权则成功亦合法
        if let Err(e) = r {
            assert_eq!(e, ProvisionError::StrictModeRefused);
        }
    }
}
