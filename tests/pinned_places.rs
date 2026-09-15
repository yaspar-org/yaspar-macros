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
