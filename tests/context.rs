// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Tests for `&mut` parameters and methods, which travel through the driver as a
//! *context* rather than in the argument payload.
//!
//! The property under test is the one the payload could not give: a `&mut` stays
//! usable *after* a recursive call returns, at every level of the recursion. Each
//! case therefore has code following the recursive call that touches the
//! reference — a version that only tail-called would compile even without the
//! context.
//!
//! As everywhere else in this suite, the depth tests run on a 64 KiB stack, so a
//! regression to native recursion aborts the process rather than failing quietly.
//!
//! The `use_nonlinear_mut` cases are also exercised under Miri (both
//! Stacked Borrows and Tree Borrows); see README.md.

use yaspar_macros::stack_safe;

const TINY_STACK: usize = 64 * 1024;

/// Miri is orders of magnitude slower than native, so the depth tests shrink
/// under it. They still cover every path — including the pointer park/restore of
/// `use_nonlinear_mut`, which is the reason to run Miri at all — just not
/// at a depth that would overflow a native stack.
#[cfg(miri)]
const DEEP: u64 = 300;
#[cfg(not(miri))]
const DEEP: u64 = 200_000;

fn on_tiny_stack<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    std::thread::Builder::new()
        .stack_size(TINY_STACK)
        .spawn(f)
        .expect("spawn")
        .join()
        .expect("join")
}

// ---------------------------------------------------------------------------
// A `&mut` parameter used non-linearly: two sequential recursive calls, plus a
// use after the last one. This is the E0505 case the payload cannot express.
// ---------------------------------------------------------------------------

#[stack_safe]
fn collect(n: u64, out: &mut Vec<u64>) {
    if n == 0 {
        return;
    }
    out.push(n);
    collect(n / 2, out);
    collect(n / 3, out);
    out.push(n);
}

fn collect_naive(n: u64, out: &mut Vec<u64>) {
    if n == 0 {
        return;
    }
    out.push(n);
    collect_naive(n / 2, out);
    collect_naive(n / 3, out);
    out.push(n);
}

#[test]
fn non_linear_mut_param_agrees_with_naive() {
    for n in 0..40 {
        let (mut a, mut b) = (Vec::new(), Vec::new());
        collect(n, &mut a);
        collect_naive(n, &mut b);
        assert_eq!(a, b, "n = {n}");
    }
}

#[stack_safe]
fn chain(n: u64, out: &mut Vec<u64>) {
    if n == 0 {
        return;
    }
    out.push(n);
    chain(n - 1, out);
    // Reached only after the whole subtree has run, so `out` has to survive it.
    out.push(n);
}

#[test]
fn deep_mut_param_is_flat() {
    let len = on_tiny_stack(|| {
        let mut v = Vec::new();
        chain(DEEP, &mut v);
        v.len()
    });
    assert_eq!(len as u64, 2 * DEEP);
}

// ---------------------------------------------------------------------------
// A `&mut` parameter across a lowered loop: the reference must be reachable from
// the loop's entry point without travelling in its state tuple.
// ---------------------------------------------------------------------------

#[stack_safe]
fn fan(n: u64, out: &mut Vec<u64>) {
    out.push(n);
    for k in 1..n {
        if k * k > n {
            break;
        }
        fan(k, out);
        out.push(0);
    }
}

fn fan_naive(n: u64, out: &mut Vec<u64>) {
    out.push(n);
    for k in 1..n {
        if k * k > n {
            break;
        }
        fan_naive(k, out);
        out.push(0);
    }
}

#[test]
fn mut_param_across_a_loop() {
    for n in 0..40 {
        let (mut a, mut b) = (Vec::new(), Vec::new());
        fan(n, &mut a);
        fan_naive(n, &mut b);
        assert_eq!(a, b, "n = {n}");
    }
}

// ---------------------------------------------------------------------------
// Methods. An arena tree keeps building and dropping the input iterative, so the
// only thing the tiny stack measures is the transformed function.
// ---------------------------------------------------------------------------

struct Node {
    val: u64,
    kids: Vec<usize>,
}

struct Visitor {
    nodes: Vec<Node>,
    log: Vec<u64>,
    total: u64,
}

impl Visitor {
    /// `&mut self`, with mutation of `self` both before and after the recursion.
    #[stack_safe]
    fn visit(&mut self, i: usize) -> u64 {
        self.log.push(self.nodes[i].val);
        let mut acc = self.nodes[i].val;
        for k in 0..self.nodes[i].kids.len() {
            let kid = self.nodes[i].kids[k];
            acc += self.visit(kid);
        }
        self.total += acc;
        acc
    }

    /// `&self`, called through the explicit `Self::f(self, ..)` form.
    #[stack_safe]
    fn depth(&self, i: usize) -> u64 {
        let mut best = 0;
        for k in 0..self.nodes[i].kids.len() {
            let d = Self::depth(self, self.nodes[i].kids[k]);
            if d > best {
                best = d;
            }
        }
        best + 1
    }
}

/// A chain `0 -> 1 -> .. -> n`, in an arena.
fn arena_chain(n: usize) -> Vec<Node> {
    (0..=n)
        .map(|i| Node {
            val: 1,
            kids: if i < n { vec![i + 1] } else { vec![] },
        })
        .collect()
}

#[test]
fn deep_mut_self_method_is_flat() {
    let n = DEEP as usize / 2;
    let (sum, log, total, depth) = on_tiny_stack(move || {
        let mut v = Visitor {
            nodes: arena_chain(n),
            log: Vec::new(),
            total: 0,
        };
        let sum = v.visit(0);
        let depth = v.depth(0);
        (sum, v.log.len(), v.total, depth)
    });
    assert_eq!(sum, n as u64 + 1);
    assert_eq!(log, n + 1);
    assert_eq!(depth, n as u64 + 1);
    // Every prefix sum, i.e. 1 + 2 + .. + (n+1).
    let k = n as u64 + 1;
    assert_eq!(total, k * (k + 1) / 2);
}

// ---------------------------------------------------------------------------
// `use_nonlinear_mut`: the child works on a place *derived* from the
// parent's reference, so the pointer is parked for the subtree and restored.
// ---------------------------------------------------------------------------

struct Tree {
    v: u64,
    kids: Vec<Tree>,
}

#[stack_safe(use_nonlinear_mut)]
fn bump(t: &mut Tree) -> u64 {
    t.v += 1;
    let mut acc = t.v;
    for i in 0..t.kids.len() {
        acc += bump(&mut t.kids[i]);
        // After the child's subtree: the parent's pointer must be back.
        t.v += 1;
        acc += 1;
    }
    acc
}

fn bump_naive(t: &mut Tree) -> u64 {
    t.v += 1;
    let mut acc = t.v;
    for i in 0..t.kids.len() {
        acc += bump_naive(&mut t.kids[i]);
        t.v += 1;
        acc += 1;
    }
    acc
}

/// `Tree`'s own `Drop` recurses, which would overflow the tiny stack long before
/// the transformed function did. Taking it apart breadth-first keeps the teardown
/// out of the measurement — the tree is not what is under test.
fn drop_iteratively(t: Tree) {
    let mut stack = vec![t];
    while let Some(mut node) = stack.pop() {
        stack.append(&mut node.kids);
    }
}

fn bushy(d: u64) -> Tree {
    if d == 0 {
        return Tree {
            v: 1,
            kids: Vec::new(),
        };
    }
    Tree {
        v: d,
        kids: (0..3).map(|_| bushy(d - 1)).collect(),
    }
}

#[test]
fn derived_reborrow_agrees_with_naive() {
    let (mut a, mut b) = (bushy(5), bushy(5));
    assert_eq!(bump(&mut a), bump_naive(&mut b));
    // ...and the trees themselves must match, not just the totals.
    fn same(x: &Tree, y: &Tree) -> bool {
        x.v == y.v
            && x.kids.len() == y.kids.len()
            && x.kids.iter().zip(&y.kids).all(|(a, b)| same(a, b))
    }
    assert!(same(&a, &b));
}

#[test]
fn deep_derived_reborrow_is_flat() {
    let total = on_tiny_stack(|| {
        let mut t = Tree {
            v: 0,
            kids: Vec::new(),
        };
        for _ in 0..DEEP {
            t = Tree {
                v: 0,
                kids: vec![t],
            };
        }
        let r = bump(&mut t);
        drop_iteratively(t);
        r
    });
    // The leaf contributes 1 (its own `v` after the increment); each inner node
    // adds its own `v` plus the `acc += 1` after its single child, i.e. 2 per
    // level.
    assert_eq!(total, 1 + 2 * DEEP);
}

// ---------------------------------------------------------------------------
// Ordering around a swapped slot. The payload arguments have to be evaluated
// *before* the derived pointer is taken, and both failures this prevents were
// silent: one was UB, the other a wrong answer.
// ---------------------------------------------------------------------------

struct Chain {
    v: u64,
    kids: Vec<Chain>,
}

/// A payload argument that *writes through* the context reference. Evaluating it
/// after `ptr::from_mut(&mut t.kids[0])` is a foreign write to a derived pointer
/// that already exists — Miri rejected it under both aliasing models.
#[stack_safe(use_nonlinear_mut)]
fn writes_through(n: u64, t: &mut Chain) -> u64 {
    if t.kids.is_empty() {
        return t.v + n;
    }
    let k = writes_through(
        {
            t.kids[0].v += 1;
            0
        },
        &mut t.kids[0],
    );
    t.v + k
}

fn writes_through_naive(n: u64, t: &mut Chain) -> u64 {
    if t.kids.is_empty() {
        return t.v + n;
    }
    let k = writes_through_naive(
        {
            t.kids[0].v += 1;
            0
        },
        &mut t.kids[0],
    );
    t.v + k
}

#[test]
fn payload_that_writes_through_the_context_is_ordered_before_the_swap() {
    let mut a = Chain {
        v: 1,
        kids: vec![Chain {
            v: 2,
            kids: Vec::new(),
        }],
    };
    let mut b = Chain {
        v: 1,
        kids: vec![Chain {
            v: 2,
            kids: Vec::new(),
        }],
    };
    assert_eq!(writes_through(0, &mut a), writes_through_naive(0, &mut b));
    assert_eq!(a.kids[0].v, b.kids[0].v);
}

/// A payload argument that *escapes* — here `continue`. It leaves before the swap
/// happens, so there is nothing to restore; when the swap came first, the slot was
/// left pointing at the child and the parent then walked the wrong node.
#[stack_safe(use_nonlinear_mut)]
fn escaping_payload(n: u64, t: &mut Chain) -> u64 {
    let mut acc = t.v + n;
    for i in 0..t.kids.len() {
        acc += escaping_payload(
            if t.kids[i].v.is_multiple_of(2) {
                continue;
            } else {
                n + 1
            },
            &mut t.kids[i],
        );
        t.v += 1;
    }
    acc + t.v
}

fn escaping_payload_naive(n: u64, t: &mut Chain) -> u64 {
    let mut acc = t.v + n;
    for i in 0..t.kids.len() {
        acc += escaping_payload_naive(
            if t.kids[i].v.is_multiple_of(2) {
                continue;
            } else {
                n + 1
            },
            &mut t.kids[i],
        );
        t.v += 1;
    }
    acc + t.v
}

fn bushy_chain(depth: u64) -> Chain {
    if depth == 0 {
        return Chain {
            v: 1,
            kids: Vec::new(),
        };
    }
    Chain {
        v: depth,
        kids: (0..2).map(|_| bushy_chain(depth - 1)).collect(),
    }
}

fn same_shape(a: &Chain, b: &Chain) -> bool {
    a.v == b.v
        && a.kids.len() == b.kids.len()
        && a.kids.iter().zip(&b.kids).all(|(x, y)| same_shape(x, y))
}

#[test]
fn escaping_payload_argument_leaves_the_slot_intact() {
    let (mut a, mut b) = (bushy_chain(3), bushy_chain(3));
    assert_eq!(
        escaping_payload(0, &mut a),
        escaping_payload_naive(0, &mut b)
    );
    // The tree itself has to match: a stale slot would have mutated the wrong node.
    assert!(same_shape(&a, &b));
}

/// Argument evaluation order around a swapped slot. The derived pointer must be taken
/// last — user code running between its creation and the callee's use of it is either
/// a foreign write to it or an escape that skips the restore — so the side effects
/// *inside* the place are hoisted to where the source put them instead. The order is
/// then the source's, whichever argument comes first.
#[test]
fn argument_order_around_a_swap_matches_the_source() {
    use std::cell::RefCell;

    struct Node {
        v: u64,
        kids: Vec<Node>,
    }

    fn pick(log: &RefCell<Vec<&'static str>>) -> usize {
        log.borrow_mut().push("derived");
        0
    }
    fn arg(log: &RefCell<Vec<&'static str>>, n: u64) -> u64 {
        log.borrow_mut().push("payload");
        n
    }

    // The derived argument is written *first*, so its side effect must run first.
    #[stack_safe(use_nonlinear_mut)]
    fn f(t: &mut Node, n: u64, log: &RefCell<Vec<&'static str>>) -> u64 {
        if t.kids.is_empty() || n == 0 {
            return t.v;
        }
        f(&mut t.kids[pick(log)], arg(log, n - 1), log)
    }

    fn naive(t: &mut Node, n: u64, log: &RefCell<Vec<&'static str>>) -> u64 {
        if t.kids.is_empty() || n == 0 {
            return t.v;
        }
        naive(&mut t.kids[pick(log)], arg(log, n - 1), log)
    }

    fn tree() -> Node {
        Node {
            v: 9,
            kids: vec![Node {
                v: 8,
                kids: Vec::new(),
            }],
        }
    }

    let (a_log, b_log) = (RefCell::new(Vec::new()), RefCell::new(Vec::new()));
    let (mut a, mut b) = (tree(), tree());
    assert_eq!(f(&mut a, 1, &a_log), naive(&mut b, 1, &b_log));
    assert_eq!(*a_log.borrow(), *b_log.borrow());
    assert_eq!(*a_log.borrow(), ["derived", "payload"]);
}

/// A *custom* `IndexMut` is user code with observable effects, and it runs when the
/// derived pointer is taken. That pointer is taken where the source takes it, so the
/// effect lands in source order — which is what makes this agree with plain recursion.
///
/// What licenses taking the pointer that early is borrowck: an argument written after
/// a `&mut` place cannot touch the parent (`f(&mut b[0], b.len())` is E0502), so
/// nothing between taking the pointer and the callee's use of it can invalidate it.
#[test]
fn a_custom_projection_runs_in_source_order() {
    use std::ops::{Index, IndexMut};
    use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

    static HITS: AtomicU64 = AtomicU64::new(0);

    struct Bag {
        v: u64,
        kids: Vec<Bag>,
    }

    impl Index<usize> for Bag {
        type Output = Bag;
        fn index(&self, i: usize) -> &Bag {
            &self.kids[i]
        }
    }
    impl IndexMut<usize> for Bag {
        fn index_mut(&mut self, i: usize) -> &mut Bag {
            HITS.fetch_add(1, Relaxed);
            &mut self.kids[i]
        }
    }

    // As written: the payload argument reads what the projection writes.
    #[stack_safe(use_nonlinear_mut)]
    fn f(t: &mut Bag, acc: u64) -> u64 {
        if t.kids.is_empty() {
            return acc + t.v;
        }
        f(&mut t[0], acc + HITS.load(Relaxed))
    }
    fn naive(t: &mut Bag, acc: u64) -> u64 {
        if t.kids.is_empty() {
            return acc + t.v;
        }
        naive(&mut t[0], acc + HITS.load(Relaxed))
    }

    // Hoisted by hand: the order is explicit, so both agree.
    #[stack_safe(use_nonlinear_mut)]
    fn hoisted(t: &mut Bag, acc: u64) -> u64 {
        if t.kids.is_empty() {
            return acc + t.v;
        }
        let next = acc + HITS.load(Relaxed);
        hoisted(&mut t[0], next)
    }
    fn hoisted_naive(t: &mut Bag, acc: u64) -> u64 {
        if t.kids.is_empty() {
            return acc + t.v;
        }
        let next = acc + HITS.load(Relaxed);
        hoisted_naive(&mut t[0], next)
    }

    fn bag(d: u64) -> Bag {
        if d == 0 {
            return Bag {
                v: 0,
                kids: Vec::new(),
            };
        }
        Bag {
            v: 0,
            kids: vec![bag(d - 1)],
        }
    }
    fn run(g: impl Fn(&mut Bag, u64) -> u64) -> u64 {
        HITS.store(0, Relaxed);
        g(&mut bag(4), 0)
    }

    // The projection runs before the argument that reads its effect, as written.
    assert_eq!(run(f), run(naive));
    assert_eq!(run(f), 10);
    // And with the argument bound out by hand, which orders it the other way.
    assert_eq!(run(hoisted), run(hoisted_naive));
    assert_eq!(run(hoisted), 6);
}

// ---------------------------------------------------------------------------
// The same, written as a method. A receiver is desugared into an ordinary
// parameter, so `self.kids[i].bump_method()` is a derived `&mut Self` exactly as
// `bump(&mut t.kids[i])` is, and obeys the same `use_nonlinear_mut` rule.
// ---------------------------------------------------------------------------

impl Tree {
    #[stack_safe(use_nonlinear_mut)]
    fn bump_method(&mut self) -> u64 {
        self.v += 1;
        let mut acc = self.v;
        for i in 0..self.kids.len() {
            acc += self.kids[i].bump_method();
            // After the child's subtree: the parent's pointer must be back.
            self.v += 1;
            acc += 1;
        }
        acc
    }

    /// A `&self` method recursing on a child, which needs no opt-in at all: a shared
    /// reference is `Copy`, so it travels in the payload.
    #[stack_safe]
    fn total(&self) -> u64 {
        let mut acc = self.v;
        for k in &self.kids {
            acc += k.total();
        }
        acc
    }
}

fn total_naive(t: &Tree) -> u64 {
    let mut acc = t.v;
    for k in &t.kids {
        acc += total_naive(k);
    }
    acc
}

#[test]
fn method_recursing_into_a_derived_receiver() {
    let (mut a, mut b) = (bushy(4), bushy(4));
    assert_eq!(a.bump_method(), bump_naive(&mut b));
    assert_eq!(a.total(), b.total());
}

#[test]
fn shared_method_recursing_into_a_child() {
    let t = bushy(4);
    assert_eq!(t.total(), total_naive(&t));
}

#[test]
fn method_receiver_recursion_is_stack_safe() {
    let depth = 100_000;
    let got = on_tiny_stack(move || {
        let mut deep = spine(depth);
        let bumped = deep.bump_method();
        let totalled = deep.total();
        drop_iteratively(deep);
        (bumped, totalled)
    });
    // Each node ends at `v == 3` except the leaf at 2, and `bump_method` counts the
    // same total; checked against `bump_naive` at small depths above.
    assert_eq!(got.0, 3 * (depth + 1) - 1);
    assert_eq!(got.1, got.0);
}

/// One child per level, so the depth is exactly `depth`.
fn spine(depth: u64) -> Tree {
    let mut t = Tree {
        v: 1,
        kids: Vec::new(),
    };
    for _ in 0..depth {
        t = Tree {
            v: 1,
            kids: vec![t],
        };
    }
    t
}

// ---------------------------------------------------------------------------
// Both options at once, at the same call site: the child works on a place *derived* from the
// parent's `&mut`, and is also lent a path the call site *builds*. Each was covered alone. Put
// together, the built value was never moved into the driver's store — the callee was handed a
// reference to a temporary that had already died, which segfaulted rather than failing quietly.
// ---------------------------------------------------------------------------

enum Path<'a> {
    Nil,
    /// How far from the root, so that a callee can read its own depth without walking the chain.
    Cons(u64, &'a Path<'a>),
}

/// The depth this link records, in constant time.
fn depth_of(p: &Path<'_>) -> u64 {
    match p {
        Path::Nil => 0,
        Path::Cons(d, _) => *d,
    }
}

/// The whole chain, walked once, checking that every link is still the one built for it: each
/// records its own depth, and its parent is one nearer the root. That is what the driver's store
/// promises for as long as the frame that built a value lives.
fn walk_to_root(p: &Path<'_>) -> u64 {
    let (mut links, mut cur) = (0, p);
    while let Path::Cons(d, parent) = cur {
        assert_eq!(*d, depth_of(cur), "each link records its own depth");
        assert_eq!(
            depth_of(parent) + 1,
            *d,
            "and its parent is one nearer the root"
        );
        links += 1;
        cur = parent;
    }
    links
}

/// Writes each node's distance from the root, and answers the deepest one. The path is read in
/// constant time on the way down and walked in full at the leaves, so a deep tree costs the
/// recursion rather than the check.
#[stack_safe(use_nonlinear_mut, data_in_frame)]
fn label(t: &mut Tree, path: &Path<'_>) -> u64 {
    t.v = depth_of(path);
    if t.kids.is_empty() {
        return walk_to_root(path).max(t.v);
    }
    let mut deepest = t.v;
    for i in 0..t.kids.len() {
        deepest = deepest.max(label(&mut t.kids[i], &Path::Cons(t.v + 1, path)));
    }
    deepest
}

fn label_naive(t: &mut Tree, path: &Path<'_>) -> u64 {
    t.v = depth_of(path);
    if t.kids.is_empty() {
        return walk_to_root(path).max(t.v);
    }
    let mut deepest = t.v;
    for i in 0..t.kids.len() {
        deepest = deepest.max(label_naive(&mut t.kids[i], &Path::Cons(t.v + 1, path)));
    }
    deepest
}

fn same_labels(a: &Tree, b: &Tree) -> bool {
    let mut stack = vec![(a, b)];
    while let Some((x, y)) = stack.pop() {
        if x.v != y.v || x.kids.len() != y.kids.len() {
            return false;
        }
        stack.extend(x.kids.iter().zip(&y.kids));
    }
    true
}

#[test]
fn a_derived_place_and_a_built_value_in_one_call_agree_with_naive() {
    let (mut a, mut b) = (bushy(4), bushy(4));
    assert_eq!(label(&mut a, &Path::Nil), label_naive(&mut b, &Path::Nil));
    assert!(same_labels(&a, &b), "every node carries its own depth");
    drop_iteratively(a);
    drop_iteratively(b);
}

#[test]
fn a_derived_place_and_a_built_value_in_one_call_is_flat() {
    let depth = 200_000;
    let got = on_tiny_stack(move || {
        let mut t = spine(depth);
        let deepest = label(&mut t, &Path::Nil);
        drop_iteratively(t);
        deepest
    });
    // The spine's leaf is `depth` links from the root, and the path is built one link per level.
    assert_eq!(got, depth);
}

// ---------------------------------------------------------------------------
// The options in combination, over the call shapes each is sensitive to. `use_nonlinear_mut` parks a
// pointer for the child's subtree; `data_in_frame` moves a value into the driver's store. Each was
// covered alone, and their combination was not, which is how a segfault survived: the value was
// never stored, since arguments after a parked pointer were re-read from the source. The order of
// the two in the argument list matters, so both orders are here, with and without a loop.
// ---------------------------------------------------------------------------

enum Trail<'a> {
    Nil,
    /// How many links to the root, so a callee reads its own depth without walking the chain.
    Cons(u64, &'a Trail<'a>),
}

fn trail_depth(t: &Trail<'_>) -> u64 {
    match t {
        Trail::Nil => 0,
        Trail::Cons(d, _) => *d,
    }
}

/// The whole trail, walked once at a leaf, checking every link is the one built for it.
fn walk_trail(t: &Trail<'_>) -> u64 {
    let (mut links, mut cur) = (0, t);
    while let Trail::Cons(d, parent) = cur {
        assert_eq!(
            trail_depth(parent) + 1,
            *d,
            "each link is one further from the root"
        );
        links += 1;
        cur = parent;
    }
    links
}

/// The built value first, the derived place second, in a loop.
#[stack_safe(use_nonlinear_mut, data_in_frame)]
fn built_first(trail: &Trail<'_>, t: &mut Tree) -> u64 {
    t.v += 1;
    if t.kids.is_empty() {
        return walk_trail(trail) + t.v;
    }
    let mut acc = trail_depth(trail) + t.v;
    for i in 0..t.kids.len() {
        acc += built_first(&Trail::Cons(trail_depth(trail) + 1, trail), &mut t.kids[i]);
    }
    acc
}

fn built_first_naive(trail: &Trail<'_>, t: &mut Tree) -> u64 {
    t.v += 1;
    if t.kids.is_empty() {
        return walk_trail(trail) + t.v;
    }
    let mut acc = trail_depth(trail) + t.v;
    for i in 0..t.kids.len() {
        acc += built_first_naive(&Trail::Cons(trail_depth(trail) + 1, trail), &mut t.kids[i]);
    }
    acc
}

/// A plain argument between the two, so neither is the one the other is hoisted around.
#[stack_safe(use_nonlinear_mut, data_in_frame)]
fn plain_between(trail: &Trail<'_>, k: u64, t: &mut Tree) -> u64 {
    t.v += 1;
    if t.kids.is_empty() {
        return walk_trail(trail) + k + t.v;
    }
    let mut acc = trail_depth(trail) + t.v;
    for i in 0..t.kids.len() {
        acc += plain_between(
            &Trail::Cons(trail_depth(trail) + 1, trail),
            k + 1,
            &mut t.kids[i],
        );
    }
    acc
}

fn plain_between_naive(trail: &Trail<'_>, k: u64, t: &mut Tree) -> u64 {
    t.v += 1;
    if t.kids.is_empty() {
        return walk_trail(trail) + k + t.v;
    }
    let mut acc = trail_depth(trail) + t.v;
    for i in 0..t.kids.len() {
        acc += plain_between_naive(
            &Trail::Cons(trail_depth(trail) + 1, trail),
            k + 1,
            &mut t.kids[i],
        );
    }
    acc
}

/// No loop: one recursive call carrying both. This is the shape that segfaulted.
#[stack_safe(use_nonlinear_mut, data_in_frame)]
fn no_loop(trail: &Trail<'_>, t: &mut Tree) -> u64 {
    t.v += 1;
    if t.kids.is_empty() {
        return walk_trail(trail) + t.v;
    }
    no_loop(&Trail::Cons(trail_depth(trail) + 1, trail), &mut t.kids[0])
}

fn no_loop_naive(trail: &Trail<'_>, t: &mut Tree) -> u64 {
    t.v += 1;
    if t.kids.is_empty() {
        return walk_trail(trail) + t.v;
    }
    no_loop_naive(&Trail::Cons(trail_depth(trail) + 1, trail), &mut t.kids[0])
}

#[test]
fn the_two_options_agree_with_naive_in_every_argument_order() {
    let (mut a, mut b) = (bushy(4), bushy(4));
    assert_eq!(
        built_first(&Trail::Nil, &mut a),
        built_first_naive(&Trail::Nil, &mut b),
    );
    drop_iteratively(a);
    drop_iteratively(b);

    let (mut a, mut b) = (bushy(4), bushy(4));
    assert_eq!(
        plain_between(&Trail::Nil, 0, &mut a),
        plain_between_naive(&Trail::Nil, 0, &mut b),
    );
    drop_iteratively(a);
    drop_iteratively(b);

    let (mut a, mut b) = (spine(6), spine(6));
    assert_eq!(
        no_loop(&Trail::Nil, &mut a),
        no_loop_naive(&Trail::Nil, &mut b)
    );
    drop_iteratively(a);
    drop_iteratively(b);
}

#[test]
fn the_two_options_in_every_argument_order_is_flat() {
    let depth = 100_000;
    // `plain_between` takes an extra argument, so each is wrapped to one shape.
    let shapes: [fn(&Trail<'_>, &mut Tree) -> u64; 3] =
        [built_first, |trail, t| plain_between(trail, 0, t), no_loop];
    for f in shapes {
        let got = on_tiny_stack(move || {
            let mut t = spine(depth);
            let out = f(&Trail::Nil, &mut t);
            drop_iteratively(t);
            out
        });
        assert!(
            got > depth,
            "each of the {depth} levels is counted at least once"
        );
    }
}

// ---------------------------------------------------------------------------
// A cycle that crosses a body, under both options. The member declared in the body is written
// *inside* the shared driver, beside the store and the parked slot it uses, so this is where the
// placement of an inner member meets the two unsafe paths.
// ---------------------------------------------------------------------------

#[stack_safe(use_nonlinear_mut, data_in_frame)]
fn crosses(t: &mut Tree, trail: &Trail<'_>) -> u64 {
    fn inner(t: &mut Tree, trail: &Trail<'_>) -> u64 {
        t.v += 1;
        if t.kids.is_empty() {
            return walk_trail(trail) + t.v;
        }
        let mut acc = t.v;
        for i in 0..t.kids.len() {
            acc += crosses(&mut t.kids[i], &Trail::Cons(trail_depth(trail) + 1, trail));
        }
        acc
    }

    t.v += 1;
    if t.kids.is_empty() {
        return walk_trail(trail) + t.v;
    }
    let mut acc = t.v;
    for i in 0..t.kids.len() {
        acc += inner(&mut t.kids[i], &Trail::Cons(trail_depth(trail) + 1, trail));
    }
    acc
}

fn crosses_naive(t: &mut Tree, trail: &Trail<'_>) -> u64 {
    fn inner(t: &mut Tree, trail: &Trail<'_>) -> u64 {
        t.v += 1;
        if t.kids.is_empty() {
            return walk_trail(trail) + t.v;
        }
        let mut acc = t.v;
        for i in 0..t.kids.len() {
            acc += crosses_naive(&mut t.kids[i], &Trail::Cons(trail_depth(trail) + 1, trail));
        }
        acc
    }

    t.v += 1;
    if t.kids.is_empty() {
        return walk_trail(trail) + t.v;
    }
    let mut acc = t.v;
    for i in 0..t.kids.len() {
        acc += inner(&mut t.kids[i], &Trail::Cons(trail_depth(trail) + 1, trail));
    }
    acc
}

#[test]
fn both_options_across_a_body_agree_with_naive() {
    let (mut a, mut b) = (bushy(4), bushy(4));
    assert_eq!(
        crosses(&mut a, &Trail::Nil),
        crosses_naive(&mut b, &Trail::Nil),
    );
    drop_iteratively(a);
    drop_iteratively(b);
}

#[test]
fn both_options_across_a_body_is_flat() {
    let depth = 100_000;
    let got = on_tiny_stack(move || {
        let mut t = spine(depth);
        let out = crosses(&mut t, &Trail::Nil);
        drop_iteratively(t);
        out
    });
    assert!(got > depth, "each of the {depth} levels is counted");
}

// ===========================================================================
// Signature shapes
//
// Each of these was refused, and each refusal was mechanical rather than
// necessary: the transform needs a *name* for every payload parameter, because it
// rebuilds the argument list as an expression, and it needs to write some types on
// a `let`. Both are satisfiable without asking the caller to change a signature.
// ===========================================================================

/// A parameter that destructures. The transform gives it a name of its own and
/// re-binds the pattern at the top of the body — which is exactly what the rejection
/// used to tell the caller to do by hand.
#[stack_safe]
fn tuple_param((a, b): (u64, u64)) -> u64 {
    if a == 0 {
        b
    } else {
        tuple_param((a - 1, b + 1))
    }
}

struct Point {
    x: u64,
    y: u64,
}

#[stack_safe]
fn struct_param(Point { x, y }: Point) -> u64 {
    if x == 0 {
        y
    } else {
        struct_param(Point { x: x - 1, y: y + 1 })
    }
}

/// A `_` parameter has no name to bind, and a reference pattern binds through one.
#[stack_safe]
fn ignored_param(n: u64, _: bool) -> u64 {
    if n == 0 {
        0
    } else {
        1 + ignored_param(n - 1, true)
    }
}

#[test]
fn destructuring_parameters_agree_with_plain_recursion() {
    assert_eq!(tuple_param((5, 0)), 5);
    assert_eq!(struct_param(Point { x: 5, y: 0 }), 5);
    assert_eq!(ignored_param(5, false), 5);
}

#[test]
fn deep_destructuring_parameter_is_flat() {
    let depth = 200_000;
    assert_eq!(on_tiny_stack(move || tuple_param((depth, 0))), depth);
}

/// `impl Trait` is not always the whole type. Nested inside one, it was annotated
/// anyway, giving `E0562` on the user's own signature.
#[stack_safe]
fn nested_impl_trait_return(n: u64) -> Box<impl Iterator<Item = u64> + use<>> {
    if n == 0 {
        Box::new(0..1)
    } else {
        nested_impl_trait_return(n - 1)
    }
}

#[stack_safe]
fn nested_impl_trait_param(xs: Vec<impl Copy>, n: u64) -> usize {
    if n == 0 {
        xs.len()
    } else {
        nested_impl_trait_param(xs, n - 1)
    }
}

#[test]
fn a_nested_impl_trait_is_accepted() {
    assert_eq!(nested_impl_trait_return(3).count(), 1);
    assert_eq!(nested_impl_trait_param(vec![1u8, 2, 3], 4), 3);
}

/// A parenthesised `&mut` is the `&mut` parameter it plainly is. It used to travel in
/// the payload instead of becoming a context slot, and the report was an `E0505`
/// naming the expansion's own types with the span on the attribute.
#[stack_safe]
#[allow(unused_parens)]
fn parenthesised_slot(n: u64, out: (&mut Vec<u64>)) {
    if n == 0 {
        return;
    }
    out.push(n);
    parenthesised_slot(n - 1, out);
    out.push(n);
}

#[test]
fn deep_parenthesised_slot_is_flat() {
    let depth = 100_000u64;
    let out = on_tiny_stack(move || {
        let mut out = Vec::new();
        parenthesised_slot(depth, &mut out);
        out
    });
    assert_eq!(out.len(), 2 * depth as usize);
    assert_eq!(out[0], depth);
    assert_eq!(out[out.len() - 1], depth);
}

/// An inert `mut` on a slot binding. Assigning *through* the reference was always
/// fine; the rejection was aimed at reassigning the binding, which this does not do.
#[stack_safe]
fn inert_mut_slot(n: u64, mut out: &mut Vec<u64>) {
    if n == 0 {
        return;
    }
    out.push(n);
    inert_mut_slot(n - 1, out);
    out.push(n);
}

#[test]
fn deep_inert_mut_on_a_slot_is_flat() {
    let depth = 100_000u64;
    let out = on_tiny_stack(move || {
        let mut out = Vec::new();
        inert_mut_slot(depth, &mut out);
        out
    });
    assert_eq!(out.len(), 2 * depth as usize);
}

/// Two members of one cycle spelling the same slot type differently: one names a
/// lifetime, the other elides it. They are the same type, and saying otherwise
/// presented two spellings of it as the caller's mistake.
#[stack_safe]
mod lifetime_spellings {
    pub fn named<'a>(n: u64, out: &'a mut Vec<u64>) {
        if n == 0 {
            return;
        }
        out.push(n);
        elided(n - 1, out);
    }

    pub fn elided(n: u64, out: &mut Vec<u64>) {
        if n == 0 {
            return;
        }
        out.push(n);
        named(n - 1, out);
    }
}

#[test]
fn deep_cycle_spelling_one_slot_two_ways_is_flat() {
    let depth = 100_000u64;
    let out = on_tiny_stack(move || {
        let mut out = Vec::new();
        lifetime_spellings::named(depth, &mut out);
        out
    });
    assert_eq!(out.len(), depth as usize);
}
