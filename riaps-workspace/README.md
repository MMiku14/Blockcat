# RIAPS 实时音频微内核 — 参考实现 Workspace

对应架构文档 v4.8.1 §1.6「代码库拓扑与模块划分」的物理文件实现。

## 安装到 ~/src 并编译

```bash
# 1. 拷贝到 ~/src
mkdir -p ~/src
cp -r riaps-workspace ~/src/

# 2. 编译 + 测试（MSRV: Rust 1.82）
cd ~/src/riaps-workspace
cargo build --workspace
cargo test  --workspace

# 3. release 构建（panic = abort 生效，不变式 #11）
cargo build --workspace --release
```

## Crate 拓扑（单向无环依赖）

```text
riaps-core   [no_std + alloc]   SPSC / RCU / DropVTable / EmergencySlot
   ↑    ↑
riaps-sys                       FpuGuard(FTZ/DAZ) / ThreadProvisioner / rdtsc 探针
   ↑    ↑
   ├── riaps-host               HostAdapter trait / AdapterBuffer / ALSA·CoreAudio 骨架
   └── riaps-dsp                DspNode / DynamicGraphBuilder / DspGraphHandle(RCU) / EventBus
```

| Crate | 可依赖 | 不可依赖 |
|-------|--------|---------|
| `riaps-core` | 无（纯核心） | — |
| `riaps-sys`  | `riaps-core` | `riaps-dsp`, `riaps-host` |
| `riaps-host` | `riaps-core`, `riaps-sys` | `riaps-dsp` |
| `riaps-dsp`  | `riaps-core`, `riaps-sys` | `riaps-host` |

## 参考实现的工程取舍

为保证**零外部依赖离线编译**，以下路径以文档化占位实现，生产环境请按
架构文档替换：

- `CachePadded<T>` → 建议 `crossbeam_utils::CachePadded`（§2.1.2）
- 32-bit 平台 `AtomicU64` → `portable-atomic`（不变式 #12）
- rtkit / portal 降级链 → `zbus`（§3.3.3）；MMCSS → `windows` crate（§3.3.5）
- `AdapterBuffer` 内部 FIFO → 固定容量环形缓冲（当前为预分配 `VecDeque`）
- ALSA / CoreAudio 后端为编译期拓扑骨架，未链接系统库

已规避的 unstable / 已弃用 API：

- `MaybeUninit::array_assume_init` → 逐元素 `assume_init`（`probe::drain_batch`）
- `pointer::is_aligned_to` → 手动取模检查（`memory::drop_and_dealloc`）
- `_mm_setcsr` / `_mm_getcsr`（已弃用）→ `stmxcsr` / `ldmxcsr` 内联汇编（`fpu`）

## 测试覆盖

```text
riaps-core:  SPSC 环回/回绕/批量拷贝/容量、RCU swap 协议、
             Deferred Drop 往返、EmergencySlot push/drain
riaps-sys:   FpuGuard RAII、时钟单调性、探针记录/直通、
             Provisioner BestEffort/Strict 语义
riaps-host:  NullAdapter 生命周期、块大小失配流完整性（384↔512）
riaps-dsp:   参数队列只留最新值、增益节点、图编译/环检测/非法边、
             RCU 热替换、EventBus 保留槽位保护致命事件
```

形式化验证（Loom / Miri / Proptest）接入方式见架构文档 §4.4 与附录 C.3。
