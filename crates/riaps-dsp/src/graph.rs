//! DSP Graph 构建、拓扑验证与 RCU 热替换（架构文档 §2.3）。
//!
//! 关键不变式（§2.3.2）：
//! - 音频线程永不参与图的分配/释放
//! - 图内部缓冲使用 `Box<[f32]>`，`compile()` 一次性分配，无后续扩容
//! - 图以唯一 `Box` 所有权发布；旧图由 hazard token 延迟回收
//! - 音频线程与 UI 线程严禁共享同一个 Deferred Drop 入口

use std::collections::VecDeque;
use std::marker::PhantomData;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, Ordering};
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
    /// 节点缓冲区大小计算溢出
    BufferSizeOverflow,
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

    pub fn connect(&mut self, from: NodeId, to: NodeId) -> Result<(), GraphError> {
        if from.0 >= self.nodes.len() {
            return Err(GraphError::InvalidNode(from.0));
        }
        if to.0 >= self.nodes.len() {
            return Err(GraphError::InvalidNode(to.0));
        }
        if self.connections.contains(&(from.0, to.0)) {
            return Err(GraphError::DuplicateEdge);
        }
        self.connections.push((from.0, to.0));
        Ok(())
    }

    pub fn disconnect(&mut self, from: NodeId, to: NodeId) {
        self.connections.retain(|&(f, t)| !(f == from.0 && t == to.0));
    }

    /// 编译：拓扑排序验证（Kahn 算法）+ 一次性内存分配。
    /// 失败返回 `GraphError`（不变式 #16：动态图运行时验证）。
    pub fn compile(self) -> Result<Box<DspGraphInner>, GraphError> {
        let n = self.nodes.len();
        if n == 0 {
            return Err(GraphError::Empty);
        }

        // Kahn 拓扑排序
        let mut indegree = vec![0usize; n];
        let mut adjacency: Vec<Vec<usize>> = vec![Vec::new(); n];
        let mut predecessors: Vec<Vec<usize>> = vec![Vec::new(); n];
        for &(from, to) in &self.connections {
            adjacency[from].push(to);
            predecessors[to].push(from);
            indegree[to] += 1;
        }
        let mut queue: VecDeque<usize> =
            (0..n).filter(|&i| indegree[i] == 0).collect();
        let mut order = Vec::with_capacity(n);
        while let Some(u) = queue.pop_front() {
            order.push(u);
            for &v in &adjacency[u] {
                indegree[v] -= 1;
                if indegree[v] == 0 {
                    queue.push_back(v);
                }
            }
        }
        if order.len() != n {
            return Err(GraphError::CycleDetected);
        }

        let sinks: Vec<usize> = adjacency
            .iter()
            .enumerate()
            .filter_map(|(index, successors)| successors.is_empty().then_some(index))
            .collect();
        let mut path_latency = vec![0usize; n];
        for &index in &order {
            let input_latency = predecessors[index]
                .iter()
                .map(|&predecessor| path_latency[predecessor])
                .max()
                .unwrap_or(0);
            path_latency[index] = input_latency.saturating_add(self.nodes[index].latency_samples());
        }
        let total_latency = sinks
            .iter()
            .map(|&sink| path_latency[sink])
            .max()
            .unwrap_or(0);
        let buffer_samples = n
            .checked_mul(self.max_block)
            .ok_or(GraphError::BufferSizeOverflow)?;

        Ok(Box::new(DspGraphInner {
            nodes: self.nodes.into_boxed_slice(),
            order: order.into_boxed_slice(),
            predecessors: predecessors
                .into_iter()
                .map(Vec::into_boxed_slice)
                .collect::<Vec<_>>()
                .into_boxed_slice(),
            sinks: sinks.into_boxed_slice(),
            node_outputs: vec![0.0; buffer_samples].into_boxed_slice(),
            mix_buffer: vec![0.0; self.max_block].into_boxed_slice(),
            max_block: self.max_block,
            total_latency,
        }))
    }
}

/// Compiled fixed-capacity graph, accessed through `DspGraphHandle::try_pin`.
pub struct DspGraphInner {
    nodes: Box<[Box<dyn DspNode>]>,
    order: Box<[usize]>,
    predecessors: Box<[Box<[usize]>]>,
    sinks: Box<[usize]>,
    node_outputs: Box<[f32]>,
    mix_buffer: Box<[f32]>,
    max_block: usize,
    total_latency: usize,
}

impl DspGraphInner {
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    pub fn max_block(&self) -> usize {
        self.max_block
    }

    /// 图级总延迟，即所有输入到输出路径中的最大累计延迟。
    pub fn total_latency_samples(&self) -> usize {
        self.total_latency
    }

    /// 音频线程热路径：按拓扑序处理；多前驱节点按样本求和，多个
    /// sink 同样混合到图输出。全部工作缓冲在 `compile()` 中预分配。
    pub fn process(&mut self, input: &[f32], output: &mut [f32], ctx: &ProcessContext) {
        let sample_count = input.len().min(output.len()).min(self.max_block);

        for &index in &self.order {
            self.mix_buffer[..sample_count].fill(0.0);
            if self.predecessors[index].is_empty() {
                self.mix_buffer[..sample_count].copy_from_slice(&input[..sample_count]);
            } else {
                for &predecessor in &self.predecessors[index] {
                    let start = predecessor * self.max_block;
                    let predecessor_output = &self.node_outputs[start..start + sample_count];
                    for (mixed, &sample) in self.mix_buffer[..sample_count]
                        .iter_mut()
                        .zip(predecessor_output)
                    {
                        *mixed += sample;
                    }
                }
            }

            let output_start = index * self.max_block;
            let node_output =
                &mut self.node_outputs[output_start..output_start + sample_count];
            self.nodes[index].process(
                &self.mix_buffer[..sample_count],
                node_output,
                ctx,
            );
        }

        output.fill(0.0);
        for &sink in &self.sinks {
            let start = sink * self.max_block;
            let sink_output = &self.node_outputs[start..start + sample_count];
            for (mixed, &sample) in output[..sample_count].iter_mut().zip(sink_output) {
                *mixed += sample;
            }
        }
    }

    pub fn reset(&mut self) {
        for node in &mut self.nodes {
            node.reset();
        }
    }
}

/// RCU 热替换句柄（§2.3.2）。目标时延：swap ~5ns（§4.6）。
pub struct DspGraphHandle {
    state: Arc<PublicationState>,
}

struct PublicationState {
    current: AtomicPtr<DspGraphInner>,
    version: AtomicU64,
    hazard: AtomicPtr<DspGraphInner>,
    pin_active: AtomicBool,
}

impl DspGraphHandle {
    pub fn new(initial: Box<DspGraphInner>) -> Self {
        Self {
            state: Arc::new(PublicationState {
                current: AtomicPtr::new(Box::into_raw(initial)),
                version: AtomicU64::new(1),
                hazard: AtomicPtr::new(std::ptr::null_mut()),
                pin_active: AtomicBool::new(false),
            }),
        }
    }

    /// Pin the graph currently visible to the audio thread.
    ///
    /// Only one guard may exist at a time. A reentrant attempt returns `None`
    /// instead of blocking. The hazard-pointer validation loop ensures that a
    /// retired graph cannot be reclaimed before the guard publishes its pin.
    pub fn try_pin(&self) -> Option<DspGraphReadGuard<'_>> {
        self.state
            .pin_active
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .ok()?;

        loop {
            let pointer = self.state.current.load(Ordering::SeqCst);
            self.state.hazard.store(pointer, Ordering::SeqCst);
            if self.state.current.load(Ordering::SeqCst) == pointer {
                return Some(DspGraphReadGuard {
                    handle: self,
                    pointer,
                    _not_send: PhantomData,
                });
            }
        }
    }

    #[inline(always)]
    pub fn version(&self) -> u64 {
        self.state.version.load(Ordering::Acquire)
    }

    /// Atomically publish a graph and return the retired graph.
    ///
    /// Call this only from the non-audio control plane. The returned
    /// [`RetiredDspGraph`] must be queued on the UI-specific deferred-drop
    /// path and reclaimed only through [`RetiredDspGraph::try_reclaim`].
    pub fn swap(&self, new_graph: Box<DspGraphInner>) -> RetiredDspGraph {
        let new_graph = Box::into_raw(new_graph);
        let old = self.state.current.swap(new_graph, Ordering::AcqRel);
        let version = self.state.version.fetch_add(1, Ordering::Release) + 1;
        RetiredDspGraph {
            pointer: old,
            retired_at_version: version,
            state: Arc::clone(&self.state),
        }
    }
}

/// A hazard-pointer guard for the graph used by one audio callback.
pub struct DspGraphReadGuard<'a> {
    handle: &'a DspGraphHandle,
    pointer: *mut DspGraphInner,
    // Audio graph mutation is single-threaded; do not move a live guard.
    _not_send: PhantomData<Rc<()>>,
}

impl DspGraphReadGuard<'_> {
    pub fn graph(&self) -> &DspGraphInner {
        // SAFETY: the published hazard protects this pointer until Drop.
        unsafe { &*self.pointer }
    }

    /// Process one block through the pinned graph.
    pub fn process(
        &mut self,
        input: &[f32],
        output: &mut [f32],
        context: &ProcessContext,
    ) {
        // SAFETY: `pin_active` permits one live guard, so mutable graph access
        // is exclusive to this audio callback.
        unsafe { &mut *self.pointer }.process(input, output, context);
    }
}

impl Drop for DspGraphReadGuard<'_> {
    fn drop(&mut self) {
        self.handle
            .state
            .hazard
            .store(std::ptr::null_mut(), Ordering::SeqCst);
        self.handle
            .state
            .pin_active
            .store(false, Ordering::Release);
    }
}

/// A graph removed from publication but potentially still used by the audio
/// callback. Dropping this token without reclaiming intentionally leaks the
/// graph rather than risking a realtime use-after-free.
#[must_use = "retired graphs must be deferred and reclaimed"]
pub struct RetiredDspGraph {
    pointer: *mut DspGraphInner,
    retired_at_version: u64,
    state: Arc<PublicationState>,
}

// SAFETY: the token owns one retired Box raw pointer. Reclamation validates
// the audio hazard before reconstructing and dropping that Box.
unsafe impl Send for RetiredDspGraph {}

impl RetiredDspGraph {
    pub fn retired_at_version(&self) -> u64 {
        self.retired_at_version
    }

    /// Try to reclaim this graph after checking the audio-thread hazard.
    pub fn try_reclaim(self) -> Result<(), Self> {
        match self.try_into_box() {
            Ok(graph) => {
                drop(graph);
                Ok(())
            }
            Err(retired) => Err(retired),
        }
    }

    /// Recover ownership after checking that the audio thread no longer pins
    /// this graph. This is useful for allocation-free alternating benchmarks;
    /// production GC normally calls [`Self::try_reclaim`].
    pub fn try_into_box(mut self) -> Result<Box<DspGraphInner>, Self> {
        if self.state.hazard.load(Ordering::SeqCst) == self.pointer {
            return Err(self);
        }

        let pointer = std::mem::take(&mut self.pointer);
        // SAFETY: swap produced one Box raw pointer; hazard validation shows
        // no audio callback can still acquire or dereference the old pointer.
        Ok(unsafe { Box::from_raw(pointer) })
    }
}

impl Drop for PublicationState {
    fn drop(&mut self) {
        debug_assert!(!self.pin_active.load(Ordering::Relaxed));
        let ptr = self.current.load(Ordering::Acquire);
        if !ptr.is_null() {
            // SAFETY: the publication state owns the current Box raw pointer.
            unsafe { drop(Box::from_raw(ptr)) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::GainNode;

    #[test]
    fn chain_compiles_and_processes() {
        let mut builder = DynamicGraphBuilder::new(512);
        let a = builder.add_node(Box::new(GainNode::new(-6.0)));
        let b = builder.add_node(Box::new(GainNode::new(-6.0)));
        builder.connect(a, b).unwrap();

        let mut graph = builder.compile().expect("合法 DAG 必须编译成功");
        assert_eq!(graph.node_count(), 2);

        let inner = &mut *graph;
        let ctx = ProcessContext {
            sample_rate: 96_000,
            block_size: 512,
        };
        let input = vec![1.0f32; 512];
        let mut output = vec![0.0f32; 512];
        inner.process(&input, &mut output, &ctx);

        // -6dB 两级串联 ≈ -12dB ≈ 0.2512
        let expected = 10.0f32.powf(-12.0 / 20.0);
        assert!((output[0] - expected).abs() < 1e-4, "got {}", output[0]);
    }

    #[test]
    fn disconnected_sinks_are_mixed_instead_of_serialized() {
        let mut builder = DynamicGraphBuilder::new(32);
        builder.add_node(Box::new(GainNode::new(-6.0)));
        builder.add_node(Box::new(GainNode::new(-6.0)));
        let mut graph = builder.compile().expect("parallel graph must compile");
        let inner = &mut *graph;
        let context = ProcessContext {
            sample_rate: 48_000,
            block_size: 32,
        };
        let input = [1.0_f32; 32];
        let mut output = [0.0_f32; 32];

        inner.process(&input, &mut output, &context);

        let expected = 2.0 * 10.0_f32.powf(-6.0 / 20.0);
        assert!((output[0] - expected).abs() < 1e-5);
    }

    #[test]
    fn branching_graph_uses_the_longest_latency_path() {
        let mut builder = DynamicGraphBuilder::new(16);
        let root = builder.add_node(Box::new(LatencyNode(4)));
        let short = builder.add_node(Box::new(LatencyNode(8)));
        let long = builder.add_node(Box::new(LatencyNode(32)));
        builder.connect(root, short).expect("valid edge");
        builder.connect(root, long).expect("valid edge");
        let graph = builder.compile().expect("branching graph must compile");

        assert_eq!(graph.total_latency_samples(), 36);
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
        let retired = handle.swap(g2);
        assert_eq!(handle.version(), 2);
        assert_eq!(retired.retired_at_version(), 2);

        let guard = handle.try_pin().expect("one audio reader may pin");
        assert_eq!(guard.graph().node_count(), 1);
        assert!(handle.try_pin().is_none(), "reentrant pin must not block");
        drop(guard);

        assert!(retired.try_reclaim().is_ok());
    }

    #[test]
    fn retired_graph_waits_for_the_audio_hazard() {
        let mut first_builder = DynamicGraphBuilder::new(64);
        first_builder.add_node(Box::new(GainNode::new(0.0)));
        let handle = DspGraphHandle::new(first_builder.compile().expect("valid graph"));
        let guard = handle.try_pin().expect("first pin succeeds");

        let mut second_builder = DynamicGraphBuilder::new(64);
        second_builder.add_node(Box::new(GainNode::new(-3.0)));
        let replacement = second_builder.compile().expect("valid graph");
        let retired = handle.swap(replacement);

        let retired = match retired.try_reclaim() {
            Ok(()) => panic!("a pinned graph must not be reclaimed"),
            Err(retired) => retired,
        };
        drop(guard);
        assert!(retired.try_reclaim().is_ok());
    }

    // 小工具：链式初始化
    trait Tap: Sized {
        fn tap(mut self, f: impl FnOnce(&mut Self)) -> Self {
            f(&mut self);
            self
        }
    }
    impl Tap for DynamicGraphBuilder {}

    struct LatencyNode(usize);

    impl DspNode for LatencyNode {
        fn process(&mut self, input: &[f32], output: &mut [f32], _ctx: &ProcessContext) {
            output.copy_from_slice(input);
        }

        fn latency_samples(&self) -> usize {
            self.0
        }
    }
}
