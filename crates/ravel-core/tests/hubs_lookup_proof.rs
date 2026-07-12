//! Proof for the hubs id-based lookup change (analysis::hubs_from_graph).
//!
//! Correctness proof for the hubs id-based lookup change on a large synthetic graph.

use ravel_core::analysis::hubs_from_graph;
use ravel_core::graph::GraphIndex;
use ravel_core::model::{Edge, EdgeConfidence, EdgeKind};
use std::cmp::Reverse;
use std::collections::BinaryHeap;

const N: u32 = 50_000; // large-codebase-scale node count
const LIMIT: usize = 20;

fn edge(from: String, to: String) -> Edge {
    Edge {
        from,
        to,
        kind: EdgeKind::Import,
        confidence: EdgeConfidence::Resolved {
            score: 1.0,
            reason: "bench".into(),
        },
        type_only: false,
    }
}

fn build_graph() -> GraphIndex {
    // ~2 edges/node, spread so most nodes have in_degree > 0 (exercises the
    // hot branch), with a skewed tail so top-k is non-trivial.
    let mut edges = Vec::with_capacity((N as usize) * 2);
    for i in 0..N {
        let name = format!("pkg/module_{i:05}/symbol");
        edges.push(edge(
            name.clone(),
            format!("pkg/module_{:05}/symbol", (i * 7) % N),
        ));
        edges.push(edge(name, format!("pkg/module_{:05}/symbol", (i * 13) % N)));
    }
    GraphIndex::from_edges(&edges, "proof".into())
}

/// OLD path: resolve every node name back to its id via the HashMap.
fn hubs_name_based(graph: &GraphIndex, limit: usize) -> Vec<(usize, String, usize)> {
    let mut heap: BinaryHeap<Reverse<(usize, String, usize)>> = BinaryHeap::new();
    for name in graph.node_names() {
        let in_d = graph.in_degree(name);
        if in_d == 0 {
            continue;
        }
        let out_d = graph.out_degree(name);
        if heap.len() < limit {
            heap.push(Reverse((in_d, name.to_owned(), out_d)));
        } else if let Some(Reverse((min_in, _, _))) = heap.peek() {
            if in_d > *min_in {
                heap.pop();
                heap.push(Reverse((in_d, name.to_owned(), out_d)));
            }
        }
    }
    let mut v: Vec<_> = heap.into_iter().map(|Reverse(t)| t).collect();
    v.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    v
}

/// NEW path: iterate in index order, use the id directly.
fn hubs_id_based(graph: &GraphIndex, limit: usize) -> Vec<(usize, String, usize)> {
    let mut heap: BinaryHeap<Reverse<(usize, String, usize)>> = BinaryHeap::new();
    for (id, name) in graph.node_names().enumerate() {
        let id = id as u32;
        let in_d = graph.in_degree_id(id);
        if in_d == 0 {
            continue;
        }
        let out_d = graph.out_degree_id(id);
        if heap.len() < limit {
            heap.push(Reverse((in_d, name.to_owned(), out_d)));
        } else if let Some(Reverse((min_in, _, _))) = heap.peek() {
            if in_d > *min_in {
                heap.pop();
                heap.push(Reverse((in_d, name.to_owned(), out_d)));
            }
        }
    }
    let mut v: Vec<_> = heap.into_iter().map(|Reverse(t)| t).collect();
    v.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    v
}

#[test]
fn hubs_id_lookup_is_identical() {
    let graph = build_graph();
    println!(
        "graph: {} nodes, {} edges",
        graph.node_count(),
        graph.edge_count()
    );

    let old = hubs_name_based(&graph, LIMIT);
    let new = hubs_id_based(&graph, LIMIT);

    // Claim 1a: id-based == name-based, byte for byte.
    assert_eq!(old, new, "id-based top-k diverged from name-based");

    // Claim 1b: shipped hubs_from_graph agrees with both.
    let shipped: Vec<(usize, String, usize)> = hubs_from_graph(&graph, LIMIT)
        .into_iter()
        .map(|h| (h.in_degree, h.name, h.out_degree))
        .collect();
    assert_eq!(shipped, new, "shipped hubs_from_graph diverged");
}
