//! DSP Graph 构建、拓扑验证与 RCU 热替换（架构文档 §2.3）。
//!
//! 关键不变式（§2.3.2）：
//! - 音频线程永不参与图的分配/释放
//! - 图内部缓冲使用 `Box<[f32]>`，`compile()` 一次性分配，无后续扩容
//! - 旧图生命周期由 `Arc` 引用计数 + Deferred Drop 双重保障
//! - 音频线程与 UI 线程严禁共享同一个 Deferred Drop 入口
//!
//! v2 改进：
//! - 编译后保留邻接表，支持多输入混合（不再仅限串行链）
//! - 每个节点拥有独立的 `Box<[f32]>` 输出缓冲，消除 scratch 乒乓限制
//! - `ProcessStats` 提供每块处理的累积统计

use std::collections::VecDeque;
use std::sync::atomic::{AtomicPtr, AtomicU64, Ordering};
use std::sync::Arc;

use crate::node::{DspNode, ProcessContext};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NodeId(pub usize);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GraphError {
    /// 拓扑含环，无法调度
    CycleDetected,
    /// 节点索引越界
    InvalidNode(usize),
    /// 重复连接
    DuplicateEdge,
    /// 空图
    Empty,
    /// 自环
    SelfLoop,
}

/// 动态图构建器（UI 线程使用，可自由分配，§2.3.3）。
pub struct DynamicGraphBuilder {
    nodes: Vec<Box<dyn DspNode>>,
    connections: Vec<(usize, usize)>,
    max_block: usize,
}

impl DynamicGraphBuilder {
    pub fn new(max_block: usize) -> Self {
        assert!(max_block > 0);
        Self {
            nodes: Vec::new(),
            connections: Vec::new(),
            max_block,
        }
    }

    pub fn add_node(&mut self, node: Box<dyn DspNode>) -> NodeId {
        self.nodes.push(node);
        NodeId(self.nodes.len() - 1)
    }

    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    pub fn connect(&mut self, from: NodeId, to: NodeId) -> Result<(), GraphError> {
        if from.0 >= self.nodes.len() {
            return Err(GraphError::InvalidNode(from.0));
        }
        if to.0 >= self.nodes.len() {
            return Err(GraphError::InvalidNode(to.0));
        }
        if from.0 == to.0 {
            return Err(GraphError::SelfLoop);
        }
        if self.connections.contains(&(from.0, to.0)) {
            return Err(GraphError::DuplicateEdge);
        }
        self.connections.push((from.0, to.0));
        Ok(())
    }

    pub fn disconnect(&mut self, from: NodeId, to: NodeId) {
        self.connections
            .retain(|&(f, t)| !(f == from.0 && t == to.0));
    }

    /// 编译：拓扑排序验证（Kahn 算法）+ 一次性内存分配。
    /// 失败返回 `GraphError`（不变式 #16：动态图运行时验证）。
    pub fn compile(self) -> Result<Arc<DspGraphInner>, GraphError> {
        let n = self.nodes.len();
        if n == 0 {
            return Err(GraphError::Empty);
        }

        // ── 构建邻接表与入度表 ──
        let mut indegree = vec![0usize; n];
        let mut forward: Vec<Vec<usize>> = vec![Vec::new(); n];
        let mut reverse: Vec<Vec<usize>> = vec![Vec::new(); n];
        for &(from, to) in &self.connections {
            forward[from].push(to);
            reverse[to].push(from);
            indegree[to] += 1;
        }

        // ── Kahn 拓扑排序 ──
        let mut queue: VecDeque<usize> = (0..n).filter(|&i| indegree[i] == 0).collect();
        let mut order = Vec::with_capacity(n);
        while let Some(u) = queue.pop_front() {
            order.push(u);
            for &v in &forward[u] {
                indegree[v] -= 1;
                if indegree[v] == 0 {
                    queue.push_back(v);
                }
            }
        }
        if order.len() != n {
            return Err(GraphError::CycleDetected);
        }

        // ── 一次性分配所有缓冲（不变式 #1：此后零分配）──
        let node_bufs: Vec<Box<[f32]>> =
            (0..n).map(|_| vec![0.0; self.max_block].into_boxed_slice()).collect();

        // 将 Vec<Vec<usize>> 转为 Box<[Box<[usize]>]> 冻结
        let reverse_adj: Box<[Box<[usize]>]> = reverse
            .into_iter()
            .map(|v| v.into_boxed_slice())
            .collect::<Vec<_>>()
            .into_boxed_slice();

        Ok(Arc::new(DspGraphInner {
            nodes: self.nodes.into_boxed_slice(),
            order: order.into_boxed_slice(),
            reverse_adj,
            node_bufs: node_bufs.into_boxed_slice(),
            mix_scratch: vec![0.0; self.max_block].into_boxed_slice(),
            max_block: self.max_block,
            stats: ProcessStats::default(),
        }))
    }
}

/// 每块处理的累积统计（音频线程热路径就地更新，零分配）。
#[derive(Debug, Clone, Copy, Default)]
pub struct ProcessStats {
    /// 已处理的总块数
    pub blocks_processed: u64,
    /// 已处理的总采样数
    pub samples_processed: u64,
}

/// 编译后的不可扩容图。音频线程通过 `DspGraphHandle::load` 获取。
pub struct DspGraphInner {
    nodes: Box<[Box<dyn DspNode>]>,
    /// 拓扑排序后的节点执行顺序
    order: Box<[usize]>,
    /// 反向邻接表：reverse_adj[i] = 输入到节点 i 的所有源节点
    reverse_adj: Box<[Box<[usize]>]>,
    /// 每个节点独立的输出缓冲（编译时一次性分配）
    node_bufs: Box<[Box<[f32]>]>,
    /// 临时混合缓冲（多输入合并用）
    mix_scratch: Box<[f32]>,
    max_block: usize,
    stats: ProcessStats,
}

impl DspGraphInner {
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    pub fn max_block(&self) -> usize {
        self.max_block
    }

    pub fn stats(&self) -> &ProcessStats {
        &self.stats
    }

    /// 图级总延迟（各节点延迟之和，串行链语义）。
    pub fn total_latency_samples(&self) -> usize {
        self.nodes.iter().map(|n| n.latency_samples()).sum()
    }

    /// 音频线程热路径：按拓扑序依次处理。
    ///
    /// 多输入节点的输入为所有前驱节点输出缓冲的逐采样相加。
    /// 无前驱的源节点直接接收外部 `input`。
    /// 零分配（不变式 #1）。
    ///
    /// 借用策略：将 `self` 的各字段提取为独立局部变量，
    /// 向编译器显式声明不相交借用关系，保证 NLL 分析无歧义。
    pub fn process(&mut self, input: &[f32], output: &mut [f32], ctx: &ProcessContext) {
        let n = input.len().min(output.len()).min(self.max_block);

        // 显式拆字段：保证 borrow checker 看到五条独立借用链
        let order = &*self.order;
        let reverse_adj = &*self.reverse_adj;
        let nodes = &mut *self.nodes;
        let node_bufs = &mut *self.node_bufs;
        let mix_scratch = &mut *self.mix_scratch;

        for &idx in order.iter() {
            let sources: &[usize] = &reverse_adj[idx];

            // ── Phase 1: 准备输入 → mix_scratch ──
            if sources.is_empty() {
                // 源节点：直接接收外部输入
                mix_scratch[..n].copy_from_slice(&input[..n]);
            } else if sources.len() == 1 {
                // 单输入：从前驱输出缓冲拷贝（避免 node_bufs 双索引冲突）
                mix_scratch[..n].copy_from_slice(&node_bufs[sources[0]][..n]);
            } else {
                // 多输入：逐采样加法混合（§2.3.3 动态图拓扑）
                mix_scratch[..n].copy_from_slice(&node_bufs[sources[0]][..n]);
                for &src in &sources[1..] {
                    let src_buf = &node_bufs[src];
                    for j in 0..n {
                        mix_scratch[j] += src_buf[j];
                    }
                }
            }
            // Phase 1 结束：node_bufs 的不可变借用在此释放（NLL）

            // ── Phase 2: 处理节点 ──
            // nodes[idx] 可变 / mix_scratch 不可变 / node_bufs[idx] 可变
            // 三者来自不同局部变量，borrow checker 无歧义
            nodes[idx].process(
                &mix_scratch[..n],
                &mut node_bufs[idx][..n],
                ctx,
            );
        }

        // 最终输出 = 拓扑序最后一个节点的输出缓冲
        let last_idx = order[order.len() - 1];
        output[..n].copy_from_slice(&node_bufs[last_idx][..n]);
        // 防御：宿主 buffer 比处理块大时，尾部填零（不变式 #18）
        output[n..].fill(0.0);

        self.stats.blocks_processed += 1;
        self.stats.samples_processed += n as u64;
    }

    pub fn reset(&mut self) {
        for node in self.nodes.iter_mut() {
            node.reset();
        }
        for buf in self.node_bufs.iter_mut() {
            buf.fill(0.0);
        }
        self.mix_scratch.fill(0.0);
        self.stats = ProcessStats::default();
    }
}

/// RCU 热替换句柄（§2.3.2）。目标时延：swap ~5ns（§4.6）。
pub struct DspGraphHandle {
    current: AtomicPtr<DspGraphInner>,
    version: AtomicU64,
}

// SAFETY: DspGraphInner 通过 Arc 管理所有权；
// 指针交换是原子的，安全性由 swap 的 Safety 契约保证。
unsafe impl Send for DspGraphHandle {}
unsafe impl Sync for DspGraphHandle {}

impl DspGraphHandle {
    pub fn new(initial: Arc<DspGraphInner>) -> Self {
        Self {
            current: AtomicPtr::new(Arc::into_raw(initial) as *mut DspGraphInner),
            version: AtomicU64::new(1),
        }
    }

    pub fn empty() -> Self {
        Self {
            current: AtomicPtr::new(std::ptr::null_mut()),
            version: AtomicU64::new(0),
        }
    }

    /// 音频线程热路径读取。
    ///
    /// 解引用契约：仅音频线程可将其转为 `&mut` 使用（swap 后 UI 线程
    /// 不再触碰旧指针指向的数据）。
    #[inline(always)]
    pub fn load(&self) -> *const DspGraphInner {
        self.current.load(Ordering::Acquire)
    }

    #[inline(always)]
    pub fn is_null(&self) -> bool {
        self.current.load(Ordering::Acquire).is_null()
    }

    #[inline(always)]
    pub fn version(&self) -> u64 {
        self.version.load(Ordering::Acquire)
    }

    /// 原子交换当前图，返回旧图裸指针。
    ///
    /// # Safety（§2.3.2 回收协议）
    /// - 必须在非音频线程调用
    /// - `new_graph` 必须非 null，且通过 `Arc::into_raw` 产生
    /// - 调用者必须在返回后立即将旧指针推入 **UI 专用** Deferred Drop
    ///   Queue（严禁与音频线程共享同一入口）
    /// - 禁止在旧指针被回收前再次调用 swap
    pub unsafe fn swap(&self, new_graph: *mut DspGraphInner) -> *mut DspGraphInner {
        debug_assert!(!new_graph.is_null());
        let old = self.current.swap(new_graph, Ordering::AcqRel);
        self.version.fetch_add(1, Ordering::Release);
        old
    }
}

impl Drop for DspGraphHandle {
    fn drop(&mut self) {
        let ptr = self.current.load(Ordering::Acquire);
        if !ptr.is_null() {
            // SAFETY: 句柄独占最后一份 into_raw 引用
            unsafe { drop(Arc::from_raw(ptr as *const DspGraphInner)) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::{GainNode, PassThrough, SumMixer};

    // 小工具：链式初始化
    trait Tap: Sized {
        fn tap(mut self, f: impl FnOnce(&mut Self)) -> Self {
            f(&mut self);
            self
        }
    }
    impl Tap for DynamicGraphBuilder {}

    fn ctx() -> ProcessContext {
        ProcessContext {
            sample_rate: 96_000,
            block_size: 512,
        }
    }

    #[test]
    fn chain_compiles_and_processes() {
        let mut builder = DynamicGraphBuilder::new(512);
        let a = builder.add_node(Box::new(GainNode::new(-6.0)));
        let b = builder.add_node(Box::new(GainNode::new(-6.0)));
        builder.connect(a, b).unwrap();

        let mut graph = builder.compile().expect("合法 DAG 必须编译成功");
        assert_eq!(graph.node_count(), 2);

        let inner = Arc::get_mut(&mut graph).unwrap();
        let input = vec![1.0f32; 512];
        let mut output = vec![0.0f32; 512];
        inner.process(&input, &mut output, &ctx());

        // -6dB 两级串联 ≈ -12dB ≈ 0.2512
        let expected = 10.0f32.powf(-12.0 / 20.0);
        assert!(
            (output[0] - expected).abs() < 1e-4,
            "got {} expected {}",
            output[0],
            expected
        );
        assert_eq!(inner.stats().blocks_processed, 1);
        assert_eq!(inner.stats().samples_processed, 512);
    }

    /// 钻石形 DAG：A → B, A → C, B → D, C → D
    /// D 收到 B+C 的混合输入
    #[test]
    fn diamond_dag_multi_input_mixing() {
        let mut b = DynamicGraphBuilder::new(64);
        let a = b.add_node(Box::new(PassThrough));           // 源节点
        let left = b.add_node(Box::new(GainNode::new(-6.0))); // -6dB
        let right = b.add_node(Box::new(GainNode::new(-6.0))); // -6dB
        let sink = b.add_node(Box::new(PassThrough));          // 混合汇

        b.connect(a, left).unwrap();
        b.connect(a, right).unwrap();
        b.connect(left, sink).unwrap();
        b.connect(right, sink).unwrap();

        let mut graph = b.compile().unwrap();
        let inner = Arc::get_mut(&mut graph).unwrap();

        let input = vec![1.0f32; 64];
        let mut output = vec![0.0f32; 64];
        inner.process(&input, &mut output, &ctx());

        // A 直通 1.0 → left/right 各 ≈ 0.5012 → sink 混合 ≈ 1.0024
        let half = 10.0f32.powf(-6.0 / 20.0);
        let expected = half * 2.0;
        assert!(
            (output[0] - expected).abs() < 1e-3,
            "钻石混合: got {} expected {}",
            output[0],
            expected
        );
    }

    /// 三路混合节点：A→D, B→D, C→D
    #[test]
    fn three_way_merge() {
        let mut b = DynamicGraphBuilder::new(16);
        let a = b.add_node(Box::new(GainNode::new(0.0)));  // 1.0
        let b_node = b.add_node(Box::new(GainNode::new(0.0)));  // 1.0
        let c = b.add_node(Box::new(GainNode::new(0.0)));  // 1.0
        let d = b.add_node(Box::new(SumMixer));

        b.connect(a, d).unwrap();
        b.connect(b_node, d).unwrap();
        b.connect(c, d).unwrap();

        let mut graph = b.compile().unwrap();
        let inner = Arc::get_mut(&mut graph).unwrap();

        let input = vec![2.0f32; 16];
        let mut output = vec![0.0f32; 16];
        inner.process(&input, &mut output, &ctx());

        // 三个源各直通 2.0 → SumMixer 混合：逐采样加法 → 6.0
        assert!(
            (output[0] - 6.0).abs() < 1e-5,
            "三路混合: got {}",
            output[0]
        );
    }

    #[test]
    fn cycle_is_rejected() {
        let mut builder = DynamicGraphBuilder::new(64);
        let a = builder.add_node(Box::new(GainNode::new(0.0)));
        let b = builder.add_node(Box::new(GainNode::new(0.0)));
        builder.connect(a, b).unwrap();
        builder.connect(b, a).unwrap();
        assert_eq!(builder.compile().err(), Some(GraphError::CycleDetected));
    }

    #[test]
    fn self_loop_rejected() {
        let mut builder = DynamicGraphBuilder::new(64);
        let a = builder.add_node(Box::new(GainNode::new(0.0)));
        assert_eq!(builder.connect(a, a), Err(GraphError::SelfLoop));
    }

    #[test]
    fn invalid_edge_and_duplicate_rejected() {
        let mut builder = DynamicGraphBuilder::new(64);
        let a = builder.add_node(Box::new(GainNode::new(0.0)));
        assert_eq!(
            builder.connect(a, NodeId(9)),
            Err(GraphError::InvalidNode(9))
        );
        let b = builder.add_node(Box::new(GainNode::new(0.0)));
        builder.connect(a, b).unwrap();
        assert_eq!(builder.connect(a, b), Err(GraphError::DuplicateEdge));
    }

    #[test]
    fn rcu_swap_protocol() {
        let g1 = DynamicGraphBuilder::new(64)
            .tap(|b| {
                b.add_node(Box::new(GainNode::new(0.0)));
            })
            .compile()
            .unwrap();
        let handle = DspGraphHandle::new(g1);
        assert_eq!(handle.version(), 1);

        let mut b2 = DynamicGraphBuilder::new(64);
        b2.add_node(Box::new(GainNode::new(-3.0)));
        let g2 = b2.compile().unwrap();
        let new_ptr = Arc::into_raw(g2) as *mut DspGraphInner;

        // SAFETY: 测试线程即"非音频线程"；旧指针随即回收
        let old = unsafe { handle.swap(new_ptr) };
        assert_eq!(handle.version(), 2);
        assert_eq!(handle.load(), new_ptr as *const DspGraphInner);
        // 模拟 GC 线程的 Physical Drop
        unsafe { drop(Arc::from_raw(old as *const DspGraphInner)) };
    }

    #[test]
    fn empty_handle() {
        let handle = DspGraphHandle::empty();
        assert!(handle.is_null());
        assert_eq!(handle.version(), 0);
    }

    #[test]
    fn reset_clears_stats() {
        let mut g = DynamicGraphBuilder::new(16)
            .tap(|b| { b.add_node(Box::new(PassThrough)); })
            .compile().unwrap();
        let inner = Arc::get_mut(&mut g).unwrap();
        let input = [1.0f32; 16];
        let mut out = [0.0f32; 16];
        inner.process(&input, &mut out, &ctx());
        inner.process(&input, &mut out, &ctx());
        assert_eq!(inner.stats().blocks_processed, 2);
        inner.reset();
        assert_eq!(inner.stats().blocks_processed, 0);
    }
}
