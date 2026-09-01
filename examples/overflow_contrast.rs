// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Demonstrates that the depths used by the test suite really do overflow a
//! native stack. Run as:
//!
//! ```text
//! cargo run --example overflow_contrast -- safe        # prints a result
//! cargo run --example overflow_contrast -- naive       # aborts: stack overflow
//! cargo run --example overflow_contrast -- safe-loop   # n-ary, `for` loop
//! cargo run --example overflow_contrast -- naive-loop  # aborts: stack overflow
//! ```
//!
//! The `naive` modes cannot be `#[test]`s: a stack overflow is a SIGSEGV on the
//! guard page, which aborts the process rather than unwinding, so it is not
//! observable with `catch_unwind`.

use yaspar_macros::stack_safe;

// --- fixed arity: two recursive calls in one expression --------------------

#[derive(Clone, Copy)]
enum Node {
    Leaf(u64),
    Pair(usize, usize),
}

fn left_chain(depth: usize) -> Vec<Node> {
    let leaf = depth;
    let mut nodes: Vec<Node> = (0..depth).map(|i| Node::Pair(i + 1, leaf)).collect();
    nodes.push(Node::Leaf(1));
    nodes
}

#[stack_safe]
fn sum(nodes: &[Node], i: usize) -> u64 {
    match nodes[i] {
        Node::Leaf(v) => v,
        Node::Pair(l, r) => sum(nodes, l) + sum(nodes, r),
    }
}

fn sum_naive(nodes: &[Node], i: usize) -> u64 {
    match nodes[i] {
        Node::Leaf(v) => v,
        Node::Pair(l, r) => sum_naive(nodes, l) + sum_naive(nodes, r),
    }
}

// --- n-ary: the recursive call lives inside a `for` loop -------------------

fn chain(depth: usize) -> Vec<Vec<usize>> {
    let mut kids: Vec<Vec<usize>> = (0..depth).map(|i| vec![i + 1]).collect();
    kids.push(Vec::new());
    kids
}

#[stack_safe]
fn count(kids: &[Vec<usize>], i: usize) -> u64 {
    let mut acc = 1;
    for &c in kids[i].iter() {
        acc += count(kids, c);
    }
    acc
}

fn count_naive(kids: &[Vec<usize>], i: usize) -> u64 {
    let mut acc = 1;
    for &c in kids[i].iter() {
        acc += count_naive(kids, c);
    }
    acc
}

fn main() {
    let which = std::env::args().nth(1).unwrap_or_else(|| "safe".into());
    let depth = 500_000;

    std::thread::Builder::new()
        .stack_size(64 * 1024)
        .spawn(move || match which.as_str() {
            "safe" => println!("stack_safe:       sum   = {}", sum(&left_chain(depth), 0)),
            "naive" => println!(
                "naive:            sum   = {}",
                sum_naive(&left_chain(depth), 0)
            ),
            "safe-loop" => println!("stack_safe loop:  count = {}", count(&chain(depth), 0)),
            "naive-loop" => {
                println!(
                    "naive loop:       count = {}",
                    count_naive(&chain(depth), 0)
                )
            }
            other => eprintln!("unknown mode {other:?}"),
        })
        .expect("spawn")
        .join()
        .expect("join");
}
