// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! What `#[stack_safe]` costs. Run as:
//!
//! ```text
//! cargo run --release --example perf_contrast
//! ```
//!
//! Three implementations of the same sum over the same arena tree:
//!
//! - `naive` — ordinary recursion, on the native stack.
//! - `manual` — a hand-written worklist, the shape you would write yourself. Its
//!   frames are on the heap too, so it isolates the cost of *leaving the stack*
//!   from the cost of the macro's particular encoding.
//! - `stack_safe` — the transform.
//!
//! The tree is deliberately shallow enough for `naive` to survive, and the work per
//! node is deliberately trivial — an index and an add. That makes this close to a
//! pure measurement of overhead, and therefore the *worst* case for the transform:
//! a workload that does real work per node dilutes the difference proportionally.
//! Read the per-call overhead rather than the ratio; the ratio only says how little
//! this particular body does.
//!
//! Release mode matters. In a debug build every number here is meaningless.

use std::alloc::{GlobalAlloc, Layout, System};
use std::hint::black_box;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::{Duration, Instant};

use yaspar_macros::stack_safe;

// ---------------------------------------------------------------------------
// A counting allocator, because "one heap allocation per recursive call" is the
// headline cost and it is worth showing rather than asserting.
// ---------------------------------------------------------------------------

static ALLOCS: AtomicU64 = AtomicU64::new(0);
static BYTES: AtomicU64 = AtomicU64::new(0);

struct Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Relaxed);
        BYTES.fetch_add(layout.size() as u64, Relaxed);
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

// ---------------------------------------------------------------------------

struct Node {
    val: u64,
    kids: Vec<usize>,
}

#[stack_safe]
fn safe(nodes: &[Node], i: usize) -> u64 {
    if nodes[i].kids.is_empty() {
        return nodes[i].val;
    }
    safe(nodes, nodes[i].kids[0]) + safe(nodes, nodes[i].kids[1])
}

fn naive(nodes: &[Node], i: usize) -> u64 {
    if nodes[i].kids.is_empty() {
        return nodes[i].val;
    }
    naive(nodes, nodes[i].kids[0]) + naive(nodes, nodes[i].kids[1])
}

/// The same traversal with an explicit worklist: heap frames, but no boxing and no
/// dynamic dispatch, because a hand-written version can name the types it stores.
fn manual(nodes: &[Node], i: usize) -> u64 {
    let mut acc = 0;
    let mut todo = vec![i];
    while let Some(n) = todo.pop() {
        if nodes[n].kids.is_empty() {
            acc += nodes[n].val;
        } else {
            todo.extend(&nodes[n].kids);
        }
    }
    acc
}

/// A complete binary tree of `2^(depth+1) - 1` nodes, built iteratively.
fn bushy(depth: u32) -> Vec<Node> {
    let mut nodes = vec![Node {
        val: 1,
        kids: Vec::new(),
    }];
    let mut frontier = vec![0usize];
    for _ in 0..depth {
        let mut next = Vec::new();
        for parent in frontier {
            for _ in 0..2 {
                let id = nodes.len();
                nodes.push(Node {
                    val: 1,
                    kids: Vec::new(),
                });
                nodes[parent].kids.push(id);
                next.push(id);
            }
        }
        frontier = next;
    }
    nodes
}

/// Best of `REPS`, which is less noisy than a mean and enough for a difference this
/// large. The allocation count is taken from the first run only.
const REPS: u32 = 5;

fn measure(name: &str, calls: u64, mut f: impl FnMut() -> u64) -> Duration {
    let mut best = Duration::MAX;
    let (mut allocs, mut bytes) = (0, 0);
    let mut answer = 0;
    for rep in 0..REPS {
        let (a0, b0) = (ALLOCS.load(Relaxed), BYTES.load(Relaxed));
        let start = Instant::now();
        answer = black_box(f());
        best = best.min(start.elapsed());
        if rep == 0 {
            allocs = ALLOCS.load(Relaxed) - a0;
            bytes = BYTES.load(Relaxed) - b0;
        }
    }
    println!(
        "{name:<12} {:>8.2} ms   {:>6.1} ns/call   {:>9} allocs ({:.2}/call)   {:>10} bytes   sum {answer}",
        best.as_secs_f64() * 1e3,
        best.as_secs_f64() * 1e9 / calls as f64,
        allocs,
        allocs as f64 / calls as f64,
        bytes,
    );
    best
}

fn main() {
    let tree = bushy(18);
    let calls = tree.len() as u64;
    println!("{calls} nodes, so that many calls\n");

    let n = measure("naive", calls, || naive(black_box(&tree), 0));
    let m = measure("manual", calls, || manual(black_box(&tree), 0));
    let s = measure("stack_safe", calls, || safe(black_box(&tree), 0));

    let (n, m, s) = (n.as_secs_f64(), m.as_secs_f64(), s.as_secs_f64());
    println!(
        "\nleaving the stack at all costs {:.1}x (manual / naive)\n\
         the macro's encoding costs a further {:.1}x (stack_safe / manual)\n\
         overhead per call: {:.0} ns",
        m / n,
        s / m,
        (s - n) * 1e9 / calls as f64,
    );
}
