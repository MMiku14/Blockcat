//! DSP Graph 构建、拓扑验证与 RCU 热替换（架构文档 §2.3）。
//!
//! 关键不变式（§2.3.2）：
//! - 音频线程永不参与图的分配/释放
//! - 图内部缓冲使用 `Box<[f32]>`，`compile()` 一次性分配，无后续扩容
//! - 旧图生命周期由 `Arc` 引用计数 + Deferred Drop 双重保障
//! - 音频线程与 UI 线程严禁共享同一个 Deferred Drop 入口

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
    pub fn compile(self) -> Result<Arc<DspGraphInner>, GraphError> {
        let n = self.nodes.len();
        if n == 0 {
            return Err(GraphError::Empty);
        }

        // Kahn 拓扑排序
        let mut indegree = vec![0usize; n];
        let mut adjacency: Vec<Vec<usize>> = vec![Vec::new(); n];
        for &(from, to) in &self.connections {
            adjacency[from].push(to);
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

        Ok(Arc::new(DspGraphInner {
            nodes: self.nodes.into_boxed_slice(),
            order: order.into_boxed_slice(),
            // Box<[f32]>：编译期确定布局，无后续扩容（§2.3.2）
            scratch_a: vec![0.0; self.max_block].into_boxed_slice(),
            scratch_b: vec![0.0; self.max_block].into_boxed_slice(),
            max_block: self.max_block,
        }))
    }
}

/// 编译后的不可扩容图。音频线程通过 `DspGraphHandle::load` 获取。
pub struct DspGraphInner {
    nodes: Box<[Box<dyn DspNode>]>,
    order: Box<[usize]>,
    scratch_a: Box<[f32]>,
    scratch_b: Box<[f32]>,
    max_block: usize,
}

impl DspGraphInner {
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    pub fn max_block(&self) -> usize {
        self.max_block
    }

    /// 图级总延迟（各节点延迟之和，串行链语义）。
    pub fn total_latency_samples(&self) -> usize {
        self.nodes.iter().map(|n| n.latency_samples()).sum()
    }

    /// 音频线程热路径：按拓扑序依次处理（参考实现为串行链语义，
    /// 生产实现按邻接表做多输入混合）。零分配（不变式 #1）。
    pub fn process(&mut self, input: &[f32], output: &mut [f32], ctx: &ProcessContext) {
        let n = input.len().min(output.len()).min(self.max_block);
        self.scratch_a[..n].copy_from_slice(&input[..n]);

        for i in 0..self.order.len() {
            let idx = self.order[i];
            {
                // 字段级不相交借用：nodes[idx] 可写 / scratch_a 只读 / scratch_b 可写
                let node = &mut self.nodes[idx];
                let inb = &self.scratch_a[..n];
                let outb = &mut self.scratch_b[..n];
                node.process(inb, outb, ctx);
            }
            std::mem::swap(&mut self.scratch_a, &mut self.scratch_b);
        }
        output[..n].copy_from_slice(&self.scratch_a[..n]);
        // 防御：宿主 buffer 比处理块大时，尾部填零（不变式 #18）
        for o in output[n..].iter_mut() {
            *o = 0.0;
        }
    }

    pub fn reset(&mut self) {
        for node in self.nodes.iter_mut() {
            node.reset();
        }
    }
}

/// RCU 热替换句柄（§2.3.2）。目标时延：swap ~5ns（§4.6）。
pub struct DspGraphHandle {
    current: AtomicPtr<DspGraphInner>,
    version: AtomicU64,
}

impl DspGraphHandle {
    pub fn new(initial: Arc<DspGraphInner>) -> Self {
        Self {
            current: AtomicPtr::new(Arc::into_raw(initial) as *mut DspGraphInner),
            version: AtomicU64::new(1),
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
    use crate::node::GainNode;

    #[test]
    fn chain_compiles_and_processes() {
        let mut builder = DynamicGraphBuilder::new(512);
        let a = builder.add_node(Box::new(GainNode::new(-6.0)));
        let b = builder.add_node(Box::new(GainNode::new(-6.0)));
        builder.connect(a, b).unwrap();

        let mut graph = builder.compile().expect("合法 DAG 必须编译成功");
        assert_eq!(graph.node_count(), 2);

        let inner = Arc::get_mut(&mut graph).unwrap();
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
        let new_ptr = Arc::into_raw(g2) as *mut DspGraphInner;

        // SAFETY: 测试线程即"非音频线程"；旧指针随即回收
        let old = unsafe { handle.swap(new_ptr) };
        assert_eq!(handle.version(), 2);
        assert_eq!(handle.load(), new_ptr as *const DspGraphInner);
        // 模拟 GC 线程的 Physical Drop
        unsafe { drop(Arc::from_raw(old as *const DspGraphInner)) };
    }

    // 小工具：链式初始化
    trait Tap: Sized {
        fn tap(mut self, f: impl FnOnce(&mut Self)) -> Self {
            f(&mut self);
            self
        }
    }
    impl Tap for DynamicGraphBuilder {}
}
