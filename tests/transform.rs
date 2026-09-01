// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Behavioural tests for `#[stack_safe]`.
//!
//! The stack-safety tests all run the transformed function on a thread with a
//! deliberately tiny stack (64 KiB). If the expansion were still recursing on
//! the native stack, they would abort the test process rather than fail — so a
//! passing run really does mean no native frame per level of recursion.

use std::cell::RefCell;
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
// An arena tree, so that building and dropping deep test inputs is itself
// iterative. A `Box`-based tree would overflow the stack in `Drop` before the
// test even ran.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum Node {
    Leaf(u64),
    Pair(usize, usize),
}

/// `nodes[0]` is the root of a left-leaning chain of `depth` `Pair`s, each with
/// a `Leaf(1)` on the right. Its sum is `depth + 1`.
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

#[test]
fn agrees_with_naive_on_shallow_trees() {
    for depth in 0..40 {
        let nodes = left_chain(depth);
        assert_eq!(sum(&nodes, 0), sum_naive(&nodes, 0), "depth {depth}");
    }
}

#[test]
fn survives_depth_that_would_overflow() {
    let depth = 500_000;
    let got = on_tiny_stack(move || {
        let nodes = left_chain(depth);
        sum(&nodes, 0)
    });
    assert_eq!(got, depth as u64 + 1);
}

// ---------------------------------------------------------------------------
// Two recursive calls inside one expression: exercises left-to-right hoisting.
// ---------------------------------------------------------------------------

#[stack_safe]
fn fib(n: u64) -> u64 {
    if n < 2 { n } else { fib(n - 1) + fib(n - 2) }
}

fn fib_naive(n: u64) -> u64 {
    if n < 2 {
        n
    } else {
        fib_naive(n - 1) + fib_naive(n - 2)
    }
}

#[test]
fn nested_calls_in_one_expression() {
    for n in 0..=22 {
        assert_eq!(fib(n), fib_naive(n), "n = {n}");
    }
}

// A recursive call as the argument of another recursive call.
#[stack_safe]
fn ackermann_ish(m: u64, n: u64) -> u64 {
    if m == 0 {
        n + 1
    } else if n == 0 {
        ackermann_ish(m - 1, 1)
    } else {
        ackermann_ish(m - 1, ackermann_ish(m, n - 1))
    }
}

fn ackermann_ish_naive(m: u64, n: u64) -> u64 {
    if m == 0 {
        n + 1
    } else if n == 0 {
        ackermann_ish_naive(m - 1, 1)
    } else {
        ackermann_ish_naive(m - 1, ackermann_ish_naive(m, n - 1))
    }
}

#[test]
fn recursive_call_as_argument_of_recursive_call() {
    for m in 0..=2 {
        for n in 0..=5 {
            assert_eq!(ackermann_ish(m, n), ackermann_ish_naive(m, n), "({m}, {n})");
        }
    }
}

// ---------------------------------------------------------------------------
// `?` inside a continuation, and early `return Err`.
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq)]
struct TooDeep;

#[stack_safe]
fn sum_bounded(nodes: &[Node], i: usize, budget: u64) -> Result<u64, TooDeep> {
    if budget == 0 {
        return Err(TooDeep);
    }
    match nodes[i] {
        Node::Leaf(v) => Ok(v),
        Node::Pair(l, r) => {
            let a = sum_bounded(nodes, l, budget - 1)?;
            let b = sum_bounded(nodes, r, budget - 1)?;
            Ok(a + b)
        }
    }
}

#[test]
fn question_mark_propagates_through_continuations() {
    let nodes = left_chain(10);
    assert_eq!(sum_bounded(&nodes, 0, 100), Ok(11));
    assert_eq!(sum_bounded(&nodes, 0, 5), Err(TooDeep));
}

#[test]
fn question_mark_is_stack_safe() {
    let depth = 200_000;
    let got = on_tiny_stack(move || {
        let nodes = left_chain(depth);
        sum_bounded(&nodes, 0, u64::MAX)
    });
    assert_eq!(got, Ok(depth as u64 + 1));
}

// ---------------------------------------------------------------------------
// `?` on an `Option`, i.e. a carrier whose residual holds nothing.
// ---------------------------------------------------------------------------

#[stack_safe]
fn first_none(xs: &[Option<u64>], i: usize) -> Option<u64> {
    if i >= xs.len() {
        return Some(0);
    }
    let head = xs[i]?;
    Some(head + first_none(xs, i + 1)?)
}

fn first_none_naive(xs: &[Option<u64>], i: usize) -> Option<u64> {
    if i >= xs.len() {
        return Some(0);
    }
    let head = xs[i]?;
    Some(head + first_none_naive(xs, i + 1)?)
}

#[test]
fn question_mark_works_on_option() {
    for xs in [
        vec![Some(1), Some(2), Some(3)],
        vec![Some(1), None, Some(3)],
        vec![None],
        vec![],
    ] {
        assert_eq!(first_none(&xs, 0), first_none_naive(&xs, 0), "{xs:?}");
    }
}

#[test]
fn question_mark_on_option_is_stack_safe() {
    let depth = 200_000;
    let got = on_tiny_stack(move || {
        let xs: Vec<Option<u64>> = (0..depth).map(Some).collect();
        first_none(&xs, 0)
    });
    assert_eq!(got, Some((0..depth).sum()));
}

// The `Err` path still widens through `From`, which the `Option` impl must not
// have displaced.

#[derive(Debug, PartialEq)]
struct Wide(&'static str);

impl From<TooDeep> for Wide {
    fn from(_: TooDeep) -> Self {
        Wide("too deep")
    }
}

#[stack_safe]
fn widened(nodes: &[Node], i: usize, budget: u64) -> Result<u64, Wide> {
    if budget == 0 {
        return Err(TooDeep)?;
    }
    match nodes[i] {
        Node::Leaf(v) => Ok(v),
        Node::Pair(l, r) => Ok(widened(nodes, l, budget - 1)? + widened(nodes, r, budget - 1)?),
    }
}

#[test]
fn question_mark_still_widens_the_error() {
    let nodes = left_chain(10);
    assert_eq!(widened(&nodes, 0, 100), Ok(11));
    assert_eq!(widened(&nodes, 0, 5), Err(Wide("too deep")));
}

#[test]
fn early_error_unwinds_the_heap_stack() {
    let depth = 200_000;
    let got = on_tiny_stack(move || {
        let nodes = left_chain(depth);
        sum_bounded(&nodes, 0, 1000)
    });
    assert_eq!(got, Err(TooDeep));
}

// ---------------------------------------------------------------------------
// Evaluation order must survive hoisting: a side effect written before a
// recursive call must still happen before it.
// ---------------------------------------------------------------------------

fn note(log: &RefCell<Vec<u64>>, v: u64) -> u64 {
    log.borrow_mut().push(v);
    0
}

#[stack_safe]
fn ordered(n: u64, log: &RefCell<Vec<u64>>) -> u64 {
    if n == 0 {
        return 0;
    }
    note(log, n) + ordered(n - 1, log) + note(log, 100 + n)
}

fn ordered_naive(n: u64, log: &RefCell<Vec<u64>>) -> u64 {
    if n == 0 {
        return 0;
    }
    note(log, n) + ordered_naive(n - 1, log) + note(log, 100 + n)
}

#[test]
fn preserves_evaluation_order() {
    let a = RefCell::new(Vec::new());
    let b = RefCell::new(Vec::new());
    assert_eq!(ordered(6, &a), ordered_naive(6, &b));
    assert_eq!(a.into_inner(), b.into_inner());
}

// ---------------------------------------------------------------------------
// Short-circuit operators, `let` bindings, nested blocks, unit return.
// ---------------------------------------------------------------------------

#[stack_safe]
fn all_leaves_nonzero(nodes: &[Node], i: usize) -> bool {
    match nodes[i] {
        Node::Leaf(v) => v != 0,
        Node::Pair(l, r) => all_leaves_nonzero(nodes, l) && all_leaves_nonzero(nodes, r),
    }
}

#[test]
fn short_circuit_and_is_lazy_and_stack_safe() {
    let depth = 200_000;
    assert!(on_tiny_stack(move || {
        let nodes = left_chain(depth);
        all_leaves_nonzero(&nodes, 0)
    }));

    // A zero leaf reached first must stop the walk.
    let mut nodes = left_chain(4);
    nodes[4] = Node::Leaf(0);
    assert!(!all_leaves_nonzero(&nodes, 0));
}

#[stack_safe]
fn depth_of(nodes: &[Node], i: usize) -> usize {
    match nodes[i] {
        Node::Leaf(_) => 0,
        Node::Pair(l, r) => {
            let dl = depth_of(nodes, l);
            let dr = {
                let inner = depth_of(nodes, r);
                inner
            };
            1 + if dl > dr { dl } else { dr }
        }
    }
}

#[test]
fn let_bindings_and_nested_blocks() {
    let nodes = left_chain(12);
    assert_eq!(depth_of(&nodes, 0), 12);
    let depth = 200_000;
    assert_eq!(
        on_tiny_stack(move || {
            let nodes = left_chain(depth);
            depth_of(&nodes, 0)
        }),
        depth
    );
}

#[stack_safe]
fn count_into(nodes: &[Node], i: usize, out: &RefCell<u64>) {
    match nodes[i] {
        Node::Leaf(_) => *out.borrow_mut() += 1,
        Node::Pair(l, r) => {
            count_into(nodes, l, out);
            count_into(nodes, r, out);
        }
    }
}

#[test]
fn unit_return_type() {
    let nodes = left_chain(10);
    let out = RefCell::new(0);
    count_into(&nodes, 0, &out);
    assert_eq!(out.into_inner(), 11);
}

// ---------------------------------------------------------------------------
// Generics and where-clauses pass through untouched.
// ---------------------------------------------------------------------------

#[stack_safe]
fn fold_chain<T>(nodes: &[Node], i: usize, seed: T) -> T
where
    T: std::ops::Add<u64, Output = T>,
{
    match nodes[i] {
        Node::Leaf(v) => seed + v,
        Node::Pair(l, r) => {
            let acc = fold_chain(nodes, l, seed);
            fold_chain(nodes, r, acc)
        }
    }
}

#[test]
fn generic_function() {
    let nodes = left_chain(9);
    assert_eq!(fold_chain(&nodes, 0, 0u64), 10);
}

/// A method call whose receiver is the *result* of a recursive call. The receiver
/// lands in a continuation, so its type has to be named there — left to inference
/// it is `{integer}` and method resolution fails with E0689.
#[test]
fn method_call_on_a_recursive_result() {
    #[stack_safe]
    fn f(n: u64) -> u64 {
        if n == 0 { 0 } else { f(n - 1).wrapping_add(1) }
    }
    assert_eq!(f(5), 5);
    assert_eq!(on_tiny_stack(|| f(200_000)), 200_000);
}

/// `impl Trait` in return position. Plain Rust rejects a *recursive* function with
/// an opaque return type; after the transform there is no recursion left, so it
/// compiles — but the type cannot be named, so the driver's result type stays
/// inferred.
#[test]
fn impl_trait_return() {
    #[stack_safe]
    fn f(n: u64) -> impl std::fmt::Debug {
        if n == 0 { 0u64 } else { f(n - 1) }
    }
    assert_eq!(format!("{:?}", f(3)), "0");
}

/// A struct literal that has both a recursive field and a functional-update base.
/// The fields already emit trailing commas, so the `..base` must not add another —
/// it used to, producing `S { v: x, , ..b }` and a bare "expected identifier"
/// parse error from the expansion.
#[test]
fn struct_literal_with_functional_update() {
    #[derive(Clone)]
    struct S {
        v: u64,
        w: u64,
    }

    #[stack_safe]
    fn f(n: u64) -> u64 {
        let base = S { v: 0, w: 7 };
        if n == 0 {
            return 0;
        }
        let s = S {
            v: f(n - 1) + 1,
            ..base
        };
        s.v + s.w
    }

    fn naive(n: u64) -> u64 {
        let base = S { v: 0, w: 7 };
        if n == 0 {
            return 0;
        }
        let s = S {
            v: naive(n - 1) + 1,
            ..base
        };
        s.v + s.w
    }

    for n in 0..8 {
        assert_eq!(f(n), naive(n), "n = {n}");
    }
}

/// A recursive call whose result is still live across a *later* loop that also
/// recurses. The result binding is created by a generated continuation, not by the
/// user's code, so no `Env` knows it is in scope — but the loop's entry point still
/// has to carry it. Before `Ctx::with_result` this leaked `cannot find value
/// `__ss_v0`` out of the expansion.
#[test]
fn recursive_result_live_across_a_later_loop() {
    #[stack_safe]
    fn f(n: u64) -> u64 {
        if n == 0 {
            return 0;
        }
        f(n - 1) + {
            let mut a = 0;
            for i in 0..(n / 2) {
                a += f(i);
            }
            a
        }
    }

    fn naive(n: u64) -> u64 {
        if n == 0 {
            return 0;
        }
        naive(n - 1) + {
            let mut a = 0;
            for i in 0..(n / 2) {
                a += naive(i);
            }
            a
        }
    }

    for n in 0..14 {
        assert_eq!(f(n), naive(n), "n = {n}");
    }
    // Still flat: the value rides in the loop's entry payload, not on the stack.
    assert_eq!(on_tiny_stack(|| f(1)), 0);
    assert_eq!(on_tiny_stack(|| f(2)), naive(2));
}

/// A closure that does not recurse is ordinary code, and is spliced through
/// untouched — in leaf code, inside a continuation, and inside a lowered loop where
/// it captures a loop-carried local. Only a *recursive call* inside a closure is
/// rejected (the closure is invoked by code the macro cannot see), and the two must
/// not be confused: `contains_rec` looking inside closures is what tells them apart.
#[test]
fn closures_without_recursion_are_untouched() {
    #[stack_safe]
    fn f(n: u64, v: &[u64]) -> u64 {
        // Leaf code.
        let mapped: u64 = v.iter().map(|x| x + 1).sum();
        if n == 0 {
            return mapped;
        }
        let rest = f(n - 1, v);
        // Inside a continuation, after a recursive call.
        let counted = v.iter().filter(|x| **x > 0).count() as u64;
        // Inside a lowered loop, capturing a local the loop carries.
        let mut acc = 0;
        for i in 0..2u64 {
            let bump = |y: u64| y + acc + i;
            acc += bump(1) + f(0, v);
        }
        mapped + rest + counted + acc
    }

    fn naive(n: u64, v: &[u64]) -> u64 {
        let mapped: u64 = v.iter().map(|x| x + 1).sum();
        if n == 0 {
            return mapped;
        }
        let rest = naive(n - 1, v);
        let counted = v.iter().filter(|x| **x > 0).count() as u64;
        let mut acc = 0;
        for i in 0..2u64 {
            let bump = |y: u64| y + acc + i;
            acc += bump(1) + naive(0, v);
        }
        mapped + rest + counted + acc
    }

    for n in 0..6 {
        assert_eq!(f(n, &[1, 2, 3]), naive(n, &[1, 2, 3]), "n = {n}");
    }
}

/// A name used only from *inside a string literal* — Rust's implicit format capture,
/// `format!("{n}")`. The payload solver decides what a frame carries by scanning the
/// generated tokens for identifiers, and such a name is not a token, so it used to be
/// missed. For a local that was a loud `cannot find value`; for a *parameter* it
/// silently resolved to the enclosing function's own parameter — the outermost call's
/// argument — and produced a wrong answer with no diagnostic at all.
#[test]
fn implicit_format_captures_are_carried() {
    use std::cell::RefCell;

    #[stack_safe]
    fn f(n: u64, log: &RefCell<Vec<String>>) -> u64 {
        if n == 0 {
            return 0;
        }
        let width = (n as usize) + 1;
        let label = format!("L{n}");
        let r = f(n - 1, log);
        // Bare capture, one with a spec, a width taken from a binding, an escaped
        // brace, a positional argument, and a captured local.
        log.borrow_mut()
            .push(format!("{n} {n:?} {n:>width$} {{esc}} {0} {label}", 7));
        r + 1
    }

    fn naive(n: u64, log: &RefCell<Vec<String>>) -> u64 {
        if n == 0 {
            return 0;
        }
        let width = (n as usize) + 1;
        let label = format!("L{n}");
        let r = naive(n - 1, log);
        log.borrow_mut()
            .push(format!("{n} {n:?} {n:>width$} {{esc}} {0} {label}", 7));
        r + 1
    }

    let (a, b) = (RefCell::new(Vec::new()), RefCell::new(Vec::new()));
    assert_eq!(f(4, &a), naive(4, &b));
    assert_eq!(*a.borrow(), *b.borrow());
    // The distinguishing symptom: every entry would read the outermost `n`.
    assert!(a.borrow()[0].starts_with("1 1"), "{:?}", a.borrow());
}

/// A `return` in one branch of an `if` that precedes a loop. The continuation is
/// duplicated into both branches, so the loop used to be lowered twice — and the copy
/// in the diverging branch was unreachable, which left its payload type with nothing
/// to pin it. rustc does not infer through dead code, so the user got
/// `type annotations needed` pointing into their own body. A diverging statement now
/// ends the continuation instead of generating one.
#[test]
fn a_diverging_branch_generates_no_continuation() {
    struct Tree {
        v: u64,
        kids: Vec<Tree>,
    }

    #[stack_safe]
    fn f(n: u64, t: &Tree) -> u64 {
        if n > 0 {
            let a = f(n - 1, t);
            return a + t.v * 1000;
        }
        let mut acc = t.v;
        for i in 0..t.kids.len() {
            acc += f(0, &t.kids[i]);
        }
        acc
    }

    fn naive(n: u64, t: &Tree) -> u64 {
        if n > 0 {
            let a = naive(n - 1, t);
            return a + t.v * 1000;
        }
        let mut acc = t.v;
        for i in 0..t.kids.len() {
            acc += naive(0, &t.kids[i]);
        }
        acc
    }

    let t = Tree {
        v: 3,
        kids: vec![
            Tree {
                v: 5,
                kids: Vec::new(),
            },
            Tree {
                v: 7,
                kids: Vec::new(),
            },
        ],
    };
    for n in 0..4 {
        assert_eq!(f(n, &t), naive(n, &t), "n = {n}");
    }
}

// ---------------------------------------------------------------------------
// A method recursing on a receiver other than `self`. A `&self` receiver is `Copy`,
// so it travels in the argument payload like any other `&T` and the callee may be a
// different value of the same type — here the tail of a list.
// ---------------------------------------------------------------------------

enum Stack<'a, T> {
    Nil,
    Cons(T, &'a Stack<'a, T>),
}

impl<T> Stack<'_, T> {
    #[stack_safe]
    fn len(&self) -> usize {
        match self {
            Stack::Nil => 0,
            Stack::Cons(_, tail) => 1 + tail.len(),
        }
    }
}

fn len_naive<T>(s: &Stack<'_, T>) -> usize {
    match s {
        Stack::Nil => 0,
        Stack::Cons(_, tail) => 1 + len_naive(tail),
    }
}

#[stack_safe(data_in_frame)]
fn rec(n: usize, stack: &Stack<'_, Vec<usize>>) -> usize {
    if stack.len() >= n {
        n
    } else {
        let v = vec![];
        1 + rec(n, &Stack::Cons(v, stack))
    }
}

fn rec_naive(n: usize, stack: &Stack<'_, Vec<usize>>) -> usize {
    if stack.len() >= n {
        n
    } else {
        let v = vec![];
        1 + rec_naive(n, &Stack::Cons(v, stack))
    }
}

#[test]
fn lends_the_callee_a_value_built_here() {
    for n in 0..8 {
        assert_eq!(rec(n, &Stack::Nil), rec_naive(n, &Stack::Nil), "n = {n}");
    }
}

#[test]
fn lending_a_built_value_is_stack_safe() {
    // `stack.len()` walks the chain at every level, so the work is quadratic; a few
    // thousand is far past what the 64 KiB stack survives natively.
    let depth = 5_000;
    let got = on_tiny_stack(move || rec(depth, &Stack::Nil));
    assert_eq!(got, 2 * depth);
}

#[test]
fn method_recursing_on_another_receiver() {
    let nil = Stack::Nil;
    let one = Stack::Cons(1u64, &nil);
    let two = Stack::Cons(2u64, &one);
    for s in [&nil, &one, &two] {
        assert_eq!(s.len(), len_naive(s));
    }
    assert_eq!(two.len(), 2);
}

// Two values grown in the *same* recursive call, of two different types. Each pinned
// position gets its own store, since a store holds one inferred element type; with a
// single store the two collided with an `E0308` blamed on the attribute.

enum Flags<'a> {
    Nil,
    Cons(#[allow(dead_code)] bool, &'a Flags<'a>),
}

fn flag_depth(f: &Flags<'_>) -> usize {
    let (mut n, mut cur) = (0, f);
    while let Flags::Cons(_, tail) = cur {
        n += 1;
        cur = tail;
    }
    n
}

#[stack_safe(data_in_frame)]
fn two_stacks(n: usize, xs: &Stack<'_, Vec<usize>>, fs: &Flags<'_>) -> usize {
    if xs.len() >= n {
        xs.len() + flag_depth(fs)
    } else {
        let v = vec![];
        two_stacks(n, &Stack::Cons(v, xs), &Flags::Cons(true, fs))
    }
}

fn two_stacks_naive(n: usize, xs: &Stack<'_, Vec<usize>>, fs: &Flags<'_>) -> usize {
    if xs.len() >= n {
        xs.len() + flag_depth(fs)
    } else {
        let v = vec![];
        two_stacks_naive(n, &Stack::Cons(v, xs), &Flags::Cons(true, fs))
    }
}

#[test]
fn two_values_of_different_types_lent_in_one_call() {
    for n in 0..6 {
        assert_eq!(
            two_stacks(n, &Stack::Nil, &Flags::Nil),
            two_stacks_naive(n, &Stack::Nil, &Flags::Nil),
            "n = {n}"
        );
    }
}

#[test]
fn two_lent_values_are_stack_safe() {
    let depth = 2_000;
    let got = on_tiny_stack(move || two_stacks(depth, &Stack::Nil, &Flags::Nil));
    assert_eq!(got, 2 * depth);
}

// Three or more lent values, and call sites that grow different subsets of them. Each
// position has its own store, so their types need not agree; a site that grows only
// some of them takes a mark for only those stores.

enum Chars<'a> {
    Nil,
    Cons(#[allow(dead_code)] char, &'a Chars<'a>),
}

fn char_depth(c: &Chars<'_>) -> usize {
    let (mut n, mut cur) = (0, c);
    while let Chars::Cons(_, tail) = cur {
        n += 1;
        cur = tail;
    }
    n
}

#[stack_safe(data_in_frame)]
fn three(n: usize, xs: &Stack<'_, Vec<usize>>, fs: &Flags<'_>, cs: &Chars<'_>) -> usize {
    if xs.len() >= n {
        xs.len() + flag_depth(fs) + char_depth(cs)
    } else if xs.len().is_multiple_of(2) {
        // Grows all three.
        three(
            n,
            &Stack::Cons(vec![], xs),
            &Flags::Cons(true, fs),
            &Chars::Cons('x', cs),
        )
    } else {
        // Grows only the first, and passes the others along.
        three(n, &Stack::Cons(vec![], xs), fs, cs)
    }
}

fn three_naive(n: usize, xs: &Stack<'_, Vec<usize>>, fs: &Flags<'_>, cs: &Chars<'_>) -> usize {
    if xs.len() >= n {
        xs.len() + flag_depth(fs) + char_depth(cs)
    } else if xs.len().is_multiple_of(2) {
        three_naive(
            n,
            &Stack::Cons(vec![], xs),
            &Flags::Cons(true, fs),
            &Chars::Cons('x', cs),
        )
    } else {
        three_naive(n, &Stack::Cons(vec![], xs), fs, cs)
    }
}

#[test]
fn three_lent_values_with_differing_subsets() {
    for n in 0..8 {
        assert_eq!(
            three(n, &Stack::Nil, &Flags::Nil, &Chars::Nil),
            three_naive(n, &Stack::Nil, &Flags::Nil, &Chars::Nil),
            "n = {n}"
        );
    }
}

#[test]
fn three_lent_values_are_stack_safe() {
    let depth = 2_000;
    let got = on_tiny_stack(move || three(depth, &Stack::Nil, &Flags::Nil, &Chars::Nil));
    assert_eq!(got, depth + 2 * (depth / 2));
}

// ---------------------------------------------------------------------------
// A method call whose receiver is mutated and whose argument recurses. The receiver is
// evaluated before the argument, so it has to survive the call — but it is a *place*,
// and hoisting it by value would mutate a copy and let the original answer afterwards.
// With a `Copy` receiver that was silent: every depth returned 1.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq)]
struct Acc(u64);

impl Acc {
    fn add(&mut self, v: u64) {
        self.0 += v;
    }
}

#[stack_safe]
fn copy_receiver(n: u64) -> u64 {
    if n == 0 {
        return 0;
    }
    let mut acc = Acc(1);
    acc.add(copy_receiver(n - 1));
    acc.0
}

fn copy_receiver_naive(n: u64) -> u64 {
    if n == 0 {
        return 0;
    }
    let mut acc = Acc(1);
    acc.add(copy_receiver_naive(n - 1));
    acc.0
}

#[stack_safe]
fn owned_receiver(n: u64) -> usize {
    if n == 0 {
        return 0;
    }
    let mut out = Vec::new();
    out.push(owned_receiver(n - 1));
    out[0] + 1
}

// Written the long way on purpose: the point is a `push` whose argument recurses.
#[allow(clippy::vec_init_then_push)]
fn owned_receiver_naive(n: u64) -> usize {
    if n == 0 {
        return 0;
    }
    let mut out = Vec::new();
    out.push(owned_receiver_naive(n - 1));
    out[0] + 1
}

#[test]
fn a_mutated_receiver_is_not_hoisted_by_value() {
    for n in 0..10 {
        assert_eq!(
            copy_receiver(n),
            copy_receiver_naive(n),
            "copy_receiver({n})"
        );
        assert_eq!(copy_receiver(n), n, "copy_receiver({n}) counts every level");
        assert_eq!(
            owned_receiver(n),
            owned_receiver_naive(n),
            "owned_receiver({n})"
        );
    }
}

#[test]
fn a_mutated_receiver_is_stack_safe() {
    let depth = 200_000;
    let got = on_tiny_stack(move || (copy_receiver(depth), owned_receiver(depth)));
    assert_eq!(got, (depth, depth as usize));
}

// An alias hiding a reference needs its lifetime written only where a *seed* carries it,
// i.e. in a lifted group. A lone function has no seed, so the bare alias is fine.

type Bare<'a> = &'a [&'a str];

#[stack_safe]
fn bare_alias(w: Bare, i: usize) -> usize {
    if i >= w.len() {
        0
    } else {
        1 + bare_alias(w, i + 1)
    }
}

fn bare_alias_naive(w: Bare, i: usize) -> usize {
    if i >= w.len() {
        0
    } else {
        1 + bare_alias_naive(w, i + 1)
    }
}

#[test]
fn a_bare_reference_alias_works_outside_a_group() {
    let words = ["a", "b", "c", "d", "e"];
    for i in 0..=words.len() {
        assert_eq!(bare_alias(&words, i), bare_alias_naive(&words, i), "i {i}");
    }
}

#[test]
fn a_bare_reference_alias_is_stack_safe() {
    let depth = 200_000;
    let got = on_tiny_stack(move || {
        let words: Vec<&str> = vec!["w"; depth];
        bare_alias(&words, 0)
    });
    assert_eq!(got, depth);
}

// ---------------------------------------------------------------------------
// A body is a scope of item definitions, so a `fn` nested in one recurses just as a free
// function does: alone, through the function hosting it, or through its siblings. Such a
// function used to be left as written, and so still consumed the native stack.
// ---------------------------------------------------------------------------

#[stack_safe]
fn hosts_a_recursion(n: u64) -> u64 {
    fn inner(n: u64) -> u64 {
        if n == 0 { 0 } else { 1 + inner(n - 1) }
    }
    inner(n)
}

#[stack_safe]
fn recurses_at_every_level(n: u64) -> u64 {
    fn mid(n: u64) -> u64 {
        fn deep(n: u64) -> u64 {
            if n == 0 { 0 } else { 1 + deep(n - 1) }
        }
        if n == 0 { deep(2) } else { 1 + mid(n - 1) }
    }
    if n == 0 {
        mid(3)
    } else {
        recurses_at_every_level(n - 1)
    }
}

/// A helper beside a recursion, which recurses not at all and must be left alone.
#[stack_safe]
fn keeps_its_helper(n: u64) -> u64 {
    fn double(n: u64) -> u64 {
        n * 2
    }
    if n == 0 {
        0
    } else {
        double(1) + keeps_its_helper(n - 1)
    }
}

#[test]
fn a_nested_recursion_is_rewritten() {
    for n in 0..8 {
        assert_eq!(hosts_a_recursion(n), n, "hosts_a_recursion({n})");
        assert_eq!(keeps_its_helper(n), 2 * n, "keeps_its_helper({n})");
    }
    // `mid(3)` counts three levels and bottoms out in `deep(2)`, which counts two.
    assert_eq!(recurses_at_every_level(0), 5);
    assert_eq!(recurses_at_every_level(4), 5);
}

#[test]
fn a_nested_recursion_is_stack_safe() {
    let depth = 200_000;
    // The outer function does not recurse at all here: the nested one is the whole point,
    // and before it was scanned this overflowed.
    assert_eq!(on_tiny_stack(move || hosts_a_recursion(depth)), depth);
    assert_eq!(on_tiny_stack(move || keeps_its_helper(depth)), 2 * depth);
    assert_eq!(on_tiny_stack(move || recurses_at_every_level(depth)), 5);
}

// A cycle that crosses the level: `outer` calls `inner` and `inner` calls `outer`, so the
// two share one driver, written beside `outer` rather than inside the body that has become
// one of its arms.

#[stack_safe]
fn crosses_the_level(n: u64) -> u64 {
    fn inner(n: u64) -> u64 {
        if n == 0 {
            0
        } else {
            1 + crosses_the_level(n - 1)
        }
    }
    if n == 0 { 0 } else { 1 + inner(n - 1) }
}

// Three nested siblings in a cycle of their own, beside an item that is not in it. The host
// recurses too, so its driver and theirs are separate.

#[stack_safe]
fn hosts_a_cycle(n: u64) -> u64 {
    fn double(n: u64) -> u64 {
        n * 2
    }
    fn bar1(n: u64) -> u64 {
        if n == 0 { 0 } else { bar2(n - 1) + 1 }
    }
    fn bar2(n: u64) -> u64 {
        if n == 0 { 0 } else { bar3(n - 1) + 1 }
    }
    fn bar3(n: u64) -> u64 {
        if n == 0 {
            0
        } else {
            bar1(n - 1) + double(0) + 1
        }
    }
    if n == 0 {
        0
    } else {
        bar1(3) + hosts_a_cycle(n - 1)
    }
}

// A member of the cycle calling an item declared beside it: the arms are written together,
// so what one body declares has to stay in scope for all of them.

#[stack_safe]
fn keeps_a_helper_in_scope(n: u64) -> u64 {
    fn help(n: u64) -> u64 {
        n + 1
    }
    fn inner(n: u64) -> u64 {
        if n == 0 {
            0
        } else {
            help(0) + keeps_a_helper_in_scope(n - 1)
        }
    }
    if n == 0 { 0 } else { inner(n - 1) + 1 }
}

// The two sides of a crossing cycle need not agree on their return type: they answer
// through one driver, which answers with a union of the two.

#[stack_safe]
fn is_even_across_levels(n: u64) -> bool {
    fn count(n: u64) -> u64 {
        if n == 0 {
            0
        } else if is_even_across_levels(n - 1) {
            1
        } else {
            0
        }
    }
    if n == 0 { true } else { count(n) == 1 }
}

// A nested function carrying the same name shadows the host, so the host's own name inside
// its body is no longer a call to itself. Read as one, the two would drive each other.

#[stack_safe]
fn shadowed(n: u64) -> u64 {
    fn shadowed(n: u64) -> u64 {
        if n == 0 { 0 } else { 1 + shadowed(n - 1) }
    }
    shadowed(n) + 1
}

// Options are the scope's, and one written on a nested function joins them. The attribute
// itself is taken off: this pass has already covered that function.

#[stack_safe]
fn hosts_an_opted_in_recursion(t: &mut Counter) -> u64 {
    // Written out rather than imported: a marker is recognised by name, since a macro resolves no
    // paths, and this crate's own path is one of the two spellings it knows.
    #[yaspar_macros::stack_safe(use_nonlinear_mut)]
    fn bump(t: &mut Counter) -> u64 {
        let mut total = t.v;
        for i in 0..t.kids.len() {
            total += bump(&mut t.kids[i]);
        }
        total
    }
    bump(t)
}

struct Counter {
    v: u64,
    kids: Vec<Counter>,
}

fn chain(depth: usize) -> Counter {
    let mut t = Counter {
        v: 1,
        kids: Vec::new(),
    };
    for _ in 0..depth {
        t = Counter {
            v: 1,
            kids: vec![t],
        };
    }
    t
}

#[test]
fn a_cycle_through_the_host_is_rewritten() {
    for n in 0..8 {
        assert_eq!(crosses_the_level(n), n, "crosses_the_level({n})");
        assert_eq!(
            keeps_a_helper_in_scope(n),
            n,
            "keeps_a_helper_in_scope({n})"
        );
        assert!(is_even_across_levels(n), "is_even_across_levels({n})");
        assert_eq!(shadowed(n), n + 1, "shadowed({n})");
    }
    // `bar1(3)` counts three levels per level of the host.
    for n in 0..5 {
        assert_eq!(hosts_a_cycle(n), 3 * n, "hosts_a_cycle({n})");
    }
}

#[test]
fn a_cycle_through_the_host_is_stack_safe() {
    let depth = 200_000;
    assert_eq!(on_tiny_stack(move || crosses_the_level(depth)), depth);
    assert_eq!(on_tiny_stack(move || hosts_a_cycle(depth)), 3 * depth);
    assert_eq!(on_tiny_stack(move || keeps_a_helper_in_scope(depth)), depth);
    assert!(on_tiny_stack(move || is_even_across_levels(depth)));
    assert_eq!(on_tiny_stack(move || shadowed(depth)), depth + 1);
}

/// `Counter`'s own `Drop` recurses, which would overflow the tiny stack long before the
/// transformed function did. Taking it apart iteratively keeps the teardown out of the
/// measurement — the chain is not what is under test.
fn drop_iteratively(t: Counter) {
    let mut stack = vec![t];
    while let Some(mut node) = stack.pop() {
        stack.append(&mut node.kids);
    }
}

#[test]
fn a_nested_recursion_keeps_its_own_options() {
    let mut t = chain(3);
    assert_eq!(hosts_an_opted_in_recursion(&mut t), 4);
    drop_iteratively(t);

    let depth = 100_000;
    let got = on_tiny_stack(move || {
        let mut t = chain(depth);
        let out = hosts_an_opted_in_recursion(&mut t);
        drop_iteratively(t);
        out
    });
    assert_eq!(got, depth as u64 + 1);
}

// A cycle running through three levels at once, and a helper declared inside a member's
// body: that member's entry point is written beside the helper, inside the driver, so the
// name it had still resolves there — and nothing of it is exposed outside the body that
// declared it.

#[stack_safe]
fn through_two_levels(n: u64) -> u64 {
    fn inner(n: u64) -> u64 {
        fn deeper(n: u64) -> u64 {
            if n == 0 {
                0
            } else {
                1 + through_two_levels(n - 1)
            }
        }
        #[allow(dead_code)]
        fn unused_helper(n: u64) -> u64 {
            inner(n)
        }
        if n == 0 { 0 } else { 1 + deeper(n) }
    }
    if n == 0 { 0 } else { 1 + inner(n - 1) }
}

fn through_two_levels_naive(n: u64) -> u64 {
    fn inner(n: u64) -> u64 {
        fn deeper(n: u64) -> u64 {
            if n == 0 {
                0
            } else {
                1 + through_two_levels_naive(n - 1)
            }
        }
        if n == 0 { 0 } else { 1 + deeper(n) }
    }
    if n == 0 { 0 } else { 1 + inner(n - 1) }
}

#[test]
fn a_cycle_through_three_levels_agrees_with_naive() {
    for n in 0..12 {
        assert_eq!(
            through_two_levels(n),
            through_two_levels_naive(n),
            "through_two_levels({n})"
        );
    }
}

#[test]
fn a_cycle_through_three_levels_is_stack_safe() {
    // Three steps per two levels, so an even depth costs exactly one and a half of it.
    let depth = 200_000;
    let got = on_tiny_stack(move || through_two_levels(depth));
    assert_eq!(got, 3 * depth / 2);
}

// A cycle that crosses a body and passes a trait object. The member declared in the body has to be
// written inside the shared driver, so the driver's signature has to name `&dyn Fn() -> u64` — which
// it can, a `dyn` behind a reference being an ordinary field. This was rejected before.

#[stack_safe]
fn calls_through(f: &dyn Fn() -> u64, n: u64) -> u64 {
    fn step(f: &dyn Fn() -> u64, n: u64) -> u64 {
        if n == 0 { f() } else { calls_through(f, n - 1) }
    }
    if n == 0 { f() } else { step(f, n - 1) }
}

#[test]
fn a_cycle_across_a_body_may_pass_a_trait_object() {
    let f = || 7u64;
    for n in 0..8 {
        assert_eq!(calls_through(&f, n), 7, "calls_through({n})");
    }
    let depth = 200_000;
    assert_eq!(on_tiny_stack(move || calls_through(&|| 7u64, depth)), 7);
}
