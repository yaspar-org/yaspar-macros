// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Tests for `#[stack_safe]`: mutual recursion flattened onto one driver.
//!
//! The interesting property is that a *cycle* costs no native stack, not just a
//! self-call — so every depth test here alternates between functions on the way
//! down. As elsewhere in this suite they run on a 64 KiB stack, where a
//! regression to native recursion aborts the process rather than failing quietly.
//!
//! The other half of the feature is what it leaves alone: a module can mix
//! recursive cycles, self-recursive functions and ordinary ones, and only the
//! cycles are rewritten.
//!
//! The annotated module also threads its top-level functions back out, so they are
//! callable unqualified at this scope — `top_down(..)` as well as
//! `nested::top_down(..)`. That works even when a signature names something the
//! module owns, because a `use` names no types at all: the re-export carries the name out
//! without ever spelling the signature. Nested modules are not lifted, so their functions are
//! still reached through their own path.

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
// The two-function cycle, plus a function that is in no cycle at all.
// ---------------------------------------------------------------------------

#[stack_safe]
mod parity {
    pub fn is_even(n: u64) -> bool {
        if n == 0 { true } else { is_odd(n - 1) }
    }

    pub fn is_odd(n: u64) -> bool {
        if n == 0 { false } else { is_even(n - 1) }
    }

    /// Calls into the cycle but is not part of it: left exactly as written.
    pub fn parity_of(n: u64) -> &'static str {
        if is_even(n) { "even" } else { "odd" }
    }
}

#[test]
fn two_function_cycle_is_flat() {
    for n in 0..40 {
        assert_eq!(parity::is_even(n), n % 2 == 0, "n = {n}");
        assert_eq!(parity::is_odd(n), n % 2 == 1, "n = {n}");
    }
    assert!(on_tiny_stack(|| parity::is_even(400_000)));
    assert!(on_tiny_stack(|| parity::is_odd(400_001)));
}

#[test]
fn non_cyclic_function_still_works() {
    assert_eq!(parity::parity_of(7), "odd");
    assert_eq!(parity::parity_of(8), "even");
}

// ---------------------------------------------------------------------------
// A three-function cycle, and a second cycle in the same module. Calls between
// the two cycles are ordinary calls: each runs its own driver.
// ---------------------------------------------------------------------------

#[stack_safe]
mod three {
    pub fn a(n: u64) -> u64 {
        if n == 0 { 0 } else { b(n - 1) + 1 }
    }
    pub fn b(n: u64) -> u64 {
        if n == 0 { 0 } else { c(n - 1) + 1 }
    }
    pub fn c(n: u64) -> u64 {
        if n == 0 { 0 } else { a(n - 1) + 1 }
    }

    /// A separate cycle. `d` also calls into the `a`/`b`/`c` group, which is a
    /// plain call — that group's driver runs to completion and returns.
    pub fn d(n: u64) -> u64 {
        if n == 0 { a(10) } else { e(n - 1) + 1 }
    }
    pub fn e(n: u64) -> u64 {
        if n == 0 { 0 } else { d(n - 1) + 1 }
    }
}

#[test]
fn three_function_cycle_is_flat() {
    for n in 0..30 {
        assert_eq!(three::a(n), n, "n = {n}");
    }
    assert_eq!(on_tiny_stack(|| three::a(300_000)), 300_000);
    assert_eq!(on_tiny_stack(|| three::b(300_000)), 300_000);
}

#[test]
fn two_independent_cycles_in_one_module() {
    assert_eq!(three::d(0), 10);
    assert_eq!(on_tiny_stack(|| three::d(200_000)), 200_000 + 10);
}

// ---------------------------------------------------------------------------
// A cycle that shares a `&mut` context, and that contains loops. The context is
// one tuple for the whole group, so both members reach the same `out`.
// ---------------------------------------------------------------------------

#[stack_safe]
mod visitor {
    /// An expression is a leaf value or a list of statements.
    pub enum Expr {
        Val(u64),
        Block(Vec<Stmt>),
    }

    pub enum Stmt {
        Emit(Expr),
        Repeat(u64, Expr),
    }

    pub fn walk_expr(e: &Expr, out: &mut Vec<u64>) -> u64 {
        match e {
            Expr::Val(v) => {
                out.push(*v);
                *v
            }
            Expr::Block(stmts) => {
                let mut acc = 0;
                for s in stmts {
                    acc += walk_stmt(s, out);
                    // Reached after the whole sub-tree: `out` has to survive it.
                    out.push(0);
                }
                acc
            }
        }
    }

    pub fn walk_stmt(s: &Stmt, out: &mut Vec<u64>) -> u64 {
        match s {
            Stmt::Emit(e) => walk_expr(e, out),
            Stmt::Repeat(n, e) => {
                let mut acc = 0;
                for _ in 0..*n {
                    acc += walk_expr(e, out);
                }
                acc
            }
        }
    }
}

fn walk_expr_naive(e: &visitor::Expr, out: &mut Vec<u64>) -> u64 {
    match e {
        visitor::Expr::Val(v) => {
            out.push(*v);
            *v
        }
        visitor::Expr::Block(stmts) => {
            let mut acc = 0;
            for s in stmts {
                acc += walk_stmt_naive(s, out);
                out.push(0);
            }
            acc
        }
    }
}

fn walk_stmt_naive(s: &visitor::Stmt, out: &mut Vec<u64>) -> u64 {
    match s {
        visitor::Stmt::Emit(e) => walk_expr_naive(e, out),
        visitor::Stmt::Repeat(n, e) => {
            let mut acc = 0;
            for _ in 0..*n {
                acc += walk_expr_naive(e, out);
            }
            acc
        }
    }
}

/// A left-nested chain `Block[Emit(Block[Emit(..Val(1)..)])]`, built iteratively.
fn nest(depth: usize) -> visitor::Expr {
    let mut e = visitor::Expr::Val(1);
    for _ in 0..depth {
        e = visitor::Expr::Block(vec![visitor::Stmt::Emit(e)]);
    }
    e
}

fn bushy(depth: usize) -> visitor::Expr {
    if depth == 0 {
        return visitor::Expr::Val(1);
    }
    visitor::Expr::Block(vec![
        visitor::Stmt::Emit(bushy(depth - 1)),
        visitor::Stmt::Repeat(2, bushy(depth - 1)),
    ])
}

#[test]
fn cycle_with_shared_mut_context_agrees_with_naive() {
    for depth in 0..6 {
        let e = bushy(depth);
        let (mut a, mut b) = (Vec::new(), Vec::new());
        assert_eq!(
            walk_expr(&e, &mut a),
            walk_expr_naive(&e, &mut b),
            "depth = {depth}"
        );
        assert_eq!(a, b, "depth = {depth}");
    }
}

/// Threading out is a `use`, which reproduces no signature, so a type only the module can name is
/// no obstacle: `Expr` is declared there, and `walk_stmt` takes it.
#[test]
fn threading_out_handles_module_owned_types() {
    let e = bushy(3);
    let (mut a, mut b) = (Vec::new(), Vec::new());
    // Unqualified, with a module-declared type in the signature.
    assert_eq!(walk_expr(&e, &mut a), walk_expr_naive(&e, &mut b));
    assert_eq!(a, b);
}

#[test]
fn deep_cycle_with_mut_context_is_flat() {
    let total = on_tiny_stack(|| {
        let e = nest(100_000);
        let mut out = Vec::new();
        let total = visitor::walk_expr(&e, &mut out);
        // A 100 000-deep `Expr` drops recursively, which this stack cannot take.
        std::mem::forget(e);
        (total, out.len())
    });
    assert_eq!(total.0, 1);
    // One `Val` push plus one `0` push per `Block` level.
    assert_eq!(total.1, 100_001);
}

// ---------------------------------------------------------------------------
// A group of one: a self-recursive function inside the module gets the same
// treatment as `#[stack_safe]` on its own.
// ---------------------------------------------------------------------------

#[stack_safe]
mod alone {
    pub fn sum_to(n: u64) -> u64 {
        if n == 0 { 0 } else { n + sum_to(n - 1) }
    }

    pub fn double(n: u64) -> u64 {
        n * 2
    }
}

#[test]
fn self_recursion_in_a_group_module() {
    assert_eq!(alone::sum_to(10), 55);
    assert_eq!(
        on_tiny_stack(|| alone::sum_to(200_000)),
        200_000 * 200_001 / 2
    );
    assert_eq!(alone::double(21), 42);
}

// ---------------------------------------------------------------------------
// Mutually recursive *methods*, grouped on the impl block. A method's body needs
// `Self` and the impl's generics, so it is rewritten where it stands rather than
// moved into an encoding module.
// ---------------------------------------------------------------------------

struct Walker {
    kids: Vec<Vec<usize>>,
    visits: u64,
}

#[stack_safe]
impl Walker {
    /// Calls its partner through `self`.
    fn expr(&mut self, i: usize) -> u64 {
        self.visits += 1;
        let mut acc = 1;
        for k in 0..self.kids[i].len() {
            let kid = self.kids[i][k];
            acc += self.stmt(kid);
            // After the child's subtree: `self` has to be usable again.
            self.visits += 1;
        }
        acc
    }

    /// ...and through the explicit `Self::f(self, ..)` form.
    fn stmt(&mut self, i: usize) -> u64 {
        Self::expr(self, i)
    }

    /// In no cycle: left exactly as written.
    fn visited(&self) -> u64 {
        self.visits
    }
}

/// A chain `0 -> 1 -> .. -> n`, so recursion alternates `expr` / `stmt` all the way
/// down. Arena-based, so building and dropping it is iterative.
fn chain_kids(n: usize) -> Vec<Vec<usize>> {
    let mut kids: Vec<Vec<usize>> = (0..n).map(|i| vec![i + 1]).collect();
    kids.push(Vec::new());
    kids
}

#[test]
fn mutually_recursive_methods_are_flat() {
    let mut w = Walker {
        kids: chain_kids(5),
        visits: 0,
    };
    // Six nodes, each contributing 1, and one extra visit per parent.
    assert_eq!(w.expr(0), 6);
    assert_eq!(w.visited(), 11);

    let deep = on_tiny_stack(|| {
        let mut w = Walker {
            kids: chain_kids(200_000),
            visits: 0,
        };
        w.expr(0)
    });
    assert_eq!(deep, 200_001);
}

// ---------------------------------------------------------------------------
// Arbitrarily deep nesting: the scan descends into modules and impl blocks, and
// groups each container on its own.
// ---------------------------------------------------------------------------

#[stack_safe]
mod nested {
    /// A cycle at the top level of the annotated module.
    pub fn top_down(n: u64) -> u64 {
        if n == 0 { 0 } else { top_up(n - 1) + 1 }
    }
    pub fn top_up(n: u64) -> u64 {
        top_down(n)
    }

    /// In no cycle, so emitted as written; threaded out all the same.
    pub fn describe(n: u64) -> &'static str {
        if top_down(n) == n { "ok" } else { "?" }
    }

    /// Private, so it cannot be threaded out — a `use` of it would sit where the name is
    /// not visible.
    fn secret(n: u64) -> u64 {
        n
    }

    pub fn use_secret(n: u64) -> u64 {
        secret(n)
    }

    pub mod middle {
        pub mod inner {
            /// A cycle two modules deep.
            pub fn even(n: u64) -> bool {
                if n == 0 { true } else { odd(n - 1) }
            }
            pub fn odd(n: u64) -> bool {
                if n == 0 { false } else { even(n - 1) }
            }
        }

        pub struct Counter(pub u64);

        /// A cycle in an impl block inside a nested module.
        impl Counter {
            pub fn down(&mut self, n: u64) -> u64 {
                if n == 0 {
                    self.0
                } else {
                    self.0 += 1;
                    self.up(n - 1)
                }
            }
            pub fn up(&mut self, n: u64) -> u64 {
                self.down(n)
            }
        }
    }
}

#[test]
fn nested_module_cycle_is_flat() {
    // Unqualified: the threaded-out re-export.
    assert_eq!(top_down(7), 7);
    assert_eq!(top_up(7), 7);
    // ...and the module path still works, reaching the same encoding.
    assert_eq!(nested::top_down(7), 7);
    assert_eq!(on_tiny_stack(|| top_down(200_000)), 200_000);
}

#[test]
fn threading_out_covers_every_public_top_level_function() {
    // Not part of any cycle, so emitted as written — and threaded out all the same.
    assert_eq!(describe(7), "ok");
    // Reaches a function the module keeps private, which cannot itself be threaded
    // out but must not stop the rest from being.
    assert_eq!(use_secret(3), 3);
}

/// A nested module is grouped in place and *not* lifted, so its functions are still
/// reached through their own path.
#[test]
fn cycle_two_modules_deep_is_flat() {
    assert!(nested::middle::inner::even(8));
    assert!(nested::middle::inner::odd(9));
    assert!(on_tiny_stack(|| nested::middle::inner::even(400_000)));
}

#[test]
fn cycle_in_a_nested_impl_is_flat() {
    let mut c = nested::middle::Counter(0);
    assert_eq!(c.down(4), 4);
    assert_eq!(
        on_tiny_stack(|| nested::middle::Counter(0).down(200_000)),
        200_000
    );
}

// ---------------------------------------------------------------------------
// The encoding module is a child of the annotated one, so `use super::*` has to
// reach everything an encoded body might name — including the module's private
// items and its private `use` declarations.
// ---------------------------------------------------------------------------

#[stack_safe]
mod private_items {
    use std::collections::HashMap;

    struct Tagged(u64);

    fn bump(x: u64) -> u64 {
        x + 1
    }

    /// References the private `use`, the private type and the private function,
    /// all from inside the encoding module.
    pub fn walk(n: u64, seen: &mut HashMap<u64, u64>) -> u64 {
        let tagged = Tagged(n);
        seen.insert(n, tagged.0);
        if n == 0 { 0 } else { bump(walk(n - 1, seen)) }
    }

    /// Not in the cycle, and calls into it: this resolves to the member itself.
    pub fn distinct(n: u64) -> usize {
        let mut seen = HashMap::new();
        walk(n, &mut seen);
        seen.len()
    }
}

#[test]
fn encoding_module_reaches_private_items() {
    let mut seen = std::collections::HashMap::new();
    assert_eq!(private_items::walk(5, &mut seen), 5);
    assert_eq!(private_items::distinct(5), 6);

    // Threaded out too, though its signature names a type the module imports
    // *privately* — no definition outside the module could have spelled that, whereas a
    // re-export never has to.
    let mut seen = std::collections::HashMap::new();
    assert_eq!(walk(5, &mut seen), 5);
    assert_eq!(distinct(5), 6);

    let deep = on_tiny_stack(|| {
        let mut seen = std::collections::HashMap::new();
        private_items::walk(100_000, &mut seen)
    });
    assert_eq!(deep, 100_000);
}

// ---------------------------------------------------------------------------
// Threading a name out moves it up one level, so its visibility has to be
// re-expressed rather than copied: `pub(super)` on a function meant "the module's
// parent", which is where the copy lands, and no copy may out-reach the module it
// came from.
// ---------------------------------------------------------------------------

mod visibility {
    use yaspar_macros::stack_safe;

    #[stack_safe]
    pub mod api {
        pub fn public(n: u64) -> u64 {
            if n == 0 { 0 } else { public(n - 1) + 1 }
        }

        pub(crate) fn crate_wide(n: u64) -> u64 {
            if n == 0 { 0 } else { crate_wide(n - 1) + 1 }
        }

        /// Visible to `visibility`, so the threaded copy is private *there*.
        pub(super) fn to_parent(n: u64) -> u64 {
            if n == 0 { 0 } else { to_parent(n - 1) + 1 }
        }

        /// Kept inside `api`, so nothing is threaded out for it.
        fn own(n: u64) -> u64 {
            if n == 0 { 0 } else { own(n - 1) + 1 }
        }

        pub fn via_own(n: u64) -> u64 {
            own(n)
        }
    }

    /// Reachable here because the copy of a `pub(super)` function is private at this
    /// level — the same reach the original had.
    pub fn reach_to_parent(n: u64) -> u64 {
        to_parent(n)
    }

    pub fn reach_public(n: u64) -> u64 {
        public(n)
    }
}

/// A `pub fn` inside a *private* module was never reachable from outside it, and
/// threading it out must not change that: the copy is capped at the module's own
/// visibility.
#[stack_safe]
mod capped {
    pub fn inner(n: u64) -> u64 {
        if n == 0 { 0 } else { inner(n - 1) + 1 }
    }
}

#[test]
fn threaded_visibility_is_re_expressed() {
    // Straight through the module path, and through the threaded copies.
    assert_eq!(visibility::api::public(3), 3);
    assert_eq!(visibility::reach_public(3), 3);
    assert_eq!(visibility::reach_to_parent(3), 3);
    assert_eq!(visibility::api::crate_wide(3), 3);
    assert_eq!(visibility::api::via_own(3), 3);

    // `capped` is private, so its copy is private here — usable in this file.
    assert_eq!(inner(3), 3);
    assert_eq!(on_tiny_stack(|| inner(200_000)), 200_000);
}

// ---------------------------------------------------------------------------
// `data_in_frame` across a mutually recursive group: each member lends a value it
// built, at a different position and of a different type. A member's entry payload is
// then only ever constructed inside *another* member's arm, so the pointer type has to
// be named in the arm rather than left to inference.
// ---------------------------------------------------------------------------

enum Ints<'a> {
    Nil,
    Cons(#[allow(dead_code)] u64, &'a Ints<'a>),
}

enum Bools<'a> {
    Nil,
    Cons(#[allow(dead_code)] bool, &'a Bools<'a>),
}

fn int_depth(x: &Ints<'_>) -> usize {
    let (mut n, mut cur) = (0, x);
    while let Ints::Cons(_, tail) = cur {
        n += 1;
        cur = tail;
    }
    n
}

fn bool_depth(x: &Bools<'_>) -> usize {
    let (mut n, mut cur) = (0, x);
    while let Bools::Cons(_, tail) = cur {
        n += 1;
        cur = tail;
    }
    n
}

#[stack_safe(data_in_frame)]
mod lending {
    use super::{Bools, Ints, bool_depth, int_depth};

    pub fn ping(n: usize, a: &Ints<'_>, b: &Bools<'_>) -> usize {
        if int_depth(a) >= n {
            int_depth(a) + bool_depth(b)
        } else {
            pong(n, &Ints::Cons(1, a), b)
        }
    }

    pub fn pong(n: usize, a: &Ints<'_>, b: &Bools<'_>) -> usize {
        if int_depth(a) >= n {
            int_depth(a) + bool_depth(b)
        } else {
            ping(n, a, &Bools::Cons(true, b))
        }
    }
}

fn ping_naive(n: usize, a: &Ints<'_>, b: &Bools<'_>) -> usize {
    if int_depth(a) >= n {
        int_depth(a) + bool_depth(b)
    } else {
        pong_naive(n, &Ints::Cons(1, a), b)
    }
}

fn pong_naive(n: usize, a: &Ints<'_>, b: &Bools<'_>) -> usize {
    if int_depth(a) >= n {
        int_depth(a) + bool_depth(b)
    } else {
        ping_naive(n, a, &Bools::Cons(true, b))
    }
}

#[test]
fn a_group_can_lend_values_it_builds() {
    for n in 0..8 {
        assert_eq!(
            lending::ping(n, &Ints::Nil, &Bools::Nil),
            ping_naive(n, &Ints::Nil, &Bools::Nil),
            "n = {n}"
        );
    }
}

#[test]
fn a_lending_group_is_stack_safe() {
    let depth = 2_000;
    let got = on_tiny_stack(move || lending::ping(depth, &Ints::Nil, &Bools::Nil));
    assert_eq!(got, depth + (depth - 1));
}

// ---------------------------------------------------------------------------
// A cycle where one member contains a loop that recurses. The loop becomes an entry
// of its own, carrying an iterator and an accumulator whose types the macro cannot
// name — which is why the shared machine takes a *seed* of the members' parameters
// and keeps the entry enum inside itself, where inference still reaches it.
// ---------------------------------------------------------------------------

enum Shape {
    Leaf(u64),
    Branch(Vec<usize>),
}

#[stack_safe]
mod tree {
    use super::Shape;

    pub fn sum_node(nodes: &[Shape], i: usize) -> u64 {
        match &nodes[i] {
            Shape::Leaf(v) => *v,
            Shape::Branch(kids) => {
                let mut acc = 0;
                for &k in kids {
                    acc += sum_kid(nodes, k);
                }
                acc
            }
        }
    }

    pub fn sum_kid(nodes: &[Shape], k: usize) -> u64 {
        sum_node(nodes, k) + 1
    }
}

fn sum_node_naive(nodes: &[Shape], i: usize) -> u64 {
    match &nodes[i] {
        Shape::Leaf(v) => *v,
        Shape::Branch(kids) => {
            let mut acc = 0;
            for &k in kids {
                acc += sum_kid_naive(nodes, k);
            }
            acc
        }
    }
}

fn sum_kid_naive(nodes: &[Shape], k: usize) -> u64 {
    sum_node_naive(nodes, k) + 1
}

/// A left-leaning chain of `depth` branches, each with a leaf beside it.
fn chain(depth: usize) -> Vec<Shape> {
    let leaf = depth;
    let mut nodes: Vec<Shape> = (0..depth)
        .map(|i| Shape::Branch(vec![i + 1, leaf]))
        .collect();
    nodes.push(Shape::Leaf(1));
    nodes
}

#[test]
fn cycle_through_a_loop_agrees_with_naive() {
    for depth in 0..12 {
        let nodes = chain(depth);
        assert_eq!(
            tree::sum_node(&nodes, 0),
            sum_node_naive(&nodes, 0),
            "depth {depth}"
        );
        assert_eq!(tree::sum_kid(&nodes, 0), sum_kid_naive(&nodes, 0));
        // Each branch adds its leaf's `sum_kid` (2) and one for the `sum_kid` hop into
        // the next branch, over a leaf of 1 — which the deep test relies on, since
        // calling the naive twin at depth would overflow.
        assert_eq!(
            tree::sum_node(&nodes, 0),
            1 + 3 * depth as u64,
            "depth {depth}"
        );
    }
}

#[test]
fn cycle_through_a_loop_is_flat() {
    let depth = 100_000;
    let got = on_tiny_stack(move || {
        let nodes = chain(depth);
        tree::sum_node(&nodes, 0)
    });
    assert_eq!(got, 1 + 3 * depth as u64);
}

// ---------------------------------------------------------------------------
// Members of a cycle need not agree on their return type. The driver has one result, so
// the macro joins them into a union of its own and each member keeps its signature.
// ---------------------------------------------------------------------------

#[stack_safe]
mod mixed {
    pub fn even_ish(n: u64) -> bool {
        if n == 0 { true } else { count(n - 1) % 2 == 1 }
    }

    pub fn count(n: u64) -> u64 {
        if n == 0 {
            0
        } else if even_ish(n - 1) {
            1
        } else {
            2
        }
    }
}

fn even_ish_naive(n: u64) -> bool {
    if n == 0 {
        true
    } else {
        count_naive(n - 1) % 2 == 1
    }
}

fn count_naive(n: u64) -> u64 {
    if n == 0 {
        0
    } else if even_ish_naive(n - 1) {
        1
    } else {
        2
    }
}

#[test]
fn differing_return_types_agree_with_naive() {
    for n in 0..15 {
        assert_eq!(mixed::even_ish(n), even_ish_naive(n), "even_ish({n})");
        assert_eq!(mixed::count(n), count_naive(n), "count({n})");
    }
}

#[test]
fn differing_return_types_are_flat() {
    let depth = 200_000;
    // The same two recurrences, bottom-up: an oracle that costs no stack, since the
    // naive twins would overflow long before this depth.
    let (mut evens, mut counts) = (vec![false; depth + 1], vec![0u64; depth + 1]);
    evens[0] = true;
    for k in 1..=depth {
        counts[k] = if evens[k - 1] { 1 } else { 2 };
        evens[k] = counts[k - 1] % 2 == 1;
    }

    let got = on_tiny_stack(move || (mixed::even_ish(depth as u64), mixed::count(depth as u64)));
    assert_eq!(got, (evens[depth], counts[depth]));
}

// Three members, three return types, so the union carries more than a pair.

#[stack_safe]
mod triple {
    pub fn size(n: u64) -> u64 {
        if n == 0 {
            0
        } else {
            1 + tag(n - 1).len() as u64
        }
    }

    pub fn tag(n: u64) -> String {
        if n == 0 {
            String::new()
        } else if flag(n - 1) {
            "x".to_string()
        } else {
            "yy".to_string()
        }
    }

    pub fn flag(n: u64) -> bool {
        if n == 0 {
            true
        } else {
            size(n - 1).is_multiple_of(2)
        }
    }
}

fn size_naive(n: u64) -> u64 {
    if n == 0 {
        0
    } else {
        1 + tag_naive(n - 1).len() as u64
    }
}

fn tag_naive(n: u64) -> String {
    if n == 0 {
        String::new()
    } else if flag_naive(n - 1) {
        "x".to_string()
    } else {
        "yy".to_string()
    }
}

fn flag_naive(n: u64) -> bool {
    if n == 0 {
        true
    } else {
        size_naive(n - 1).is_multiple_of(2)
    }
}

#[test]
fn three_members_with_three_return_types() {
    for n in 0..15 {
        assert_eq!(triple::size(n), size_naive(n), "measure({n})");
        assert_eq!(triple::tag(n), tag_naive(n), "tag({n})");
        assert_eq!(triple::flag(n), flag_naive(n), "flag({n})");
    }
}

#[test]
fn three_return_types_are_flat() {
    let depth = 100_000usize;
    // The three recurrences, bottom-up. The *value* stays small — a tag is one or two
    // characters — but the recursion still descends through all three members, which is
    // what the tiny stack is there to catch.
    let mut sizes = vec![0u64; depth + 1];
    let mut tags: Vec<&str> = vec![""; depth + 1];
    let mut flags = vec![false; depth + 1];
    flags[0] = true;
    for k in 1..=depth {
        sizes[k] = 1 + tags[k - 1].len() as u64;
        tags[k] = if flags[k - 1] { "x" } else { "yy" };
        flags[k] = sizes[k - 1].is_multiple_of(2);
    }

    let got = on_tiny_stack(move || {
        (
            triple::size(depth as u64),
            triple::tag(depth as u64),
            triple::flag(depth as u64),
        )
    });
    assert_eq!(got, (sizes[depth], tags[depth].to_string(), flags[depth]));
}

// Differing return types alongside the other machinery: a shared `&mut` context, a loop
// that recurses, and a cycle of methods.

#[stack_safe]
mod mixed_ctx {
    pub fn record(n: u64, log: &mut Vec<u64>) -> u64 {
        log.push(n);
        if n == 0 {
            0
        } else {
            1 + trace(n - 1, log).len() as u64
        }
    }

    pub fn trace(n: u64, log: &mut Vec<u64>) -> String {
        if n == 0 {
            String::new()
        } else {
            let k = record(n - 1, log);
            if k.is_multiple_of(2) {
                "e".to_string()
            } else {
                "oo".to_string()
            }
        }
    }
}

fn record_naive(n: u64, log: &mut Vec<u64>) -> u64 {
    log.push(n);
    if n == 0 {
        0
    } else {
        1 + trace_naive(n - 1, log).len() as u64
    }
}

fn trace_naive(n: u64, log: &mut Vec<u64>) -> String {
    if n == 0 {
        String::new()
    } else {
        let k = record_naive(n - 1, log);
        if k.is_multiple_of(2) {
            "e".to_string()
        } else {
            "oo".to_string()
        }
    }
}

#[test]
fn differing_return_types_with_a_mut_context() {
    for n in 0..12 {
        let (mut a, mut b) = (Vec::new(), Vec::new());
        assert_eq!(
            mixed_ctx::record(n, &mut a),
            record_naive(n, &mut b),
            "record({n})"
        );
        // The side effects have to match too, not just the results.
        assert_eq!(a, b, "log after record({n})");

        let (mut a, mut b) = (Vec::new(), Vec::new());
        assert_eq!(
            mixed_ctx::trace(n, &mut a),
            trace_naive(n, &mut b),
            "trace({n})"
        );
        assert_eq!(a, b, "log after trace({n})");
    }
}

#[test]
fn differing_return_types_with_a_mut_context_are_flat() {
    let depth = 100_000;
    let got = on_tiny_stack(move || {
        let mut log = Vec::new();
        let n = mixed_ctx::record(depth, &mut log);
        (n, log.len())
    });
    // One `push` per `record`, and `record` runs at every other level.
    assert!(got.1 >= depth as usize / 2, "pushes: {}", got.1);
}

#[stack_safe]
mod mixed_loop {
    pub fn total(kids: &[u64], i: usize) -> u64 {
        if i >= kids.len() {
            return 0;
        }
        let mut acc = 0;
        for k in i..kids.len() {
            let s = label(kids, k);
            acc += s.len() as u64 + kids[k];
        }
        acc
    }

    pub fn label(kids: &[u64], i: usize) -> String {
        if i + 1 >= kids.len() {
            "end".to_string()
        } else {
            let t = total(kids, i + 1);
            if t.is_multiple_of(2) {
                "e".to_string()
            } else {
                "oo".to_string()
            }
        }
    }
}

fn total_naive(kids: &[u64], i: usize) -> u64 {
    if i >= kids.len() {
        return 0;
    }
    let mut acc = 0;
    for k in i..kids.len() {
        let s = label_naive(kids, k);
        acc += s.len() as u64 + kids[k];
    }
    acc
}

fn label_naive(kids: &[u64], i: usize) -> String {
    if i + 1 >= kids.len() {
        "end".to_string()
    } else {
        let t = total_naive(kids, i + 1);
        if t.is_multiple_of(2) {
            "e".to_string()
        } else {
            "oo".to_string()
        }
    }
}

#[test]
fn differing_return_types_across_a_loop() {
    for len in 0..9 {
        let kids: Vec<u64> = (0..len as u64).collect();
        assert_eq!(
            mixed_loop::total(&kids, 0),
            total_naive(&kids, 0),
            "len {len}"
        );
        assert_eq!(
            mixed_loop::label(&kids, 0),
            label_naive(&kids, 0),
            "len {len}"
        );
    }
}

struct Pair(u64);

#[stack_safe]
impl Pair {
    pub fn depth(&self, n: u64) -> u64 {
        if n == 0 {
            self.0
        } else {
            1 + self.name(n - 1).len() as u64
        }
    }

    pub fn name(&self, n: u64) -> String {
        if n == 0 {
            "leaf".to_string()
        } else if self.depth(n - 1).is_multiple_of(2) {
            "e".to_string()
        } else {
            "oo".to_string()
        }
    }
}

fn depth_naive(p: &Pair, n: u64) -> u64 {
    if n == 0 {
        p.0
    } else {
        1 + name_naive(p, n - 1).len() as u64
    }
}

fn name_naive(p: &Pair, n: u64) -> String {
    if n == 0 {
        "leaf".to_string()
    } else if depth_naive(p, n - 1).is_multiple_of(2) {
        "e".to_string()
    } else {
        "oo".to_string()
    }
}

#[test]
fn differing_return_types_on_an_impl_block() {
    let p = Pair(3);
    for n in 0..12 {
        assert_eq!(p.depth(n), depth_naive(&p, n), "depth({n})");
        assert_eq!(p.name(n), name_naive(&p, n), "describe_at({n})");
    }
}

#[test]
fn differing_return_types_on_an_impl_block_are_flat() {
    let got = on_tiny_stack(|| {
        let p = Pair(3);
        (p.depth(100_000), p.name(100_000))
    });
    assert!(got.0 >= 2 && got.1.len() <= 4, "got {got:?}");
}

// `?` inside a cycle whose members return different types. An early exit finishes the
// member from wherever it stands, so it has to enter the union just as a tail expression
// does — which `return` and `?` did not, being rewritten on their own.

#[derive(Debug, PartialEq)]
struct Bad(u64);

#[stack_safe]
mod fallible {
    use super::Bad;

    pub fn measure(n: u64, fail_at: u64) -> Result<u64, Bad> {
        if n == fail_at {
            return Err(Bad(n));
        }
        if n == 0 {
            return Ok(0);
        }
        let s = describe_at(n - 1, fail_at)?;
        Ok(1 + s.len() as u64)
    }

    pub fn describe_at(n: u64, fail_at: u64) -> Result<String, Bad> {
        if n == fail_at {
            return Err(Bad(n));
        }
        if n == 0 {
            return Ok("leaf".to_string());
        }
        let k = measure(n - 1, fail_at)?;
        Ok(if k.is_multiple_of(2) {
            "e".to_string()
        } else {
            "oo".to_string()
        })
    }
}

fn measure_naive(n: u64, fail_at: u64) -> Result<u64, Bad> {
    if n == fail_at {
        return Err(Bad(n));
    }
    if n == 0 {
        return Ok(0);
    }
    let s = describe_at_naive(n - 1, fail_at)?;
    Ok(1 + s.len() as u64)
}

fn describe_at_naive(n: u64, fail_at: u64) -> Result<String, Bad> {
    if n == fail_at {
        return Err(Bad(n));
    }
    if n == 0 {
        return Ok("leaf".to_string());
    }
    let k = measure_naive(n - 1, fail_at)?;
    Ok(if k.is_multiple_of(2) {
        "e".to_string()
    } else {
        "oo".to_string()
    })
}

#[test]
fn question_mark_inside_a_cycle_of_differing_types() {
    for n in 0..10 {
        // Never fails, so every exit is a tail expression or an `Ok` return.
        assert_eq!(
            fallible::measure(n, u64::MAX),
            measure_naive(n, u64::MAX),
            "measure({n})"
        );
        assert_eq!(
            fallible::describe_at(n, u64::MAX),
            describe_at_naive(n, u64::MAX),
            "describe_at({n})"
        );
        // ...and failing at every level in turn, so the `?` exits from each member.
        for fail_at in 0..n {
            assert_eq!(
                fallible::measure(n, fail_at),
                measure_naive(n, fail_at),
                "measure({n}) failing at {fail_at}"
            );
            assert_eq!(
                fallible::describe_at(n, fail_at),
                describe_at_naive(n, fail_at),
                "describe_at({n}) failing at {fail_at}"
            );
        }
    }
}

#[test]
fn question_mark_unwinds_a_cycle_of_differing_types() {
    let depth = 100_000;
    // Deep and successful, then deep and failing near the bottom, so the `?` propagates
    // back up through both members' frames.
    let got = on_tiny_stack(move || {
        (
            fallible::measure(depth, u64::MAX).is_ok(),
            fallible::measure(depth, 1),
        )
    });
    assert_eq!(got, (true, Err(Bad(1))));
}

// The union has to compose with everything else the transform does, so each combination
// is checked against a naive twin: an early `break` carrying a value out of a lowered
// loop, a member returning `()`, `?` on carriers that differ between members, and the
// two opt-ins.

#[stack_safe]
mod brk {
    pub fn first_wide(xs: &[u64], i: usize) -> u64 {
        let mut k = i;
        loop {
            if k >= xs.len() {
                break 0;
            }
            if width(xs, k).len() == 2 {
                break xs[k];
            }
            k += 1;
        }
    }

    pub fn width(xs: &[u64], i: usize) -> String {
        if i + 1 >= xs.len() {
            "e".to_string()
        } else if first_wide(xs, i + 1) > 3 {
            "oo".to_string()
        } else {
            "e".to_string()
        }
    }
}

fn first_wide_naive(xs: &[u64], i: usize) -> u64 {
    let mut k = i;
    loop {
        if k >= xs.len() {
            break 0;
        }
        if width_naive(xs, k).len() == 2 {
            break xs[k];
        }
        k += 1;
    }
}

fn width_naive(xs: &[u64], i: usize) -> String {
    if i + 1 >= xs.len() {
        "e".to_string()
    } else if first_wide_naive(xs, i + 1) > 3 {
        "oo".to_string()
    } else {
        "e".to_string()
    }
}

#[test]
fn a_break_carrying_a_value_enters_the_union() {
    for len in 0..8 {
        let xs: Vec<u64> = (0..len as u64).collect();
        assert_eq!(
            brk::first_wide(&xs, 0),
            first_wide_naive(&xs, 0),
            "len {len}"
        );
        assert_eq!(brk::width(&xs, 0), width_naive(&xs, 0), "len {len}");
    }
}

#[stack_safe]
mod unit_member {
    pub fn note(n: u64, log: &mut Vec<u64>) {
        log.push(n);
        if n > 0 {
            let _ = level(n - 1, log);
        }
    }

    pub fn level(n: u64, log: &mut Vec<u64>) -> u64 {
        if n == 0 {
            0
        } else {
            note(n - 1, log);
            1
        }
    }
}

fn note_naive(n: u64, log: &mut Vec<u64>) {
    log.push(n);
    if n > 0 {
        let _ = level_naive(n - 1, log);
    }
}

fn level_naive(n: u64, log: &mut Vec<u64>) -> u64 {
    if n == 0 {
        0
    } else {
        note_naive(n - 1, log);
        1
    }
}

#[test]
fn a_unit_member_joins_the_union() {
    for n in 0..10 {
        let (mut a, mut b) = (Vec::new(), Vec::new());
        unit_member::note(n, &mut a);
        note_naive(n, &mut b);
        assert_eq!(a, b, "note({n}) side effects");
        assert_eq!(
            unit_member::level(n, &mut Vec::new()),
            level_naive(n, &mut Vec::new()),
            "level({n})"
        );
    }
}

#[test]
fn a_unit_member_is_flat() {
    let depth = 100_000;
    let pushes = on_tiny_stack(move || {
        let mut log = Vec::new();
        unit_member::note(depth, &mut log);
        log.len()
    });
    assert!(pushes >= depth as usize / 2, "pushes {pushes}");
}

#[stack_safe]
mod carriers {
    pub fn maybe(n: u64) -> Option<u64> {
        if n == 0 {
            return None;
        }
        let s = surely(n - 1).ok()?;
        Some(s + 1)
    }

    pub fn surely(n: u64) -> Result<u64, ()> {
        if n == 0 {
            return Ok(0);
        }
        let v = maybe(n - 1).ok_or(())?;
        Ok(v)
    }
}

fn maybe_naive(n: u64) -> Option<u64> {
    if n == 0 {
        return None;
    }
    let s = surely_naive(n - 1).ok()?;
    Some(s + 1)
}

fn surely_naive(n: u64) -> Result<u64, ()> {
    if n == 0 {
        return Ok(0);
    }
    let v = maybe_naive(n - 1).ok_or(())?;
    Ok(v)
}

#[test]
fn members_may_use_different_carriers_for_question_mark() {
    for n in 0..10 {
        assert_eq!(carriers::maybe(n), maybe_naive(n), "maybe({n})");
        assert_eq!(carriers::surely(n), surely_naive(n), "surely({n})");
    }
}

// Both opt-ins, each alongside a union.

struct Node {
    v: u64,
    kids: Vec<Node>,
}

#[stack_safe(use_nonlinear_mut)]
mod nonlinear_union {
    use super::Node;

    pub fn bump(t: &mut Node) -> u64 {
        t.v += 1;
        let mut acc = t.v;
        for i in 0..t.kids.len() {
            acc += render(&mut t.kids[i]).len() as u64;
        }
        acc
    }

    pub fn render(t: &mut Node) -> String {
        let n = bump(t);
        if n.is_multiple_of(2) {
            "e".to_string()
        } else {
            "oo".to_string()
        }
    }
}

fn bump_union_naive(t: &mut Node) -> u64 {
    t.v += 1;
    let mut acc = t.v;
    for i in 0..t.kids.len() {
        acc += label_union_naive(&mut t.kids[i]).len() as u64;
    }
    acc
}

fn label_union_naive(t: &mut Node) -> String {
    let n = bump_union_naive(t);
    if n.is_multiple_of(2) {
        "e".to_string()
    } else {
        "oo".to_string()
    }
}

#[test]
fn a_union_composes_with_use_nonlinear_mut() {
    let (mut a, mut b) = (bushy_tree(4), bushy_tree(4));
    assert_eq!(nonlinear_union::bump(&mut a), bump_union_naive(&mut b));
    let (mut a, mut b) = (bushy_tree(4), bushy_tree(4));
    assert_eq!(nonlinear_union::render(&mut a), label_union_naive(&mut b));
}

enum Chain<'a> {
    Nil,
    Cons(#[allow(dead_code)] u64, &'a Chain<'a>),
}

fn chain_depth(c: &Chain<'_>) -> usize {
    let (mut n, mut cur) = (0, c);
    while let Chain::Cons(_, tail) = cur {
        n += 1;
        cur = tail;
    }
    n
}

#[stack_safe(data_in_frame)]
mod frame_union {
    use super::{Chain, chain_depth};

    pub fn grow(n: usize, c: &Chain<'_>) -> usize {
        if chain_depth(c) >= n {
            chain_depth(c)
        } else {
            mark(n, &Chain::Cons(1, c)).len()
        }
    }

    pub fn mark(n: usize, c: &Chain<'_>) -> String {
        let d = grow(n, c);
        "x".repeat(d)
    }
}

fn grow_naive(n: usize, c: &Chain<'_>) -> usize {
    if chain_depth(c) >= n {
        chain_depth(c)
    } else {
        mark_naive(n, &Chain::Cons(1, c)).len()
    }
}

fn mark_naive(n: usize, c: &Chain<'_>) -> String {
    let d = grow_naive(n, c);
    "x".repeat(d)
}

#[test]
fn a_union_composes_with_data_in_frame() {
    for n in 0..8 {
        assert_eq!(
            frame_union::grow(n, &Chain::Nil),
            grow_naive(n, &Chain::Nil),
            "grow({n})"
        );
        assert_eq!(
            frame_union::mark(n, &Chain::Nil),
            mark_naive(n, &Chain::Nil),
            "mark({n})"
        );
    }
}

/// A small binary tree, for the `use_nonlinear_mut` cases above.
fn bushy_tree(d: u64) -> Node {
    if d == 0 {
        return Node {
            v: 1,
            kids: Vec::new(),
        };
    }
    Node {
        v: d,
        kids: (0..2).map(|_| bushy_tree(d - 1)).collect(),
    }
}

/// An alias hiding a reference, written with its lifetime spelled out. Left bare, as
/// `w: Words`, the shared seed would need a lifetime nothing in the tokens mentions and
/// the result is an `E0106` on that parameter.
type Words<'a> = &'a [&'a str];

#[stack_safe]
mod aliased {
    use super::Words;

    pub fn words_left(w: Words<'_>, i: usize) -> usize {
        if i >= w.len() {
            0
        } else {
            1 + words_span(w, i + 1)
        }
    }

    pub fn words_span(w: Words<'_>, i: usize) -> usize {
        words_left(w, i)
    }
}

fn count_aliased_naive(w: Words<'_>, i: usize) -> usize {
    if i >= w.len() {
        0
    } else {
        1 + width_aliased_naive(w, i + 1)
    }
}

fn width_aliased_naive(w: Words<'_>, i: usize) -> usize {
    count_aliased_naive(w, i)
}

#[test]
fn a_reference_alias_works_when_its_lifetime_is_written() {
    let words = ["a", "b", "c", "d"];
    for i in 0..=words.len() {
        assert_eq!(
            aliased::words_left(&words, i),
            count_aliased_naive(&words, i),
            "i {i}"
        );
    }
}

// `impl Trait` is rejected only where a group has to *share* one result type. A member in
// no cycle is emitted as written, and a self-recursive member is a group of one, which is
// the only thing its own driver answers for.

#[stack_safe]
mod opaque {
    pub fn helper(n: u64) -> impl Iterator<Item = u64> {
        0..n
    }

    pub fn solo(n: u64) -> impl Iterator<Item = u64> {
        if n == 0 {
            0..1
        } else {
            let k = solo(n - 1).count() as u64;
            0..(k + 1)
        }
    }

    pub fn opaque_flag(n: u64) -> bool {
        if n == 0 { true } else { opaque_size(n - 1) > 0 }
    }

    pub fn opaque_size(n: u64) -> u64 {
        if n == 0 {
            0
        } else if opaque_flag(n - 1) {
            1
        } else {
            2
        }
    }
}

fn solo_naive(n: u64) -> u64 {
    if n == 0 { 1 } else { solo_naive(n - 1) + 1 }
}

#[test]
fn impl_trait_is_allowed_outside_a_shared_result() {
    // In no cycle: untouched.
    assert_eq!(opaque::helper(4).count(), 4);
    // A group of one: transformed, and still opaque.
    for n in 0..8 {
        assert_eq!(opaque::solo(n).count() as u64, solo_naive(n), "solo({n})");
    }
}

#[test]
fn a_self_recursive_impl_trait_member_is_flat() {
    let depth = 100_000;
    let got = on_tiny_stack(move || opaque::solo(depth).count() as u64);
    assert_eq!(got, solo_naive_iter(depth));
}

/// `solo` bottom-up, since the naive twin would overflow at this depth.
fn solo_naive_iter(n: u64) -> u64 {
    (0..=n).fold(1, |acc, k| if k == 0 { 1 } else { acc + 1 })
}

#[test]
fn differing_return_types_are_reachable_unqualified() {
    // The threaded-out names reach the same union-returning machine.
    for n in 0..10 {
        assert_eq!(opaque_flag(n), opaque::opaque_flag(n), "opaque_flag({n})");
        assert_eq!(opaque_size(n), opaque::opaque_size(n), "opaque_size({n})");
    }
}

// ---------------------------------------------------------------------------
// A member's body is a scope of its own, so the scan reaches into it: a cycle declared
// entirely inside one is rewritten there, and one that runs through the member hosting it
// joins that member's driver. Both used to be left as written, and so still consumed the
// native stack.
// ---------------------------------------------------------------------------

#[stack_safe]
mod bodies {
    /// A cycle declared entirely inside the body. `hosts` itself does not recurse.
    pub fn hosts(n: u64) -> u64 {
        fn go(n: u64) -> u64 {
            if n == 0 { 0 } else { 1 + go(n - 1) }
        }
        go(n)
    }

    /// A cycle between the member and a function declared in its body.
    pub fn crosses(n: u64) -> u64 {
        fn step(n: u64) -> u64 {
            if n == 0 { 0 } else { 1 + crosses(n - 1) }
        }
        if n == 0 { 0 } else { 1 + step(n - 1) }
    }

    /// A cycle between two members, one of which hosts a recursion of its own besides.
    pub fn tick(n: u64) -> u64 {
        fn aside(n: u64) -> u64 {
            if n == 0 { 0 } else { 1 + aside(n - 1) }
        }
        if n == 0 { aside(3) } else { tock(n - 1) }
    }

    pub fn tock(n: u64) -> u64 {
        if n == 0 { 0 } else { tick(n - 1) }
    }
}

struct Host;

#[stack_safe]
impl Host {
    /// A nested function cannot name `Self`, so it recurses on its own; the method is left
    /// as written around it.
    fn hosts(&self, n: u64) -> u64 {
        fn go(n: u64) -> u64 {
            if n == 0 { 0 } else { 1 + go(n - 1) }
        }
        go(n)
    }
}

#[test]
fn a_recursion_in_a_member_body_agrees_with_naive() {
    for n in 0..10 {
        assert_eq!(bodies::hosts(n), n, "hosts({n})");
        assert_eq!(bodies::crosses(n), n, "crosses({n})");
        assert_eq!(Host.hosts(n), n, "Host::hosts({n})");
    }
    // Alternating down to zero: an even depth ends at `tick(0)`, where `aside(3)` counts
    // three more, and an odd one at `tock(0)`, which counts none.
    for n in 0..10 {
        let want = if n % 2 == 0 { 3 } else { 0 };
        assert_eq!(bodies::tick(n), want, "tick({n})");
    }
}

#[test]
fn a_recursion_in_a_member_body_is_flat() {
    let depth = 200_000;
    assert_eq!(on_tiny_stack(move || bodies::hosts(depth)), depth);
    assert_eq!(on_tiny_stack(move || bodies::crosses(depth)), depth);
    assert_eq!(on_tiny_stack(move || bodies::tick(depth)), 3);
    assert_eq!(on_tiny_stack(move || Host.hosts(depth)), depth);
}

// ---------------------------------------------------------------------------
// A cycle that leaves one member's body for a *different* member. Neither half of the scan
// sees it alone: the container's own functions never call `sideways`, and `sideways` calls a
// name that is not in the body declaring it. Only the whole scope as one graph has the cycle.
// ---------------------------------------------------------------------------

#[stack_safe]
mod crossing {
    pub fn climb(n: u64) -> u64 {
        fn sideways(n: u64) -> u64 {
            if n == 0 { 0 } else { 1 + fall(n - 1) }
        }
        if n == 0 { 0 } else { sideways(n) }
    }

    pub fn fall(n: u64) -> u64 {
        if n == 0 { 0 } else { climb(n - 1) }
    }
}

fn climb_naive(n: u64) -> u64 {
    fn sideways(n: u64) -> u64 {
        if n == 0 { 0 } else { 1 + fall_naive(n - 1) }
    }
    if n == 0 { 0 } else { sideways(n) }
}

fn fall_naive(n: u64) -> u64 {
    if n == 0 { 0 } else { climb_naive(n - 1) }
}

#[test]
fn a_cycle_leaving_a_body_for_another_member_agrees_with_naive() {
    for n in 0..12 {
        assert_eq!(crossing::climb(n), climb_naive(n), "climb({n})");
        assert_eq!(crossing::fall(n), fall_naive(n), "fall({n})");
    }
}

#[test]
fn a_cycle_leaving_a_body_for_another_member_is_flat() {
    // One step of `sideways` per two levels.
    let depth = 200_000;
    assert_eq!(on_tiny_stack(move || crossing::climb(depth)), depth / 2);
    assert_eq!(on_tiny_stack(move || crossing::fall(depth)), depth / 2);
}

// ---------------------------------------------------------------------------
// What a group's members may have in their signatures and still share one driver. The seed enum
// names those types, so it carries the members' own generic parameters — the union of them, keyed
// by name — and a `dyn` behind a reference is a field like any other. Where the parameters cannot
// be shared, the group is emitted as a copy per member instead, which is not observable here
// beyond its still working.
// ---------------------------------------------------------------------------

#[stack_safe]
mod trait_object {
    pub fn dyn_tick(f: &dyn Fn(u64) -> u64, n: u64) -> u64 {
        if n == 0 { 0 } else { f(1) + dyn_tock(f, n - 1) }
    }
    pub fn dyn_tock(f: &dyn Fn(u64) -> u64, n: u64) -> u64 {
        if n == 0 { 0 } else { f(1) + dyn_tick(f, n - 1) }
    }
}

#[stack_safe]
mod generic {
    pub fn gen_up<T: Copy + Into<u64>>(t: T, n: u64) -> u64 {
        if n == 0 {
            t.into()
        } else {
            gen_down(t, n - 1) + 1
        }
    }
    pub fn gen_down<T: Copy + Into<u64>>(t: T, n: u64) -> u64 {
        if n == 0 {
            t.into()
        } else {
            gen_up(t, n - 1) + 1
        }
    }
}

#[stack_safe]
mod borrowed {
    pub fn borrow_walk<'a>(s: &'a str, n: u64) -> usize {
        if n == 0 {
            s.len()
        } else {
            borrow_step(s, n - 1)
        }
    }
    pub fn borrow_step<'a>(s: &'a str, n: u64) -> usize {
        if n == 0 {
            s.len()
        } else {
            borrow_walk(s, n - 1)
        }
    }
}

#[stack_safe]
mod bounded {
    pub fn bound_rise<T>(t: T, n: u64) -> u64
    where
        T: Copy + Into<u64>,
    {
        if n == 0 {
            t.into()
        } else {
            bound_sink(t, n - 1) + 1
        }
    }
    pub fn bound_sink<T>(t: T, n: u64) -> u64
    where
        T: Copy + Into<u64>,
    {
        if n == 0 {
            t.into()
        } else {
            bound_rise(t, n - 1) + 1
        }
    }
}

/// The same requirement spelled three ways: bounds inline, the same bounds in another order, and
/// the same bounds in a where-clause. They are compared as sets, so this is one shared driver.
#[stack_safe]
mod spelled_differently {
    pub fn spelled_one<T: Copy + Into<u64>>(t: T, n: u64) -> u64 {
        if n == 0 {
            t.into()
        } else {
            spelled_two(t, n - 1) + 1
        }
    }
    pub fn spelled_two<T: Into<u64> + Copy>(t: T, n: u64) -> u64 {
        if n == 0 {
            t.into()
        } else {
            spelled_three(t, n - 1) + 1
        }
    }
    pub fn spelled_three<T>(t: T, n: u64) -> u64
    where
        T: Into<u64> + Copy,
    {
        if n == 0 {
            t.into()
        } else {
            spelled_one(t, n - 1) + 1
        }
    }
}

#[test]
fn signatures_a_shared_driver_can_name_agree_with_naive() {
    let f = |x: u64| x;
    for n in 0..10 {
        assert_eq!(trait_object::dyn_tick(&f, n), n, "dyn_tick({n})");
        assert_eq!(generic::gen_up(7u8, n), 7 + n, "gen_up({n})");
        assert_eq!(borrowed::borrow_walk("abcd", n), 4, "borrow_walk({n})");
        assert_eq!(bounded::bound_rise(7u8, n), 7 + n, "bound_rise({n})");
        assert_eq!(
            spelled_differently::spelled_one(7u8, n),
            7 + n,
            "spelled_one({n})"
        );
        assert_eq!(
            spelled_differently::spelled_three(7u8, n),
            7 + n,
            "spelled_three({n})"
        );
    }
}

#[test]
fn signatures_a_shared_driver_can_name_are_flat() {
    let depth = 200_000;
    assert_eq!(
        on_tiny_stack(move || trait_object::dyn_tick(&|x: u64| x, depth)),
        depth
    );
    assert_eq!(
        on_tiny_stack(move || generic::gen_up(7u8, depth)),
        7 + depth
    );
    assert_eq!(
        on_tiny_stack(move || borrowed::borrow_walk("abcd", depth)),
        4
    );
    assert_eq!(
        on_tiny_stack(move || bounded::bound_rise(7u8, depth)),
        7 + depth
    );
    assert_eq!(
        on_tiny_stack(move || spelled_differently::spelled_one(7u8, depth)),
        7 + depth
    );
}

// ---------------------------------------------------------------------------
// What a name means, which the scan has to read the way Rust does. Each of these once compiled and
// then gave a wrong answer, or was rejected for something it does not do.
// ---------------------------------------------------------------------------

/// A free function, which is what a bare call inside an impl block names: associated items are
/// never in scope under a bare name. Read as a call to the method beside it, `BareCall::doubler`
/// answered 0 for every input.
fn doubler(n: u64) -> u64 {
    n * 2
}

struct BareCall;

#[stack_safe]
impl BareCall {
    fn doubler(n: u64) -> u64 {
        if n == 0 { 0 } else { doubler(n - 1) }
    }

    /// A real recursion beside it, so the scan does group something in this block.
    fn depth(&self, n: u64) -> u64 {
        if n == 0 { 0 } else { 1 + self.depth(n - 1) }
    }
}

/// `Vec::new()` once gave a method called `new` a self-edge of its own, which put a `const fn` into
/// a group and rejected it for allocating.
struct Falsely {
    kids: Vec<Falsely>,
}

#[stack_safe]
impl Falsely {
    const fn new() -> Self {
        Falsely { kids: Vec::new() }
    }

    fn count(&self) -> usize {
        let mut n = 1;
        for kid in &self.kids {
            n += kid.count();
        }
        n
    }
}

#[test]
fn a_bare_call_in_an_impl_block_names_the_free_function() {
    for n in 1..8 {
        assert_eq!(BareCall::doubler(n), 2 * (n - 1), "doubler({n})");
    }
    assert_eq!(BareCall::doubler(0), 0);
    // The method beside it is genuinely recursive, and flat.
    assert_eq!(on_tiny_stack(|| BareCall.depth(200_000)), 200_000);
}

#[test]
fn a_method_call_on_a_value_is_not_a_call_to_a_member() {
    let t = Falsely {
        kids: vec![Falsely::new(), Falsely::new()],
    };
    assert_eq!(t.count(), 3);
}

// ---------------------------------------------------------------------------
// A trait impl is permitted, so long as no member of it recurses. A rewritten member would need a
// plain associated function beside it to carry the body, which such a block may not hold, and that
// is rejected by name — see `tests/ui/trait_impl_recursion.rs`. A recursion declared *inside* a
// member's body is another matter: its driver is written in that body.
// ---------------------------------------------------------------------------

struct Counted {
    v: u64,
    kids: Vec<Counted>,
}

trait Total {
    fn total(&self) -> u64;
    fn label(&self) -> &'static str;
}

#[stack_safe]
impl Total for Counted {
    fn total(&self) -> u64 {
        fn walk(t: &Counted) -> u64 {
            let mut n = t.v;
            for kid in &t.kids {
                n += walk(kid);
            }
            n
        }
        walk(self)
    }

    /// Recurses not at all, and is emitted as written.
    fn label(&self) -> &'static str {
        "counted"
    }
}

/// One child per level, so the depth is exactly `depth`.
fn counted_spine(depth: u64) -> Counted {
    let mut t = Counted {
        v: 1,
        kids: Vec::new(),
    };
    for _ in 0..depth {
        t = Counted {
            v: 1,
            kids: vec![t],
        };
    }
    t
}

/// `Counted`'s own `Drop` recurses, which would overflow the tiny stack before the rewritten
/// function did.
fn drop_counted(t: Counted) {
    let mut stack = vec![t];
    while let Some(mut node) = stack.pop() {
        stack.append(&mut node.kids);
    }
}

#[test]
fn a_trait_impl_may_hold_a_recursion_in_a_body() {
    let t = counted_spine(3);
    assert_eq!(t.total(), 4);
    assert_eq!(t.label(), "counted");
    drop_counted(t);

    let depth = 200_000;
    let got = on_tiny_stack(move || {
        let t = counted_spine(depth);
        let total = t.total();
        drop_counted(t);
        total
    });
    assert_eq!(got, depth + 1);
}

// ---------------------------------------------------------------------------
// The unsafe options across a *cycle*. Each is covered alone for a two-member cycle above; what
// follows covers them in longer cycles, in one argument list together, and between methods. A cycle
// shares one driver, so its members share one context tuple and one set of stores: the slot a member
// parks has to be the slot the next member re-derives from, and a value one member lends has to
// outlive the whole subtree of another.
// ---------------------------------------------------------------------------

/// One link per level, so the trail's own depth is the recursion's.
enum Trail<'a> {
    Nil,
    Cons(u64, &'a Trail<'a>),
}

fn trail_top(t: &Trail<'_>) -> u64 {
    match t {
        Trail::Nil => 0,
        Trail::Cons(d, _) => *d,
    }
}

/// Walked once at a leaf, checking every link is still the one built for it.
fn trail_intact(t: &Trail<'_>) -> u64 {
    let (mut links, mut cur) = (0, t);
    while let Trail::Cons(d, parent) = cur {
        assert_eq!(
            trail_top(parent) + 1,
            *d,
            "each link is one further from the root"
        );
        links += 1;
        cur = parent;
    }
    links
}

/// A three-member cycle handing a derived place along, so the slot is parked and restored by a
/// different member each time round.
#[stack_safe(use_nonlinear_mut)]
mod unsafe_options_derived {
    use super::Node;

    pub fn one(t: &mut Node) -> u64 {
        t.v += 1;
        let mut acc = t.v;
        for i in 0..t.kids.len() {
            acc += two(&mut t.kids[i]);
        }
        acc
    }

    pub fn two(t: &mut Node) -> u64 {
        t.v += 1;
        let mut acc = t.v;
        for i in 0..t.kids.len() {
            acc += three(&mut t.kids[i]);
        }
        acc
    }

    pub fn three(t: &mut Node) -> u64 {
        t.v += 1;
        let mut acc = t.v;
        for i in 0..t.kids.len() {
            acc += one(&mut t.kids[i]);
        }
        acc
    }
}

fn derived_one_naive(t: &mut Node) -> u64 {
    t.v += 1;
    let mut acc = t.v;
    for i in 0..t.kids.len() {
        acc += derived_two_naive(&mut t.kids[i]);
    }
    acc
}

fn derived_two_naive(t: &mut Node) -> u64 {
    t.v += 1;
    let mut acc = t.v;
    for i in 0..t.kids.len() {
        acc += derived_three_naive(&mut t.kids[i]);
    }
    acc
}

fn derived_three_naive(t: &mut Node) -> u64 {
    t.v += 1;
    let mut acc = t.v;
    for i in 0..t.kids.len() {
        acc += derived_one_naive(&mut t.kids[i]);
    }
    acc
}

/// A three-member cycle lending values it builds, each member growing a different trail.
#[stack_safe(data_in_frame)]
mod unsafe_options_lent {
    use super::{Trail, trail_intact, trail_top};

    pub fn first(n: u64, t: &Trail<'_>) -> u64 {
        if trail_top(t) >= n {
            trail_intact(t)
        } else {
            second(n, &Trail::Cons(trail_top(t) + 1, t))
        }
    }

    pub fn second(n: u64, t: &Trail<'_>) -> u64 {
        if trail_top(t) >= n {
            trail_intact(t)
        } else {
            third(n, &Trail::Cons(trail_top(t) + 1, t))
        }
    }

    pub fn third(n: u64, t: &Trail<'_>) -> u64 {
        if trail_top(t) >= n {
            trail_intact(t)
        } else {
            first(n, &Trail::Cons(trail_top(t) + 1, t))
        }
    }
}

fn lent_first_naive(n: u64, t: &Trail<'_>) -> u64 {
    if trail_top(t) >= n {
        trail_intact(t)
    } else {
        lent_second_naive(n, &Trail::Cons(trail_top(t) + 1, t))
    }
}

fn lent_second_naive(n: u64, t: &Trail<'_>) -> u64 {
    if trail_top(t) >= n {
        trail_intact(t)
    } else {
        lent_third_naive(n, &Trail::Cons(trail_top(t) + 1, t))
    }
}

fn lent_third_naive(n: u64, t: &Trail<'_>) -> u64 {
    if trail_top(t) >= n {
        trail_intact(t)
    } else {
        lent_first_naive(n, &Trail::Cons(trail_top(t) + 1, t))
    }
}

/// Both options, in one argument list, between two members: each hands the other a place derived
/// from its own `&mut` and, beside it, a trail link built at the call site.
#[stack_safe(use_nonlinear_mut, data_in_frame)]
mod unsafe_options_both {
    use super::{Node, Trail, trail_intact, trail_top};

    pub fn down(t: &mut Node, trail: &Trail<'_>) -> u64 {
        t.v = trail_top(trail);
        if t.kids.is_empty() {
            return trail_intact(trail) + t.v;
        }
        let mut acc = t.v;
        for i in 0..t.kids.len() {
            acc += up(&mut t.kids[i], &Trail::Cons(trail_top(trail) + 1, trail));
        }
        acc
    }

    pub fn up(t: &mut Node, trail: &Trail<'_>) -> u64 {
        t.v = trail_top(trail);
        if t.kids.is_empty() {
            return trail_intact(trail) + t.v;
        }
        let mut acc = t.v;
        for i in 0..t.kids.len() {
            acc += down(&mut t.kids[i], &Trail::Cons(trail_top(trail) + 1, trail));
        }
        acc
    }
}

fn both_down_naive(t: &mut Node, trail: &Trail<'_>) -> u64 {
    t.v = trail_top(trail);
    if t.kids.is_empty() {
        return trail_intact(trail) + t.v;
    }
    let mut acc = t.v;
    for i in 0..t.kids.len() {
        acc += both_up_naive(&mut t.kids[i], &Trail::Cons(trail_top(trail) + 1, trail));
    }
    acc
}

fn both_up_naive(t: &mut Node, trail: &Trail<'_>) -> u64 {
    t.v = trail_top(trail);
    if t.kids.is_empty() {
        return trail_intact(trail) + t.v;
    }
    let mut acc = t.v;
    for i in 0..t.kids.len() {
        acc += both_down_naive(&mut t.kids[i], &Trail::Cons(trail_top(trail) + 1, trail));
    }
    acc
}

struct Sides;

/// The same, between *methods*, where the receiver is a context slot of its own beside the tree.
#[stack_safe(use_nonlinear_mut, data_in_frame)]
impl Sides {
    fn left(&self, t: &mut Node, trail: &Trail<'_>) -> u64 {
        t.v = trail_top(trail);
        if t.kids.is_empty() {
            return trail_intact(trail) + t.v;
        }
        let mut acc = t.v;
        for i in 0..t.kids.len() {
            acc += self.right(&mut t.kids[i], &Trail::Cons(trail_top(trail) + 1, trail));
        }
        acc
    }

    fn right(&self, t: &mut Node, trail: &Trail<'_>) -> u64 {
        t.v = trail_top(trail);
        if t.kids.is_empty() {
            return trail_intact(trail) + t.v;
        }
        let mut acc = t.v;
        for i in 0..t.kids.len() {
            acc += self.left(&mut t.kids[i], &Trail::Cons(trail_top(trail) + 1, trail));
        }
        acc
    }
}

/// One child per level, so the depth is exactly `depth`.
fn node_spine(depth: u64) -> Node {
    let mut t = Node {
        v: 0,
        kids: Vec::new(),
    };
    for _ in 0..depth {
        t = Node {
            v: 0,
            kids: vec![t],
        };
    }
    t
}

/// `Node`'s own `Drop` recurses, which would overflow the tiny stack before the rewritten
/// functions did.
fn drop_nodes(t: Node) {
    let mut stack = vec![t];
    while let Some(mut node) = stack.pop() {
        stack.append(&mut node.kids);
    }
}

#[test]
fn unsafe_options_across_a_cycle_agree_with_naive() {
    let (mut a, mut b) = (bushy_tree(4), bushy_tree(4));
    assert_eq!(
        unsafe_options_derived::one(&mut a),
        derived_one_naive(&mut b),
        "a derived place handed round a three-member cycle"
    );
    drop_nodes(a);
    drop_nodes(b);

    for n in 0..8 {
        assert_eq!(
            unsafe_options_lent::first(n, &Trail::Nil),
            lent_first_naive(n, &Trail::Nil),
            "values lent round a three-member cycle, n = {n}"
        );
    }

    let (mut a, mut b) = (bushy_tree(4), bushy_tree(4));
    assert_eq!(
        unsafe_options_both::down(&mut a, &Trail::Nil),
        both_down_naive(&mut b, &Trail::Nil),
        "both options in one argument list, between two members"
    );
    drop_nodes(a);
    drop_nodes(b);

    let (mut a, mut b) = (bushy_tree(4), bushy_tree(4));
    assert_eq!(
        Sides.left(&mut a, &Trail::Nil),
        both_down_naive(&mut b, &Trail::Nil),
        "and between two methods"
    );
    drop_nodes(a);
    drop_nodes(b);
}

#[test]
fn unsafe_options_across_a_cycle_is_flat() {
    let depth = 100_000;
    let derived = on_tiny_stack(move || {
        let mut t = node_spine(depth);
        let out = unsafe_options_derived::one(&mut t);
        drop_nodes(t);
        out
    });
    assert_eq!(derived, depth + 1, "one visit per node");

    let lent = on_tiny_stack(move || unsafe_options_lent::first(depth, &Trail::Nil));
    assert_eq!(lent, depth, "one link per level");

    let both = on_tiny_stack(move || {
        let mut t = node_spine(depth);
        let out = unsafe_options_both::down(&mut t, &Trail::Nil);
        drop_nodes(t);
        out
    });
    let methods = on_tiny_stack(move || {
        let mut t = node_spine(depth);
        let out = Sides.left(&mut t, &Trail::Nil);
        drop_nodes(t);
        out
    });
    assert_eq!(
        both, methods,
        "the module cycle and the method cycle count alike"
    );
}
