// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! What a call may lend a callee out of the frame it is leaving.
//!
//! A lend has to outlive a call that becomes a `return`, so `data_in_frame` puts the value in the
//! driver's store. Two shapes are covered here beyond a value built in the argument list: a whole
//! local, whose `let` need not say a type, and a *place inside* a local, where the local is parked
//! and handed back when the frame resumes so that the code after the call still owns it.

use yaspar_macros::stack_safe;

/// Deep enough to cross the store's chunk boundary many times over.
const DEEP: usize = 200_000;

#[derive(Debug, PartialEq)]
struct Def {
    body: u64,
    rest: u64,
}

/// A whole local, bound by a `let` that says no type.
#[stack_safe(data_in_frame)]
fn lend_a_local(n: usize, t: &u64) -> u64 {
    if n == 0 {
        return *t;
    }
    let next = *t + 1;
    lend_a_local(n - 1, &next)
}

/// A place inside a local, where the local is still wanted afterwards.
///
/// The root's `let` says its type: parking something the code only borrows would be wrong, and a
/// reference is what an unannotated `let` might be holding.
#[stack_safe(data_in_frame)]
fn lend_a_place(n: usize, t: &u64) -> u64 {
    if n == 0 {
        return *t;
    }
    let def: Def = Def {
        body: *t + 1,
        rest: 3,
    };
    let deeper = lend_a_place(n - 1, &def.body);
    // the root is ours again: a store that only moved values would have taken it
    deeper + def.rest - 3
}

/// A place reached by indexing, and a root whose `let` says a type.
#[stack_safe(data_in_frame)]
fn lend_an_element(n: usize, t: &u64) -> u64 {
    if n == 0 {
        return *t;
    }
    let row: Vec<u64> = vec![*t + 1, 7];
    let deeper = lend_an_element(n - 1, &row[0]);
    deeper + row[1] - 7
}

/// Two lends in one call, so the parked locals come back in the right order.
#[stack_safe(data_in_frame)]
fn lend_two(n: usize, a: &u64, b: &u64) -> u64 {
    if n == 0 {
        return *a + *b;
    }
    let left: Def = Def {
        body: *a + 1,
        rest: 0,
    };
    let right: Def = Def { body: *b, rest: 0 };
    let deeper = lend_two(n - 1, &left.body, &right.body);
    deeper + left.rest + right.rest
}

/// Three shapes lent from one function: a value built at the call site, a parked struct, and a
/// parked collection. One store holds them all, as variants of one enum, and a lend that lands
/// *above* a parked value in that store must not be what the resume arm takes back.
#[stack_safe(data_in_frame)]
fn lend_three_shapes(n: usize, t: &u64, d: &Def) -> u64 {
    if n == 0 {
        return *t + d.rest;
    }
    let def: Def = Def {
        body: n as u64,
        rest: 1,
    };
    let row: Vec<u64> = vec![n as u64, 5];
    // The first call parks `row` and *then* lends a built `Def`, so `row` is not on top.
    let a = lend_three_shapes(n - 1, &row[0], &Def { body: 0, rest: 0 });
    let b = lend_three_shapes(n - 1, &def.body, d);
    a + b + def.rest + row[1]
}

/// What `lend_three_shapes` says, written as the compiler would run it.
fn lend_three_shapes_naive(n: usize, t: &u64, d: &Def) -> u64 {
    if n == 0 {
        return *t + d.rest;
    }
    let def: Def = Def {
        body: n as u64,
        rest: 1,
    };
    let row: Vec<u64> = vec![n as u64, 5];
    let a = lend_three_shapes_naive(n - 1, &row[0], &Def { body: 0, rest: 0 });
    let b = lend_three_shapes_naive(n - 1, &def.body, d);
    a + b + def.rest + row[1]
}

/// A parked local inside a borrowing loop, so the loop's collection and the park share the store.
///
/// The collection is pushed on the way into the loop and lives until the loop is left, while each
/// iteration parks a local *above* it and takes it back. The take therefore has to leave the
/// collection where it is, or the iterator would be reading a dropped value.
#[stack_safe(data_in_frame)]
fn park_inside_a_borrowing_loop(n: usize, v: Vec<u64>, t: &u64) -> u64 {
    if n == 0 {
        return *t;
    }
    let mut acc = 0;
    for x in &v {
        let row: Vec<u64> = vec![*x, 5];
        acc += park_inside_a_borrowing_loop(n - 1, vec![*x], &row[0]);
        acc += row[1];
    }
    acc
}

/// What `park_inside_a_borrowing_loop` says, written as the compiler would run it.
fn park_inside_a_borrowing_loop_naive(n: usize, v: Vec<u64>, t: &u64) -> u64 {
    if n == 0 {
        return *t;
    }
    let mut acc = 0;
    for x in &v {
        let row: Vec<u64> = vec![*x, 5];
        acc += park_inside_a_borrowing_loop_naive(n - 1, vec![*x], &row[0]);
        acc += row[1];
    }
    acc
}

/// Two members of one group, each parking a local of its own type: the variants of the shared
/// store come from different members, and the descent alternates between them.
#[stack_safe(data_in_frame)]
mod two_members {
    use super::Def;

    pub(super) fn even(n: usize, t: &u64) -> u64 {
        if n == 0 {
            return *t;
        }
        let row: Vec<u64> = vec![n as u64, 5];
        odd(n - 1, &row[0]) + row[1]
    }

    pub(super) fn odd(n: usize, t: &u64) -> u64 {
        if n == 0 {
            return *t;
        }
        let def: Def = Def {
            body: n as u64,
            rest: 3,
        };
        even(n - 1, &def.body) + def.rest
    }
}

/// What `two_members` says, written as the compiler would run it.
mod two_members_naive {
    use super::Def;

    pub(super) fn even(n: usize, t: &u64) -> u64 {
        if n == 0 {
            return *t;
        }
        let row: Vec<u64> = vec![n as u64, 5];
        odd(n - 1, &row[0]) + row[1]
    }

    pub(super) fn odd(n: usize, t: &u64) -> u64 {
        if n == 0 {
            return *t;
        }
        let def: Def = Def {
            body: n as u64,
            rest: 3,
        };
        even(n - 1, &def.body) + def.rest
    }
}

/// A place lend that sits after a `#[cfg]`ed statement which itself recurses.
///
/// Such a statement makes the macro lower everything after it twice — once under the gate, once
/// under its negation — so this site asks for a store twice. Both asks have to land in the same
/// slot: a slot only the dropped lowering pushes into has nothing to give it a type, which is
/// `type annotations needed` pointing at the attribute.
#[stack_safe(data_in_frame)]
fn lend_a_place_after_a_gated_call(n: usize, t: &u64) -> u64 {
    if n == 0 {
        return *t;
    }
    #[cfg(not(any()))]
    if *t == u64::MAX {
        return lend_a_place_after_a_gated_call(n - 1, &0);
    }
    let row: Vec<u64> = vec![n as u64, 5];
    let deeper = lend_a_place_after_a_gated_call(n - 1, &row[0]);
    deeper + row[1]
}

/// Two call sites lending places out of the *same* local, in branches that both survive.
///
/// They share one store, and the descent through them nests, so the store has to behave as a stack.
#[stack_safe(data_in_frame)]
fn lend_the_same_local_twice(n: usize, t: &u64) -> u64 {
    if n == 0 {
        return *t;
    }
    let row: Vec<u64> = vec![n as u64, 5];
    let deeper = if n.is_multiple_of(2) {
        lend_the_same_local_twice(n - 1, &row[0])
    } else {
        lend_the_same_local_twice(n - 1, &row[1])
    };
    deeper + row[0]
}

#[test]
fn a_whole_local_needs_no_annotation() {
    assert_eq!(lend_a_local(DEEP, &0), DEEP as u64);
}

#[test]
fn a_place_inside_a_local_is_parked_and_handed_back() {
    assert_eq!(lend_a_place(3, &0), 3);
    assert_eq!(lend_a_place(DEEP, &0), DEEP as u64);
}

#[test]
fn an_element_of_a_local_collection_too() {
    assert_eq!(lend_an_element(DEEP, &0), DEEP as u64);
}

#[test]
fn two_lends_in_one_call() {
    assert_eq!(lend_two(DEEP, &0, &5), DEEP as u64 + 5);
}

/// The parked value is dropped exactly once when the callee's subtree unwinds through it.
#[test]
fn a_park_inside_a_borrowing_loop() {
    // Branching by the collection's length, so the same applies as above.
    for n in 0..5 {
        let v = vec![1, 2, 3];
        assert_eq!(
            park_inside_a_borrowing_loop(n, v.clone(), &1),
            park_inside_a_borrowing_loop_naive(n, v, &1),
            "n = {n}"
        );
    }
}

#[test]
fn two_members_park_their_own_shapes() {
    for n in 0..10 {
        assert_eq!(
            two_members::even(n, &1),
            two_members_naive::even(n, &1),
            "n = {n}"
        );
    }
}

/// The alternation is still flat, however many variants the shared store has. Named for the skip
/// pattern the Miri jobs use: it is about frames, not aliasing.
#[test]
fn two_members_parking_is_flat() {
    two_members::even(DEEP, &1);
}

/// Every shape a frame holds is dropped exactly once when the descent unwinds through it.
#[test]
fn a_panic_drops_every_shape_once() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    static DROPS: AtomicUsize = AtomicUsize::new(0);

    #[derive(Clone)]
    struct Counted(u64);

    impl Drop for Counted {
        fn drop(&mut self) {
            DROPS.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[stack_safe(data_in_frame)]
    fn boom(n: usize, t: &u64, c: &Counted) -> u64 {
        if n == 0 {
            panic!("from the deepest call");
        }
        // one parked local and one value built here, in that order, so the store holds both shapes
        let held: Counted = Counted(*t + 1);
        let deeper = boom(n - 1, &held.0, &Counted(0));
        deeper + held.0 + c.0
    }

    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let caught = std::panic::catch_unwind(|| boom(64, &0, &Counted(0)));
    std::panic::set_hook(prev);
    assert!(caught.is_err(), "the panic propagates");
    assert_eq!(
        DROPS.load(Ordering::Relaxed),
        // one parked local and one built value per level, plus the one this test owns; the
        // deepest call panics before binding either
        64 + 64 + 1,
        "each value the store held is dropped once"
    );
}

#[test]
fn three_shapes_share_one_store() {
    // Doubling recursion, so a handful of levels is thousands of store operations and every
    // interleaving of the three shapes. Kept small on purpose: this one runs under Miri.
    for n in 0..6 {
        let d = Def { body: 0, rest: 2 };
        assert_eq!(
            lend_three_shapes(n, &1, &d),
            lend_three_shapes_naive(n, &1, &d),
            "n = {n}"
        );
    }
}

#[test]
fn a_place_lend_after_a_gated_call_shares_one_slot() {
    assert_eq!(
        lend_a_place_after_a_gated_call(DEEP, &1),
        1 + 5 * DEEP as u64
    );
}

#[test]
fn the_same_local_lent_from_two_sites() {
    // `f(n) = f(n - 1) + n`, and the deepest frame answers with `row[1]`, which is 5.
    let expect: u64 = (1..=DEEP as u64).sum::<u64>() + 5;
    assert_eq!(lend_the_same_local_twice(DEEP, &1), expect);
}

#[test]
fn a_panic_leaves_the_store_to_drop_what_it_holds() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    static DROPS: AtomicUsize = AtomicUsize::new(0);

    struct Counted(u64);

    impl Drop for Counted {
        fn drop(&mut self) {
            DROPS.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[stack_safe(data_in_frame)]
    fn boom(n: usize, t: &u64) -> u64 {
        if n == 0 {
            panic!("from the deepest call");
        }
        let held: Counted = Counted(*t + 1);
        let deeper = boom(n - 1, &held.0);
        deeper + held.0
    }

    // the hook is silenced, since the panic below is the point of the test rather than a failure
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let caught = std::panic::catch_unwind(|| boom(64, &0));
    std::panic::set_hook(prev);
    assert!(caught.is_err(), "the panic propagates");
    assert_eq!(
        DROPS.load(Ordering::Relaxed),
        64,
        "each parked value dropped once"
    );
}

/// A field of a *parameter*, which needs no store: the referent is rooted outside the driver, so a
/// plain borrow travels in the payload exactly as it did before parking existed.
#[stack_safe]
fn field_of_a_parameter(n: usize, node: &Node) -> u64 {
    if n == 0 || node.kids.is_empty() {
        return node.value;
    }
    node.value + field_of_a_parameter(n - 1, &node.kids[0])
}

/// The same through a `&mut` parameter, i.e. what `use_nonlinear_mut` is for.
#[stack_safe(use_nonlinear_mut)]
fn field_of_a_mut_parameter(node: &mut Node) -> u64 {
    node.value += 1;
    let mut sum = node.value;
    let mut i = 0usize;
    while i < node.kids.len() {
        sum += field_of_a_mut_parameter(&mut node.kids[i]);
        i += 1;
    }
    sum
}

struct Node {
    value: u64,
    kids: Vec<Node>,
}

/// A chain of `depth` nodes, each holding the next.
fn chain(depth: usize) -> Node {
    let mut node = Node {
        value: 1,
        kids: Vec::new(),
    };
    for _ in 0..depth {
        node = Node {
            value: 1,
            kids: vec![node],
        };
    }
    node
}

#[test]
fn a_field_of_a_parameter_stays_a_plain_borrow() {
    let deep = chain(DEEP);
    assert_eq!(field_of_a_parameter(DEEP + 1, &deep), DEEP as u64 + 1);
    std::mem::forget(deep); // dropping a chain this deep recurses natively
}

#[test]
fn a_field_of_a_mut_parameter_too() {
    let mut deep = chain(DEEP);
    assert_eq!(field_of_a_mut_parameter(&mut deep), (DEEP as u64 + 1) * 2);
    std::mem::forget(deep);
}
