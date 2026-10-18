// SPDX-License-Identifier: BSD-2-Clause
//! The dependency graph: supplier → consumer edges between device nodes,
//! from the tree and from firmware (DRIVER-MODEL.md §6).
//!
//! One graph orders every transition: a node attaches once its required
//! suppliers have, and shuts down before them. When a node attaches, only
//! its consumers are re-examined; there is no global retry. A node that is
//! waiting can always say what for ([`Graph::blocking`]).
//!
//! Firmware can describe cycles. [`Graph::break_cycles`] removes edges
//! until none remain: optional edges first, then non-tree edges whose
//! consumer is a tree ancestor of another node in the cycle, then any
//! non-tree edge. Tree edges are never removed.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec::Vec;

/// Whether a consumer can attach without the supplier.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum EdgeKind {
    /// The consumer cannot attach until the supplier has.
    Required,
    /// The consumer attaches regardless and is told when the supplier
    /// appears.
    Optional,
}

/// Where an edge came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Origin {
    /// Parent to child.
    Tree,
    /// ACPI `_DEP`.
    Dep,
    /// A `_CRS` resource source (GPIO, serial bus, interrupt combiner).
    ResourceSource,
    /// An ACPI power resource (`_PR0`, `_PR3`).
    PowerResource,
    /// A PCC channel an object's AML uses.
    Pcc,
    /// IORT (SMMU or ITS group).
    Iort,
    /// DMAR or IVRS scope.
    Iommu,
    /// A `_PRT` interrupt link.
    InterruptLink,
    /// The interrupt controller serving a GSI.
    Gsi,
    /// Declared by a driver during attach.
    Driver,
}

/// One edge.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Edge<N> {
    /// The node depended on.
    pub supplier: N,
    /// The dependent node.
    pub consumer: N,
    /// Required or optional.
    pub kind: EdgeKind,
    /// Where it came from.
    pub origin: Origin,
}

impl<N> Edge<N> {
    fn is_tree(&self) -> bool {
        self.origin == Origin::Tree
    }
}

/// Nodes whose edges form a cycle that cannot be broken (only tree edges,
/// which cannot happen in a well-formed tree).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cycle<N>(pub Vec<N>);

/// The graph, over node ids of type `N`. Orders are deterministic: ties
/// go to the smaller id.
#[derive(Clone, Debug)]
pub struct Graph<N> {
    nodes: BTreeSet<N>,
    edges: BTreeSet<Edge<N>>,
}

impl<N: Copy + Ord> Default for Graph<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<N: Copy + Ord> Graph<N> {
    /// An empty graph.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            nodes: BTreeSet::new(),
            edges: BTreeSet::new(),
        }
    }

    /// Adds a node (no-op if present).
    pub fn add_node(&mut self, n: N) {
        self.nodes.insert(n);
    }

    /// Removes a node and every edge touching it.
    pub fn remove_node(&mut self, n: N) {
        self.nodes.remove(&n);
        self.edges.retain(|e| e.supplier != n && e.consumer != n);
    }

    /// Adds an edge (its nodes too). Returns false if it was already there.
    pub fn add_edge(&mut self, e: Edge<N>) -> bool {
        self.nodes.insert(e.supplier);
        self.nodes.insert(e.consumer);
        self.edges.insert(e)
    }

    /// Removes an edge. Returns whether it was there.
    pub fn remove_edge(&mut self, e: &Edge<N>) -> bool {
        self.edges.remove(e)
    }

    /// Every node.
    pub fn nodes(&self) -> impl Iterator<Item = N> + '_ {
        self.nodes.iter().copied()
    }

    /// Every edge.
    pub fn edges(&self) -> impl Iterator<Item = &Edge<N>> + '_ {
        self.edges.iter()
    }

    /// Edges into `n` (its suppliers).
    pub fn suppliers(&self, n: N) -> impl Iterator<Item = &Edge<N>> + '_ {
        self.edges.iter().filter(move |e| e.consumer == n)
    }

    /// Edges out of `n` (its consumers): the nodes to re-examine when `n`
    /// attaches.
    pub fn consumers(&self, n: N) -> impl Iterator<Item = &Edge<N>> + '_ {
        self.edges.iter().filter(move |e| e.supplier == n)
    }

    /// The required suppliers of `n` that are not attached; empty means
    /// `n` is ready.
    pub fn missing(&self, n: N, attached: impl Fn(N) -> bool) -> Vec<N> {
        self.suppliers(n)
            .filter(|e| e.kind == EdgeKind::Required && !attached(e.supplier))
            .map(|e| e.supplier)
            .collect()
    }

    /// Why `n` waits: for each missing required supplier, the chain from
    /// it down to a supplier that is itself not waiting on anything (the
    /// root cause), as `kern.abyss.bus.waiting` reports.
    pub fn blocking(&self, n: N, attached: impl Fn(N) -> bool + Copy) -> Vec<Vec<N>> {
        self.missing(n, attached)
            .into_iter()
            .map(|s| {
                let mut chain = alloc::vec![s];
                let mut seen = BTreeSet::from([n, s]);
                let mut cur = s;
                while let Some(&next) = self.missing(cur, attached).first() {
                    if !seen.insert(next) {
                        break; // a cycle; break_cycles reports it
                    }
                    chain.push(next);
                    cur = next;
                }
                chain
            })
            .collect()
    }

    /// Suppliers before consumers, over every edge (required and
    /// optional), or the nodes of a cycle.
    pub fn topo_order(&self) -> Result<Vec<N>, Cycle<N>> {
        let mut indegree: BTreeMap<N, usize> = self.nodes.iter().map(|&n| (n, 0)).collect();
        for e in &self.edges {
            *indegree.entry(e.consumer).or_default() += 1;
        }
        let mut ready: BTreeSet<N> = indegree
            .iter()
            .filter(|&(_, &d)| d == 0)
            .map(|(&n, _)| n)
            .collect();
        let mut order = Vec::with_capacity(self.nodes.len());
        while let Some(n) = ready.pop_first() {
            order.push(n);
            for e in self.consumers(n) {
                let d = indegree.get_mut(&e.consumer).expect("edge to a node");
                *d -= 1;
                if *d == 0 {
                    ready.insert(e.consumer);
                }
            }
        }
        if order.len() == self.nodes.len() {
            Ok(order)
        } else {
            let done: BTreeSet<N> = order.into_iter().collect();
            Err(Cycle(
                self.nodes
                    .iter()
                    .copied()
                    .filter(|n| !done.contains(n))
                    .collect(),
            ))
        }
    }

    /// Consumers before suppliers: the order for shutdown and suspend.
    pub fn shutdown_order(&self) -> Result<Vec<N>, Cycle<N>> {
        self.topo_order().map(|mut o| {
            o.reverse();
            o
        })
    }

    /// Finds one cycle, as its edges in order.
    fn find_cycle(&self) -> Option<Vec<Edge<N>>> {
        #[derive(Clone, Copy, PartialEq, Eq)]
        enum Mark {
            Open,
            Done,
        }
        let mut mark: BTreeMap<N, Mark> = BTreeMap::new();
        for &start in &self.nodes {
            if mark.contains_key(&start) {
                continue;
            }
            // Iterative DFS keeping the path of edges taken.
            let mut path: Vec<Edge<N>> = Vec::new();
            let mut stack: Vec<(N, Vec<Edge<N>>)> =
                alloc::vec![(start, self.consumers(start).copied().collect())];
            mark.insert(start, Mark::Open);
            while let Some((node, pending)) = stack.last_mut() {
                let node = *node;
                if let Some(e) = pending.pop() {
                    match mark.get(&e.consumer) {
                        Some(Mark::Open) => {
                            // Back edge: the cycle is the path from the
                            // consumer's position to here, plus `e`.
                            let from = path
                                .iter()
                                .position(|p| p.supplier == e.consumer)
                                .unwrap_or(path.len());
                            let mut cycle = path[from..].to_vec();
                            cycle.push(e);
                            return Some(cycle);
                        }
                        Some(Mark::Done) => {}
                        None => {
                            mark.insert(e.consumer, Mark::Open);
                            path.push(e);
                            stack.push((e.consumer, self.consumers(e.consumer).copied().collect()));
                        }
                    }
                } else {
                    mark.insert(node, Mark::Done);
                    stack.pop();
                    path.pop();
                }
            }
        }
        None
    }

    /// Whether `a` is a proper tree ancestor of `b`.
    fn tree_ancestor(&self, a: N, b: N) -> bool {
        let mut cur = b;
        let mut steps = 0;
        while let Some(p) = self
            .suppliers(cur)
            .find(|e| e.is_tree())
            .map(|e| e.supplier)
        {
            if p == a {
                return true;
            }
            cur = p;
            steps += 1;
            if steps > self.nodes.len() {
                return false;
            }
        }
        false
    }

    /// Removes edges until the graph is acyclic; returns the removed edges
    /// in removal order, or the nodes of a cycle made only of tree edges.
    pub fn break_cycles(&mut self) -> Result<Vec<Edge<N>>, Cycle<N>> {
        let mut dropped = Vec::new();
        while let Some(cycle) = self.find_cycle() {
            let members: Vec<N> = cycle.iter().map(|e| e.supplier).collect();
            let pick = cycle
                .iter()
                .find(|e| e.kind == EdgeKind::Optional)
                .or_else(|| {
                    cycle.iter().find(|e| {
                        !e.is_tree()
                            && members
                                .iter()
                                .any(|&m| m != e.consumer && self.tree_ancestor(e.consumer, m))
                    })
                })
                .or_else(|| cycle.iter().find(|e| !e.is_tree()))
                .copied();
            let Some(e) = pick else {
                return Err(Cycle(members));
            };
            self.edges.remove(&e);
            dropped.push(e);
        }
        Ok(dropped)
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use super::*;

    fn tree(p: u32, c: u32) -> Edge<u32> {
        Edge {
            supplier: p,
            consumer: c,
            kind: EdgeKind::Required,
            origin: Origin::Tree,
        }
    }

    fn dep(s: u32, c: u32, kind: EdgeKind) -> Edge<u32> {
        Edge {
            supplier: s,
            consumer: c,
            kind,
            origin: Origin::Dep,
        }
    }

    /// root(0) → acpi(1) → {gpio(2), i2c(3), touch(4)}; touch needs i2c
    /// and gpio; i2c needs gpio.
    fn sample() -> Graph<u32> {
        let mut g = Graph::new();
        for (p, c) in [(0, 1), (1, 2), (1, 3), (1, 4)] {
            g.add_edge(tree(p, c));
        }
        g.add_edge(dep(3, 4, EdgeKind::Required));
        g.add_edge(dep(2, 4, EdgeKind::Required));
        g.add_edge(dep(2, 3, EdgeKind::Required));
        g
    }

    #[test]
    fn orders() {
        let g = sample();
        assert_eq!(g.topo_order().unwrap(), vec![0, 1, 2, 3, 4]);
        assert_eq!(g.shutdown_order().unwrap(), vec![4, 3, 2, 1, 0]);
        assert!(!g.add_edge_dup());
    }

    impl Graph<u32> {
        fn add_edge_dup(&self) -> bool {
            let mut g = self.clone();
            g.add_edge(tree(0, 1))
        }
    }

    #[test]
    fn readiness_and_blame() {
        let g = sample();
        let attached = |n: u32| n <= 1;
        assert_eq!(g.missing(4, attached), vec![2, 3]);
        assert_eq!(g.blocking(4, attached), vec![vec![2], vec![3, 2]]);
        assert!(g.missing(2, attached).is_empty(), "gpio is ready");
        let consumers: Vec<u32> = g.consumers(2).map(|e| e.consumer).collect();
        assert_eq!(
            consumers,
            vec![3, 4],
            "attaching gpio re-examines only these"
        );
    }

    #[test]
    fn breaks_cycles_preferring_optional_then_ancestor() {
        let mut g = sample();
        // gpio also (optionally) depends on touch: a cycle 2→4→2.
        g.add_edge(dep(4, 2, EdgeKind::Optional));
        assert!(g.topo_order().is_err());
        let dropped = g.break_cycles().unwrap();
        assert_eq!(dropped, vec![dep(4, 2, EdgeKind::Optional)]);
        assert!(g.topo_order().is_ok());

        // A required cycle against the tree: acpi(1) depends on touch(4),
        // whose tree ancestor it is; that edge goes.
        let mut g = sample();
        g.add_edge(dep(4, 1, EdgeKind::Required));
        let dropped = g.break_cycles().unwrap();
        assert_eq!(dropped, vec![dep(4, 1, EdgeKind::Required)]);
        assert!(g.edges().filter(|e| e.origin == Origin::Tree).count() == 4);
    }

    #[test]
    fn unbreakable_cycle_is_reported() {
        let mut g = Graph::new();
        g.add_edge(tree(1, 2));
        g.add_edge(tree(2, 1));
        assert!(matches!(g.break_cycles(), Err(Cycle(n)) if n.len() == 2));
    }

    /// Random trees with random firmware edges: breaking cycles always
    /// leaves an acyclic graph, never drops a tree edge, and the order then
    /// respects every remaining edge.
    #[test]
    #[allow(clippy::cast_possible_truncation)] // small random numbers
    fn random_graphs() {
        let mut rng = 0x2545_f491_4f6c_dd1d_u64;
        let mut next = |n: u64| {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng % n
        };
        for _ in 0..500 {
            let nodes = 2 + next(40) as u32;
            let mut g = Graph::new();
            g.add_node(0);
            for c in 1..nodes {
                g.add_edge(tree(next(u64::from(c)) as u32, c));
            }
            for _ in 0..next(3 * u64::from(nodes)) {
                let s = next(u64::from(nodes)) as u32;
                let c = next(u64::from(nodes)) as u32;
                if s != c {
                    let kind = if next(2) == 0 {
                        EdgeKind::Required
                    } else {
                        EdgeKind::Optional
                    };
                    g.add_edge(dep(s, c, kind));
                }
            }
            let trees = g.edges().filter(|e| e.origin == Origin::Tree).count();
            let dropped = g.break_cycles().expect("tree edges alone never cycle");
            assert!(dropped.iter().all(|e| e.origin != Origin::Tree));
            assert_eq!(
                g.edges().filter(|e| e.origin == Origin::Tree).count(),
                trees
            );
            let order = g.topo_order().expect("acyclic after breaking");
            let pos = |n: u32| order.iter().position(|&m| m == n).unwrap();
            assert!(g.edges().all(|e| pos(e.supplier) < pos(e.consumer)));
            assert_eq!(order.len(), nodes as usize);
        }
    }

    #[test]
    fn removal() {
        let mut g = sample();
        g.remove_node(3);
        assert_eq!(g.topo_order().unwrap(), vec![0, 1, 2, 4]);
        assert!(g.edges().all(|e| e.supplier != 3 && e.consumer != 3));
    }
}
