// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Tests for recursive calls inside loops.
//!
//! A loop whose body recurses is lowered to an extra entry point into the
//! function body; one iteration is a `Tail` step carrying `(iterator, live
//! locals)`. Two properties matter and both are tested for every construct:
//!
//! 1. **Depth** costs no native stack — recursion 200 000 deep on a 64 KiB stack.
//! 2. **Iteration** costs no native stack either — a `Tail` must not push a
//!    frame, so a loop with 200 000 iterations at one recursion level must also
//!    survive. `wide_loop_is_flat` is the test that would catch a regression here.

use yaspar_macros::stack_safe;

const TINY_STACK: usize = 64 * 1024;

fn on_tiny_stack<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    std::thread::Builder::new()
        .stack_size(TINY_STACK)
        .spawn(f)
        .expect("spawn")
        .join()
        .expect("join")
}

// ---------------------------------------------------------------------------
// An n-ary arena tree. Arena-based so building and dropping deep inputs is
// itself iterative.
// ---------------------------------------------------------------------------

struct Tree {
    kids: Vec<Vec<usize>>,
    vals: Vec<u64>,
}

impl Tree {
    /// A chain of `depth` nodes, each with exactly one child. Every value is 1.
    fn chain(depth: usize) -> Tree {
        let mut kids: Vec<Vec<usize>> = (0..depth).map(|i| vec![i + 1]).collect();
        kids.push(Vec::new());
        Tree {
            vals: vec![1; depth + 1],
            kids,
        }
    }

    /// One root with `n` leaf children: deliberately shallow but very wide, to
    /// prove that iterating does not push frames.
    fn star(n: usize) -> Tree {
        let mut kids = vec![(1..=n).collect::<Vec<usize>>()];
        kids.extend((0..n).map(|_| Vec::new()));
        Tree {
            vals: vec![1; n + 1],
            kids,
        }
    }

    /// A balanced tree with the given branching factor and depth.
    fn bushy(branch: usize, depth: u32) -> Tree {
        let mut kids: Vec<Vec<usize>> = vec![Vec::new()];
        let mut frontier = vec![0usize];
        for _ in 0..depth {
            let mut next = Vec::new();
            for parent in frontier {
                for _ in 0..branch {
                    let id = kids.len();
                    kids.push(Vec::new());
                    kids[parent].push(id);
                    next.push(id);
                }
            }
            frontier = next;
        }
        let n = kids.len();
        Tree {
            vals: (0..n as u64).collect(),
            kids,
        }
    }
}

// ---------------------------------------------------------------------------
// `for` — the case the closure-only version could not express at all.
// ---------------------------------------------------------------------------

#[stack_safe]
fn sum(t: &Tree, i: usize) -> u64 {
    let mut acc = t.vals[i];
    for &c in t.kids[i].iter() {
        acc += sum(t, c);
    }
    acc
}

fn sum_naive(t: &Tree, i: usize) -> u64 {
    let mut acc = t.vals[i];
    for &c in t.kids[i].iter() {
        acc += sum_naive(t, c);
    }
    acc
}

#[test]
fn for_loop_agrees_with_naive() {
    for depth in 0..30 {
        let t = Tree::chain(depth);
        assert_eq!(sum(&t, 0), sum_naive(&t, 0), "chain {depth}");
    }
    for (branch, depth) in [(2, 5), (3, 4), (5, 3)] {
        let t = Tree::bushy(branch, depth);
        assert_eq!(sum(&t, 0), sum_naive(&t, 0), "bushy {branch}^{depth}");
    }
}

#[test]
fn deep_for_loop_recursion_is_flat() {
    let depth = 200_000;
    let got = on_tiny_stack(move || {
        let t = Tree::chain(depth);
        sum(&t, 0)
    });
    assert_eq!(got, depth as u64 + 1);
}

/// The property specific to loop lowering: many *iterations* at one recursion
/// level must not accumulate frames either.
#[test]
fn wide_loop_is_flat() {
    let n = 200_000;
    let got = on_tiny_stack(move || {
        let t = Tree::star(n);
        sum(&t, 0)
    });
    assert_eq!(got, n as u64 + 1);
}

// ---------------------------------------------------------------------------
// `break` and `continue`, both from code that recurses and code that does not.
// ---------------------------------------------------------------------------

#[stack_safe]
fn sum_until_marker(t: &Tree, i: usize) -> u64 {
    let mut acc = t.vals[i];
    for &c in t.kids[i].iter() {
        // No recursive call in this statement: `continue` is rewritten by the
        // leaf pass rather than by the CPS pass.
        if t.vals[c] % 7 == 3 {
            continue;
        }
        if t.vals[c] % 11 == 5 {
            break;
        }
        acc += sum_until_marker(t, c);
    }
    acc
}

fn sum_until_marker_naive(t: &Tree, i: usize) -> u64 {
    let mut acc = t.vals[i];
    for &c in t.kids[i].iter() {
        if t.vals[c] % 7 == 3 {
            continue;
        }
        if t.vals[c] % 11 == 5 {
            break;
        }
        acc += sum_until_marker_naive(t, c);
    }
    acc
}

#[test]
fn break_and_continue() {
    for (branch, depth) in [(2, 6), (4, 4), (7, 3)] {
        let t = Tree::bushy(branch, depth);
        assert_eq!(
            sum_until_marker(&t, 0),
            sum_until_marker_naive(&t, 0),
            "bushy {branch}^{depth}"
        );
    }
}

// ---------------------------------------------------------------------------
// `while`, and `loop` with `break <value>`.
// ---------------------------------------------------------------------------

#[stack_safe]
fn sum_while(t: &Tree, i: usize) -> u64 {
    let mut acc = t.vals[i];
    let mut n = 0;
    while n < t.kids[i].len() {
        acc += sum_while(t, t.kids[i][n]);
        n += 1;
    }
    acc
}

fn sum_while_naive(t: &Tree, i: usize) -> u64 {
    let mut acc = t.vals[i];
    let mut n = 0;
    while n < t.kids[i].len() {
        acc += sum_while_naive(t, t.kids[i][n]);
        n += 1;
    }
    acc
}

#[test]
fn while_loop() {
    let t = Tree::bushy(3, 4);
    assert_eq!(sum_while(&t, 0), sum_while_naive(&t, 0));

    let depth = 200_000;
    assert_eq!(
        on_tiny_stack(move || {
            let t = Tree::chain(depth);
            sum_while(&t, 0)
        }),
        depth as u64 + 1
    );
    let n = 200_000;
    assert_eq!(
        on_tiny_stack(move || {
            let t = Tree::star(n);
            sum_while(&t, 0)
        }),
        n as u64 + 1
    );
}

/// Deepest descendant value reachable by always taking the first child, found
/// with `loop` and `break <value>` — including a `break` in a branch with no
/// recursive call, which the leaf pass must rewrite using the continuation.
#[stack_safe]
fn first_child_depth(t: &Tree, i: usize) -> u64 {
    let mut n = 0;
    loop {
        if t.kids[i].is_empty() {
            break 0;
        }
        if n > 0 {
            break n;
        }
        n = 1 + first_child_depth(t, t.kids[i][0]);
    }
}

fn first_child_depth_naive(t: &Tree, i: usize) -> u64 {
    let mut n = 0;
    loop {
        if t.kids[i].is_empty() {
            break 0;
        }
        if n > 0 {
            break n;
        }
        n = 1 + first_child_depth_naive(t, t.kids[i][0]);
    }
}

#[test]
fn loop_with_break_value() {
    for depth in 0..20 {
        let t = Tree::chain(depth);
        assert_eq!(
            first_child_depth(&t, 0),
            first_child_depth_naive(&t, 0),
            "chain {depth}"
        );
    }
    let depth = 200_000;
    assert_eq!(
        on_tiny_stack(move || {
            let t = Tree::chain(depth);
            first_child_depth(&t, 0)
        }),
        depth as u64
    );
}

// ---------------------------------------------------------------------------
// Nested loops. Exercises the fixed-point that decides which locals each entry
// point threads: the inner loop's exhaustion branch continues into the outer
// loop's next iteration, so it must keep the outer iterator and accumulator
// alive as well as its own.
// ---------------------------------------------------------------------------

#[stack_safe]
fn nested(t: &Tree, i: usize) -> u64 {
    let mut acc = t.vals[i];
    for &c in t.kids[i].iter() {
        let mut extra = 0;
        for k in 0..2u64 {
            extra += nested(t, c) + k;
        }
        acc += extra;
    }
    acc
}

fn nested_naive(t: &Tree, i: usize) -> u64 {
    let mut acc = t.vals[i];
    for &c in t.kids[i].iter() {
        let mut extra = 0;
        for k in 0..2u64 {
            extra += nested_naive(t, c) + k;
        }
        acc += extra;
    }
    acc
}

#[test]
fn nested_loops() {
    for (branch, depth) in [(2, 4), (3, 3)] {
        let t = Tree::bushy(branch, depth);
        assert_eq!(nested(&t, 0), nested_naive(&t, 0), "bushy {branch}^{depth}");
    }
}

/// Same nesting shape as `nested`, but each child is visited once, so the work
/// is linear in the depth and the test can go deep. `nested` itself recurses on
/// the same child twice per inner iteration, which is exponential by
/// construction — fine for small trees, useless for a depth test.
#[stack_safe]
fn nested_linear(t: &Tree, i: usize) -> u64 {
    let mut acc = t.vals[i];
    for round in 0..2u64 {
        for &c in t.kids[i].iter() {
            if round == 0 {
                acc += nested_linear(t, c);
            } else {
                acc += 1;
            }
        }
    }
    acc
}

fn nested_linear_naive(t: &Tree, i: usize) -> u64 {
    let mut acc = t.vals[i];
    for round in 0..2u64 {
        for &c in t.kids[i].iter() {
            if round == 0 {
                acc += nested_linear_naive(t, c);
            } else {
                acc += 1;
            }
        }
    }
    acc
}

#[test]
fn nested_loops_agree_with_naive() {
    for (branch, depth) in [(2, 4), (3, 3), (5, 2)] {
        let t = Tree::bushy(branch, depth);
        assert_eq!(
            nested_linear(&t, 0),
            nested_linear_naive(&t, 0),
            "bushy {branch}^{depth}"
        );
    }
}

#[test]
fn nested_loops_are_flat() {
    // Deep: the inner loop must thread the outer loop's iterator so the outer
    // can resume after each recursive descent returns.
    let depth = 100_000;
    let got = on_tiny_stack(move || {
        let t = Tree::chain(depth);
        nested_linear(&t, 0)
    });
    // Each of the `depth` internal nodes contributes its own value (1) plus 1
    // for the second round; the leaf contributes 1.
    assert_eq!(got, 2 * depth as u64 + 1);

    // Wide: many iterations of a *nested* loop must not push frames either.
    let n = 100_000;
    let got = on_tiny_stack(move || {
        let t = Tree::star(n);
        nested_linear(&t, 0)
    });
    assert_eq!(got, 1 + 2 * n as u64);
}

// ---------------------------------------------------------------------------
// Loops combined with the other supported features.
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq)]
struct Overflow;

#[stack_safe]
fn sum_bounded(t: &Tree, i: usize, budget: u64) -> Result<u64, Overflow> {
    if budget == 0 {
        return Err(Overflow);
    }
    let mut acc = t.vals[i];
    for &c in t.kids[i].iter() {
        acc += sum_bounded(t, c, budget - 1)?;
    }
    Ok(acc)
}

#[test]
fn question_mark_inside_a_loop() {
    let t = Tree::chain(10);
    assert_eq!(sum_bounded(&t, 0, 100), Ok(11));
    assert_eq!(sum_bounded(&t, 0, 5), Err(Overflow));

    let depth = 200_000;
    assert_eq!(
        on_tiny_stack(move || {
            let t = Tree::chain(depth);
            sum_bounded(&t, 0, u64::MAX)
        }),
        Ok(depth as u64 + 1)
    );
    // Failing deep inside must unwind the heap stack, not the native one.
    assert_eq!(
        on_tiny_stack(move || {
            let t = Tree::chain(depth);
            sum_bounded(&t, 0, 1000)
        }),
        Err(Overflow)
    );
}

/// A loop that does *not* recurse, in a function that does. Must be left as an
/// ordinary Rust loop, with its own `break` / `continue`.
#[stack_safe]
fn mixed(t: &Tree, i: usize) -> u64 {
    let mut acc = 0;
    for v in 0..5u64 {
        if v == 3 {
            break;
        }
        if v == 1 {
            continue;
        }
        acc += v;
    }
    for &c in t.kids[i].iter() {
        acc += mixed(t, c);
    }
    acc
}

fn mixed_naive(t: &Tree, i: usize) -> u64 {
    let mut acc = 0;
    for v in 0..5u64 {
        if v == 3 {
            break;
        }
        if v == 1 {
            continue;
        }
        acc += v;
    }
    for &c in t.kids[i].iter() {
        acc += mixed_naive(t, c);
    }
    acc
}

#[test]
fn ordinary_loop_beside_a_lowered_one() {
    let t = Tree::bushy(3, 3);
    assert_eq!(mixed(&t, 0), mixed_naive(&t, 0));
}

/// A loop whose body moves a local declared before it — the state solver must
/// not try to thread a value the body has already consumed.
#[stack_safe]
fn with_moved_local(t: &Tree, i: usize) -> u64 {
    let label = String::from("consumed before the loop");
    let n = label.len() as u64;
    drop(label);
    let mut acc = t.vals[i] + n;
    for &c in t.kids[i].iter() {
        acc += with_moved_local(t, c);
    }
    acc
}

#[test]
fn local_moved_before_the_loop() {
    let t = Tree::chain(5);
    assert_eq!(with_moved_local(&t, 0), 6 * (1 + 24));
}

// ---------------------------------------------------------------------------
// Evaluation order across a loop.
// ---------------------------------------------------------------------------

use std::cell::RefCell;

#[stack_safe]
fn logged(t: &Tree, i: usize, log: &RefCell<Vec<u64>>) -> u64 {
    log.borrow_mut().push(t.vals[i]);
    let mut acc = 0;
    for &c in t.kids[i].iter() {
        acc += logged(t, c, log);
        log.borrow_mut().push(1000 + t.vals[c]);
    }
    log.borrow_mut().push(2000 + t.vals[i]);
    acc + 1
}

fn logged_naive(t: &Tree, i: usize, log: &RefCell<Vec<u64>>) -> u64 {
    log.borrow_mut().push(t.vals[i]);
    let mut acc = 0;
    for &c in t.kids[i].iter() {
        acc += logged_naive(t, c, log);
        log.borrow_mut().push(1000 + t.vals[c]);
    }
    log.borrow_mut().push(2000 + t.vals[i]);
    acc + 1
}

#[test]
fn preserves_order_around_a_loop() {
    let t = Tree::bushy(3, 3);
    let a = RefCell::new(Vec::new());
    let b = RefCell::new(Vec::new());
    assert_eq!(logged(&t, 0, &a), logged_naive(&t, 0, &b));
    assert_eq!(a.into_inner(), b.into_inner());
}

// ---------------------------------------------------------------------------
// `while let`, and the iteration half of the `loop` case. Both entry points are
// lowered the same way as `for` and `while`, so the two stack properties have to
// hold for them too.
// ---------------------------------------------------------------------------

/// Sums a subtree with `while let`, popping an explicit worklist of the current
/// node's children. The scrutinee is re-evaluated on every iteration, so the
/// worklist has to survive in the loop's entry payload.
#[stack_safe]
fn sum_while_let(t: &Tree, i: usize) -> u64 {
    let mut acc = t.vals[i];
    let mut todo: Vec<usize> = t.kids[i].clone();
    while let Some(c) = todo.pop() {
        acc += sum_while_let(t, c);
    }
    acc
}

fn sum_while_let_naive(t: &Tree, i: usize) -> u64 {
    let mut acc = t.vals[i];
    let mut todo: Vec<usize> = t.kids[i].clone();
    while let Some(c) = todo.pop() {
        acc += sum_while_let_naive(t, c);
    }
    acc
}

#[test]
fn while_let_loop() {
    for (branch, depth) in [(2, 5), (3, 4)] {
        let t = Tree::bushy(branch, depth);
        assert_eq!(sum_while_let(&t, 0), sum_while_let_naive(&t, 0));
    }

    // Depth: 200 000 levels of recursion, one iteration each.
    let depth = 200_000;
    assert_eq!(
        on_tiny_stack(move || {
            let t = Tree::chain(depth);
            sum_while_let(&t, 0)
        }),
        depth as u64 + 1
    );

    // Iteration: one level of recursion, 200 000 iterations.
    let n = 200_000;
    assert_eq!(
        on_tiny_stack(move || {
            let t = Tree::star(n);
            sum_while_let(&t, 0)
        }),
        n as u64 + 1
    );
}

/// The same worklist shape with a bare `loop` plus `break`, for the iteration
/// property: `loop_with_break_value` only ever takes one child per level, so it
/// tests depth alone.
#[stack_safe]
fn sum_loop(t: &Tree, i: usize) -> u64 {
    let mut acc = t.vals[i];
    let mut todo: Vec<usize> = t.kids[i].clone();
    loop {
        match todo.pop() {
            None => break,
            Some(c) => acc += sum_loop(t, c),
        }
    }
    acc
}

#[test]
fn wide_bare_loop_is_flat() {
    let t = Tree::bushy(3, 4);
    assert_eq!(sum_loop(&t, 0), sum_while_let_naive(&t, 0));

    let n = 200_000;
    assert_eq!(
        on_tiny_stack(move || {
            let t = Tree::star(n);
            sum_loop(&t, 0)
        }),
        n as u64 + 1
    );
    let depth = 200_000;
    assert_eq!(
        on_tiny_stack(move || {
            let t = Tree::chain(depth);
            sum_loop(&t, 0)
        }),
        depth as u64 + 1
    );
}

// ---------------------------------------------------------------------------
// A `for` loop over a borrow, with a recursive call in the body.
//
// The loop's iterator has to be parked in the entry payload, and an iterator over `&xs` borrows
// the very local the payload owns — a value that cannot be returned (E0515). Under
// `data_in_frame` the collection moves into the driver's store for the loop instead, so the
// iterator borrows the store; `Pin` never moves what it holds, and the mark travels in the
// payload so the loop releases it on the way out.
// ---------------------------------------------------------------------------

#[stack_safe(data_in_frame)]
mod borrowed_loop {
    pub fn borrowed_sum(xs: Vec<u64>, seen: &mut Vec<u64>) -> u64 {
        let mut total = 0;
        for x in &xs {
            total += borrowed_step(*x, seen);
        }
        total
    }

    pub fn borrowed_step(n: u64, seen: &mut Vec<u64>) -> u64 {
        if n == 0 {
            return 0;
        }
        seen.push(n);
        borrowed_sum(vec![n - 1], seen) + 1
    }
}

#[test]
fn borrowed_loop_is_correct() {
    let mut seen = Vec::new();
    assert_eq!(borrowed_loop::borrowed_step(3, &mut seen), 3);
    assert_eq!(seen, vec![3, 2, 1]);
}

#[test]
fn borrowed_loop_is_flat() {
    let deep = on_tiny_stack(|| {
        let mut seen = Vec::new();
        borrowed_loop::borrowed_step(200_000, &mut seen)
    });
    assert_eq!(deep, 200_000);
}

/// The store has to be released on every way out of the loop, or it grows for the rest of the
/// drive. `break` leaves through the loop's continuation and `?` through `Env::teardown`; both
/// paths are exercised here, repeatedly, so a leak would show up as memory growth rather than a
/// wrong answer.
#[stack_safe(data_in_frame)]
mod borrowed_loop_exits {
    /// Sentinels the descending chain cannot reach, so every level takes both exits without
    /// perturbing the count.
    pub const BREAK_AT: u64 = u64::MAX - 1;

    pub fn exits_scan(xs: Vec<u64>, seen: &mut Vec<u64>) -> Result<u64, u64> {
        let mut total = 0;
        for x in &xs {
            if *x == BREAK_AT {
                break;
            }
            if *x == u64::MAX {
                return Err(*x);
            }
            total += exits_probe(*x, seen)?;
        }
        Ok(total)
    }

    pub fn exits_probe(n: u64, seen: &mut Vec<u64>) -> Result<u64, u64> {
        if n == 0 {
            return Ok(0);
        }
        seen.push(n);
        Ok(exits_scan(vec![n - 1, BREAK_AT], seen)? + 1)
    }
}

/// Kept shallow so Miri can run it: this is the case that catches a teardown running *before* the
/// value it releases is read, which `return Err(*x)` does for an `x` borrowed out of the store.
#[test]
fn borrowed_loop_releases_on_break_and_question_mark() {
    let mut seen = Vec::new();
    assert_eq!(borrowed_loop_exits::exits_probe(4, &mut seen), Ok(4));
    let mut seen = Vec::new();
    assert_eq!(
        borrowed_loop_exits::exits_scan(vec![u64::MAX], &mut seen),
        Err(u64::MAX)
    );
}

#[test]
fn borrowed_loop_with_exits_is_flat() {
    // Deep enough that a store entry leaked per iteration would be obvious.
    let deep = on_tiny_stack(|| {
        let mut seen = Vec::new();
        borrowed_loop_exits::exits_probe(100_000, &mut seen)
    });
    assert_eq!(deep, Ok(100_000));
}

// ---------------------------------------------------------------------------
// A loop that follows a recursive call in the same body.
//
// The loop's state travels in its own entry variant, and the seed dispatch only ever builds the
// *members'* variants — so a loop state's type parameter was still an inference variable when the
// driver closure's own type was settled, and a closure's parameter types are settled before its
// body is checked. Nothing built inside could pin it, and the report landed on the loop's condition
// in the user's body:
//
//     error[E0282]: type annotations needed
//     while i < arms1.len() { … }
//               ^^^^^ cannot infer type
//
// The entry type is now named outside the closure, `_` standing in for any slot no annotation
// reached, which pins the rest.
// ---------------------------------------------------------------------------

struct Pair {
    kids: Vec<Pair>,
    val: u64,
}

impl Pair {
    fn chain(depth: usize) -> Pair {
        let mut node = Pair {
            kids: vec![],
            val: 1,
        };
        for _ in 0..depth {
            node = Pair {
                kids: vec![node],
                val: 1,
            };
        }
        node
    }
}

#[stack_safe]
mod call_then_loop {
    use super::Pair;

    pub fn total(acc: &mut Vec<u64>, p: &Pair, leaf: &Pair) -> u64 {
        if p.kids.is_empty() {
            acc.push(p.val);
            return p.val;
        }
        let kids: &[Pair] = &p.kids;
        // A recursive call, on a node with no children so the walk stays linear…
        if scoped(acc, vec![], leaf, leaf) == 0 {
            return 0;
        }
        // …then a loop, whose state threads `kids` across a call to the other member.
        let mut sum: u64 = 0;
        let mut i: usize = 0;
        while i < kids.len() {
            sum += scoped(acc, vec![i as u64], &kids[i], leaf);
            i += 1;
        }
        sum
    }

    pub fn scoped(acc: &mut Vec<u64>, marks: Vec<u64>, p: &Pair, leaf: &Pair) -> u64 {
        let _ = marks.len();
        total(acc, p, leaf)
    }
}

#[test]
fn loop_after_a_call_is_typed() {
    let t = Pair::chain(3);
    let mut acc = Vec::new();
    // 3 inner levels each add a `leaf` push plus the base case.
    let leaf = Pair {
        kids: vec![],
        val: 1,
    };
    assert_eq!(call_then_loop::total(&mut acc, &t, &leaf), 1);
}

#[test]
fn loop_after_a_call_is_flat() {
    let deep = on_tiny_stack(|| {
        let t = Pair::chain(200_000);
        let mut acc = Vec::new();
        let leaf = Pair {
            kids: vec![],
            val: 1,
        };
        let n = call_then_loop::total(&mut acc, &t, &leaf);
        // Dropping a 200k-deep `Pair` is itself recursive, so it is leaked rather than dropped.
        std::mem::forget(t);
        n
    });
    assert_eq!(deep, 1);
}
