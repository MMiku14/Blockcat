# RIAPS 实时音频微内核 — 参考实现 Workspace

对应架构文档 v4.8.1 §1.6「代码库拓扑与模块划分」的物理文件实现。

## 编译 + 测试

```bash
cd riaps-workspace
cargo build --workspace
cargo test  --workspace

# release 构建（panic = abort 生效，不变式 #11）
cargo build --workspace --release
```

## Crate 拓扑（单向无环依赖）

```text
riaps-core   [no_std + alloc]   SPSC / RCU / DropVTable / DeferredDropQueue / EmergencyPool
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

## v2 主要改进

### riaps-core/spsc.rs
- **`push_slice`** 批量推入（单次 Release fence，比逐元素 push 更高效）
- **`pop_slice_into`** 动态长度批量弹出
- **`peek`** 非破坏性窥视队首
- **`Producer::available` / `Consumer::available`** 背压查询
- 多线程压力测试（100K 消息序列化验证）

### riaps-core/memory.rs
- **`DeferredDropQueue<N>`** 基于 SPSC 的类型擦除回收队列（§2.2 文档化但原先缺失）
- **`EmergencyPool<SLOTS>`** 数组化紧急槽位池，含 `push` / `drain_all` / `vacant_count`
- **`GcContext::defer_box_entry`** 一步生成 `DropEntry`
- **`EmergencySlot::drain_and_reclaim`** 便捷排空 + 释放

### riaps-host/adapter_buf.rs
- **批量 `copy_from_slice`** 替代逐采样 `pop_front`（编译为 memcpy/SIMD）
- **`make_contiguous` + `drain`** 替代逐元素 pop（VecDeque O(1) head 偏移）
- **`theoretical_latency(host_block_size)`** 确定性延迟公式
- **`dropped_input_samples` / `zero_filled_samples`** 运行指标
- **`reset`** 采样率/块大小切换后清空状态
- 修复测试 bug：输入从 1.0 起步消除零填充歧义，增加极端比例、host>dsp、容量溢出、长压测、reset 等 7 个测试

### riaps-dsp/graph.rs
- **多输入混合**：编译后保留反向邻接表，process 时自动逐采样加法混合前驱输出
- **每节点独立输出缓冲**：消除原先双 scratch 乒乓的串行链限制
- **`ProcessStats`**：blocks_processed / samples_processed 热路径就地更新
- **`DspGraphHandle::empty`**：空句柄，延迟初始化场景
- **`GraphError::SelfLoop`**：自环检测
- 钻石 DAG、三路混合、reset 等新测试

### riaps-dsp/node.rs
- **`PassThrough`** 恒等直通节点（零开销基准）
- **`SumMixer`** 逐采样加法节点（多输入汇点）
- **`DelayNode`** 固定采样延迟（环形缓冲，含 `latency_samples`）
- **`ParameterQueue::has_pending`** 非破坏性状态查询

## 测试覆盖

```text
riaps-core:  SPSC 环回/回绕/批量拷贝/容量/push_slice/pop_slice_into/peek/多线程压力
             RCU swap 协议
             DeferredDropQueue 往返/溢出
             EmergencySlot push/drain, EmergencyPool push/drain_all

riaps-sys:   FpuGuard RAII、时钟单调性、探针记录/直通
             Provisioner BestEffort/Strict 语义

riaps-host:  NullAdapter 生命周期
             AdapterBuffer：块大小失配(384↔512)、极端比例(64↔512)、
             host>dsp(512↔128)、容量溢出截断、长压测(500轮)、reset

riaps-dsp:   参数队列只留最新值/has_pending、增益节点、直通/混合/延迟节点
             图编译/环检测/自环/非法边、钻石DAG多输入混合、三路混合
             RCU 热替换、空句柄、stats/reset
             EventBus 保留槽位保护致命事件
```
