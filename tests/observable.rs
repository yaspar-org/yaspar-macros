// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Observable equivalence with the recursive original: side-effect order, drop
//! order and drop count, early exit, and panic unwinding.
//!
//! Every test runs a `#[stack_safe]` function and a hand-written naive twin over
//! the same workload and compares the logs. The naive twin is the specification.
//!
//! Side-effect order matches throughout, and every value is dropped exactly once. The
//! four tests at the end of this file are different in kind: they pin drop *timing*,
//! which does differ because locals live in frames rather than on the native stack.
//! They assert the current behaviour deliberately, so a change to it fails loudly
//! rather than passing unnoticed — see README.md for what each one means.

use std::cell::RefCell;
use std::panic::{AssertUnwindSafe, catch_unwind};
use yaspar_macros::stack_safe;

// ---------------------------------------------------------------------------
// A drop log. `G` records its construction and its destruction, so a reordered
// drop, a leak (missing "drop"), and a double drop (two "drop"s) are all
// visible in the log.
// ---------------------------------------------------------------------------

type Log = RefCell<Vec<String>>;

struct G<'a> {
    name: String,
    log: &'a Log,
}
impl Drop for G<'_> {
    fn drop(&mut self) {
        self.log.borrow_mut().push(format!("drop {}", self.name));
    }
}
fn g<'a>(log: &'a Log, name: String) -> G<'a> {
    log.borrow_mut().push(format!("new {name}"));
    G { name, log }
}
fn note(log: &Log, s: &str) {
    log.borrow_mut().push(s.to_string());
}
/// Spelling the arguments out rather than using `format!("{p}{n}")` keeps these
/// tests measuring drop order alone. Implicit captures are carried correctly — see
/// `implicit_format_captures_are_carried` in `transform.rs`, which pins that.
fn nm(p: &str, n: u64) -> String {
    format!("{}{}", p, n)
}
fn show(v: &[String]) -> String {
    v.join(" | ")
}

/// Run both versions and require identical logs — first as multisets, which
/// catches a leak or a double drop, then in order.
fn same<T: PartialEq + std::fmt::Debug>(
    f: impl FnOnce(u64, &Log) -> T,
    naive: impl FnOnce(u64, &Log) -> T,
    n: u64,
) {
    let (la, lb): (Log, Log) = (RefCell::new(Vec::new()), RefCell::new(Vec::new()));
    let ra = f(n, &la);
    let rb = naive(n, &lb);
    let (a, b) = (la.into_inner(), lb.into_inner());

    let (mut ma, mut mb) = (a.clone(), b.clone());
    ma.sort();
    mb.sort();
    assert_eq!(
        ma,
        mb,
        "drop multiset differs — a leak or a double drop\n stack_safe: {}\n naive     : {}\n",
        show(&a),
        show(&b)
    );
    assert_eq!(
        a,
        b,
        "log order differs\n stack_safe: {}\n naive     : {}\n",
        show(&a),
        show(&b)
    );
    assert_eq!(ra, rb, "return value");
}

// ===========================================================================
// Drop order and count
// ===========================================================================

/// A local live across a recursive call and used after it: parked in the frame,
/// dropped when the resume arm that consumes it finishes.
#[test]
fn local_live_across_a_call_drops_in_order() {
    #[stack_safe]
    fn f(n: u64, log: &Log) -> u64 {
        if n == 0 {
            note(log, "base");
            return 0;
        }
        let a = g(log, nm("A", n));
        let r = f(n - 1, log);
        note(log, &format!("mid {}", a.name));
        r + 1
    }
    fn naive(n: u64, log: &Log) -> u64 {
        if n == 0 {
            note(log, "base");
            return 0;
        }
        let a = g(log, nm("A", n));
        let r = naive(n - 1, log);
        note(log, &format!("mid {}", a.name));
        r + 1
    }
    same(f, naive, 4);
}

/// Two locals in one frame still drop in reverse declaration order.
#[test]
fn two_locals_in_one_frame_drop_in_reverse_order() {
    #[stack_safe]
    fn f(n: u64, log: &Log) -> u64 {
        if n == 0 {
            return 0;
        }
        let a = g(log, nm("A", n));
        let b = g(log, nm("B", n));
        let r = f(n - 1, log);
        note(log, &format!("mid {} {}", a.name, b.name));
        r + 1
    }
    fn naive(n: u64, log: &Log) -> u64 {
        if n == 0 {
            return 0;
        }
        let a = g(log, nm("A", n));
        let b = g(log, nm("B", n));
        let r = naive(n - 1, log);
        note(log, &format!("mid {} {}", a.name, b.name));
        r + 1
    }
    same(f, naive, 3);
}

/// A local declared in a branch: the continuation is duplicated into each arm,
/// so each arm's local must still be dropped exactly once.
#[test]
fn locals_in_branches_drop_once_each() {
    #[stack_safe]
    fn f(n: u64, log: &Log) -> u64 {
        if n == 0 {
            return 0;
        }
        let r = if n.is_multiple_of(2) {
            let a = g(log, nm("E", n));
            let v = f(n - 1, log);
            note(log, &format!("even {}", a.name));
            v
        } else {
            let b = g(log, nm("O", n));
            let v = f(n - 1, log);
            note(log, &format!("odd {}", b.name));
            v
        };
        r + 1
    }
    fn naive(n: u64, log: &Log) -> u64 {
        if n == 0 {
            return 0;
        }
        let r = if n.is_multiple_of(2) {
            let a = g(log, nm("E", n));
            let v = naive(n - 1, log);
            note(log, &format!("even {}", a.name));
            v
        } else {
            let b = g(log, nm("O", n));
            let v = naive(n - 1, log);
            note(log, &format!("odd {}", b.name));
            v
        };
        r + 1
    }
    same(f, naive, 5);
}

/// A local declared inside a lowered loop's body, live across the call in that
/// body: one construction and one drop per iteration, in iteration order.
#[test]
fn locals_in_a_lowered_loop_body_drop_per_iteration() {
    #[stack_safe]
    fn f(n: u64, log: &Log) -> u64 {
        if n == 0 {
            return 0;
        }
        let mut acc = 0;
        for i in 0..2u64 {
            let a = g(log, format!("L{}_{}", n, i));
            acc += f(n - 1, log);
            note(log, &format!("iter {}", a.name));
        }
        note(log, "post-loop");
        acc + 1
    }
    fn naive(n: u64, log: &Log) -> u64 {
        if n == 0 {
            return 0;
        }
        let mut acc = 0;
        for i in 0..2u64 {
            let a = g(log, format!("L{}_{}", n, i));
            acc += naive(n - 1, log);
            note(log, &format!("iter {}", a.name));
        }
        note(log, "post-loop");
        acc + 1
    }
    same(f, naive, 3);
}

/// `?` propagating from the bottom of the recursion, with two live locals at
/// every level and a `From` conversion on the error: every parked frame's
/// locals are dropped, innermost first, exactly as unwinding would.
#[test]
fn question_mark_from_depth_drops_every_frame() {
    #[derive(Debug, PartialEq)]
    struct Low;
    #[derive(Debug, PartialEq)]
    struct High(&'static str);
    impl From<Low> for High {
        fn from(_: Low) -> High {
            High("converted")
        }
    }
    fn bottom(log: &Log) -> Result<u64, Low> {
        note(log, "bottom");
        Err(Low)
    }

    #[stack_safe]
    fn f(n: u64, log: &Log) -> Result<u64, High> {
        let a = g(log, nm("A", n));
        let b = g(log, nm("B", n));
        if n == 0 {
            let v = bottom(log)?;
            note(log, "unreachable");
            return Ok(v);
        }
        let r = f(n - 1, log)?;
        note(log, &format!("mid {} {}", a.name, b.name));
        Ok(r + 1)
    }
    fn naive(n: u64, log: &Log) -> Result<u64, High> {
        let a = g(log, nm("A", n));
        let b = g(log, nm("B", n));
        if n == 0 {
            let v = bottom(log)?;
            note(log, "unreachable");
            return Ok(v);
        }
        let r = naive(n - 1, log)?;
        note(log, &format!("mid {} {}", a.name, b.name));
        Ok(r + 1)
    }
    same(f, naive, 4);
}

/// An early `return` from deep in the recursion: `return` becomes `Done(v)`,
/// which the driver hands to each parked frame in turn, so each frame's locals
/// are still dropped once, innermost first.
#[test]
fn early_return_from_depth_drops_every_frame() {
    #[stack_safe]
    fn f(n: u64, log: &Log) -> u64 {
        if n == 0 {
            note(log, "bottom");
            return 999;
        }
        let a = g(log, nm("A", n));
        let r = f(n - 1, log);
        note(log, &format!("mid {}", a.name));
        if r == 999 {
            return r;
        }
        r + 1
    }
    fn naive(n: u64, log: &Log) -> u64 {
        if n == 0 {
            note(log, "bottom");
            return 999;
        }
        let a = g(log, nm("A", n));
        let r = naive(n - 1, log);
        note(log, &format!("mid {}", a.name));
        if r == 999 {
            return r;
        }
        r + 1
    }
    same(f, naive, 4);
}

// ===========================================================================
// Side-effect order
// ===========================================================================

type Nums = RefCell<Vec<u64>>;

fn p(log: &Nums, v: u64) -> u64 {
    log.borrow_mut().push(v);
    v
}
fn pb(log: &Nums, v: u64, b: bool) -> bool {
    log.borrow_mut().push(v);
    b
}

fn same_nums(f: impl FnOnce(u64, &Nums) -> u64, naive: impl FnOnce(u64, &Nums) -> u64, n: u64) {
    let (la, lb): (Nums, Nums) = (RefCell::new(Vec::new()), RefCell::new(Vec::new()));
    let ra = f(n, &la);
    let rb = naive(n, &lb);
    let (a, b) = (la.into_inner(), lb.into_inner());
    assert_eq!(a, b, "\n stack_safe: {:?}\n naive     : {:?}\n", a, b);
    assert_eq!(ra, rb, "return value");
}

/// Call arguments are evaluated left to right, and a non-recursive argument
/// before a recursive one still happens before it: `cps_seq` has to bind it to a
/// temporary rather than move it into the continuation.
#[test]
fn call_arguments_keep_left_to_right_order() {
    fn take(a: u64, b: u64, c: u64) -> u64 {
        a + b + c
    }
    #[stack_safe]
    fn f(n: u64, log: &Nums) -> u64 {
        if n == 0 {
            return 0;
        }
        take(p(log, n), f(n - 1, log), p(log, 100 + n))
    }
    fn naive(n: u64, log: &Nums) -> u64 {
        if n == 0 {
            return 0;
        }
        take(p(log, n), naive(n - 1, log), p(log, 100 + n))
    }
    same_nums(f, naive, 4);
}

/// The *recursive* call's own arguments, which become an entry payload rather than an
/// argument list. They stay inline where none of them recurses, so the payload tuple
/// evaluates them left to right; and where one of them does, everything before it is bound
/// to a temporary first, so its effects still come first.
#[test]
fn recursive_call_arguments_keep_left_to_right_order() {
    #[stack_safe]
    fn f(n: u64, acc: u64, log: &Nums) -> u64 {
        if n == 0 {
            return acc;
        }
        f(p(log, n) - 1, acc + p(log, 100 + n), log)
    }
    fn naive(n: u64, acc: u64, log: &Nums) -> u64 {
        if n == 0 {
            return acc;
        }
        naive(p(log, n) - 1, acc + p(log, 100 + n), log)
    }
    same_nums(|n, log| f(n, 0, log), |n, log| naive(n, 0, log), 4);
}

/// The same, with an argument that itself recurses: the one before it has to be bound to a
/// temporary, and the one after it belongs to the resume arm.
#[test]
fn arguments_around_a_recursive_argument_keep_their_order() {
    #[stack_safe]
    fn f(n: u64, b: u64, log: &Nums) -> u64 {
        if n == 0 {
            return b;
        }
        f(
            p(log, n) - 1,
            f(n - 1, p(log, 100 + n), log) + p(log, 200 + n),
            log,
        )
    }
    fn naive(n: u64, b: u64, log: &Nums) -> u64 {
        if n == 0 {
            return b;
        }
        naive(
            p(log, n) - 1,
            naive(n - 1, p(log, 100 + n), log) + p(log, 200 + n),
            log,
        )
    }
    same_nums(|n, log| f(n, 0, log), |n, log| naive(n, 0, log), 4);
}

/// A method call's receiver is evaluated before its arguments, even when an
/// argument recurses.
#[test]
fn method_receiver_is_evaluated_before_arguments() {
    #[stack_safe]
    fn f(n: u64, log: &Nums) -> u64 {
        if n == 0 {
            return 1;
        }
        p(log, n)
            .wrapping_add(f(n - 1, log))
            .wrapping_add(p(log, 200 + n))
    }
    fn naive(n: u64, log: &Nums) -> u64 {
        if n == 0 {
            return 1;
        }
        p(log, n)
            .wrapping_add(naive(n - 1, log))
            .wrapping_add(p(log, 200 + n))
    }
    same_nums(f, naive, 4);
}

/// `&&` and `||` stay lazy in both directions: a recursive right operand is not
/// evaluated when the left operand short-circuits, and a non-recursive right
/// operand is not evaluated when a recursive left operand short-circuits.
#[test]
fn short_circuit_operators_stay_lazy() {
    #[stack_safe]
    fn f(n: u64, log: &Nums) -> u64 {
        if n == 0 {
            return 0;
        }
        let x = pb(log, n, n.is_multiple_of(2)) && f(n - 1, log) > 0;
        let y = pb(log, 100 + n, n.is_multiple_of(3)) || f(n - 1, log) > 0;
        let z = f(n - 1, log) > 100 && pb(log, 200 + n, true);
        (x as u64) + (y as u64) + (z as u64) + n
    }
    fn naive(n: u64, log: &Nums) -> u64 {
        if n == 0 {
            return 0;
        }
        let x = pb(log, n, n.is_multiple_of(2)) && naive(n - 1, log) > 0;
        let y = pb(log, 100 + n, n.is_multiple_of(3)) || naive(n - 1, log) > 0;
        let z = naive(n - 1, log) > 100 && pb(log, 200 + n, true);
        (x as u64) + (y as u64) + (z as u64) + n
    }
    same_nums(f, naive, 4);
}

/// Compound assignment: the place is not hoisted, and the right-hand side's
/// effects happen in source order around the recursive call.
#[test]
fn compound_assignment_keeps_effect_order() {
    #[stack_safe]
    fn f(n: u64, log: &Nums) -> u64 {
        if n == 0 {
            return 0;
        }
        let mut acc = p(log, n);
        acc += f(n - 1, log) + p(log, 100 + n);
        acc *= p(log, 200 + n);
        acc
    }
    fn naive(n: u64, log: &Nums) -> u64 {
        if n == 0 {
            return 0;
        }
        let mut acc = p(log, n);
        acc += naive(n - 1, log) + p(log, 100 + n);
        acc *= p(log, 200 + n);
        acc
    }
    same_nums(f, naive, 3);
}

/// A `for` loop's iterator expression is evaluated exactly once per entry to the
/// loop, not once per iteration — the lowered loop re-enters its entry point on
/// every iteration, so the `IntoIterator` call must stay outside it.
#[test]
fn for_loop_iterator_expression_runs_once() {
    #[stack_safe]
    fn f(n: u64, log: &Nums) -> u64 {
        if n == 0 {
            return 0;
        }
        let mut acc = 0;
        for i in {
            p(log, 900 + n);
            0..2u64
        } {
            acc += f(n - 1, log) + p(log, i);
        }
        acc + 1
    }
    fn naive(n: u64, log: &Nums) -> u64 {
        if n == 0 {
            return 0;
        }
        let mut acc = 0;
        for i in {
            p(log, 900 + n);
            0..2u64
        } {
            acc += naive(n - 1, log) + p(log, i);
        }
        acc + 1
    }
    same_nums(f, naive, 3);
}

/// A `while` condition with a side effect is re-evaluated once per iteration and
/// once more to end the loop.
#[test]
fn while_condition_effects_run_once_per_iteration() {
    #[stack_safe]
    fn f(n: u64, log: &Nums) -> u64 {
        if n == 0 {
            return 0;
        }
        let mut i = 0;
        let mut acc = 0;
        while p(log, 500 + i) < 502 {
            acc += f(n - 1, log);
            i += 1;
        }
        acc + 1
    }
    fn naive(n: u64, log: &Nums) -> u64 {
        if n == 0 {
            return 0;
        }
        let mut i = 0;
        let mut acc = 0;
        while p(log, 500 + i) < 502 {
            acc += naive(n - 1, log);
            i += 1;
        }
        acc + 1
    }
    same_nums(f, naive, 3);
}

/// Tuple, array and index positions are strict and left to right.
#[test]
fn tuple_array_and_index_positions_keep_order() {
    #[stack_safe]
    fn f(n: u64, log: &Nums) -> u64 {
        if n == 0 {
            return 0;
        }
        let v = [10u64, 20, 30];
        let t = (p(log, n), f(n - 1, log), p(log, 100 + n));
        let arr = [p(log, 300 + n), f(n - 1, log)];
        v[(t.0 % 3) as usize] + t.1 + t.2 + arr[0] + arr[1]
    }
    fn naive(n: u64, log: &Nums) -> u64 {
        if n == 0 {
            return 0;
        }
        let v = [10u64, 20, 30];
        let t = (p(log, n), naive(n - 1, log), p(log, 100 + n));
        let arr = [p(log, 300 + n), naive(n - 1, log)];
        v[(t.0 % 3) as usize] + t.1 + t.2 + arr[0] + arr[1]
    }
    same_nums(f, naive, 3);
}

// ===========================================================================
// Early exit
// ===========================================================================

/// `return` from inside a loop that lives inside a continuation: the loop was
/// lowered to its own entry point, so the `return` has to abandon the loop state
/// and every frame below it.
#[test]
fn return_from_a_loop_inside_a_continuation() {
    #[stack_safe]
    fn f(n: u64, log: &Log) -> u64 {
        if n == 0 {
            return 0;
        }
        let a = g(log, nm("A", n));
        let r = f(n - 1, log);
        note(log, &format!("resumed {}", a.name));
        let mut acc = r;
        for i in 0..3u64 {
            let b = g(log, format!("L{}_{}", n, i));
            acc += i;
            note(log, &format!("iter {}", b.name));
            if acc > 4 {
                return 777;
            }
        }
        acc + 1
    }
    fn naive(n: u64, log: &Log) -> u64 {
        if n == 0 {
            return 0;
        }
        let a = g(log, nm("A", n));
        let r = naive(n - 1, log);
        note(log, &format!("resumed {}", a.name));
        let mut acc = r;
        for i in 0..3u64 {
            let b = g(log, format!("L{}_{}", n, i));
            acc += i;
            note(log, &format!("iter {}", b.name));
            if acc > 4 {
                return 777;
            }
        }
        acc + 1
    }
    same(f, naive, 4);
}

/// `?` inside a lowered loop, failing part-way through an iteration.
#[test]
fn question_mark_inside_a_lowered_loop() {
    #[derive(Debug, PartialEq)]
    struct Low;
    fn bottom(log: &Log) -> Result<u64, Low> {
        note(log, "bottom");
        Err(Low)
    }

    #[stack_safe]
    fn f(n: u64, log: &Log) -> Result<u64, Low> {
        if n == 0 {
            return Ok(0);
        }
        let a = g(log, nm("A", n));
        let mut acc = 0;
        for i in 0..2u64 {
            let b = g(log, format!("L{}_{}", n, i));
            acc += f(n - 1, log)?;
            note(log, &format!("iter {}", b.name));
            if n == 2 && i == 1 {
                note(log, "failing");
                let _ = bottom(log)?;
            }
        }
        note(log, &format!("done {}", a.name));
        Ok(acc + 1)
    }
    fn naive(n: u64, log: &Log) -> Result<u64, Low> {
        if n == 0 {
            return Ok(0);
        }
        let a = g(log, nm("A", n));
        let mut acc = 0;
        for i in 0..2u64 {
            let b = g(log, format!("L{}_{}", n, i));
            acc += naive(n - 1, log)?;
            note(log, &format!("iter {}", b.name));
            if n == 2 && i == 1 {
                note(log, "failing");
                let _ = bottom(log)?;
            }
        }
        note(log, &format!("done {}", a.name));
        Ok(acc + 1)
    }
    same(f, naive, 3);
}

/// `break value` where the value comes from a recursive call, and the loop is
/// re-entered several times before it breaks.
#[test]
fn break_with_a_recursive_value() {
    #[stack_safe]
    fn f(n: u64, log: &Nums) -> u64 {
        if n == 0 {
            return 0;
        }
        let total = loop {
            let v = f(n - 1, log);
            p(log, 100 + n);
            if v < 100 {
                break v + f(n - 1, log) + 10;
            }
        };
        p(log, 200 + n);
        total
    }
    fn naive(n: u64, log: &Nums) -> u64 {
        if n == 0 {
            return 0;
        }
        let total = loop {
            let v = naive(n - 1, log);
            p(log, 100 + n);
            if v < 100 {
                break v + naive(n - 1, log) + 10;
            }
        };
        p(log, 200 + n);
        total
    }
    same_nums(f, naive, 4);
}

// ===========================================================================
// Panic
// ===========================================================================

/// A panic from deep in the recursion, caught with `catch_unwind`: the frame
/// stack is a local of the driver, so unwinding drops it and every parked frame
/// with it — nothing leaks and nothing is dropped twice.
///
/// The *order* differs from native unwinding: the frames are dropped in `Vec`
/// order, i.e. outermost first, where unwinding drops innermost first. See
/// `panic_drops_parked_frames_outermost_first`.
#[test]
fn panic_from_depth_drops_every_parked_frame_exactly_once() {
    let a = panic_log(boom, 5);
    let b = panic_log(boom_naive, 5);
    let count = |v: &[String], s: &str| v.iter().filter(|x| x.starts_with(s)).count();
    assert_eq!(count(&a, "new"), count(&b, "new"), "constructions");
    assert_eq!(count(&a, "drop"), count(&b, "drop"), "drops");
    assert_eq!(
        count(&a, "new"),
        count(&a, "drop"),
        "every value dropped once"
    );
    let (mut ma, mut mb) = (a.clone(), b.clone());
    ma.sort();
    mb.sort();
    assert_eq!(
        ma,
        mb,
        "drop multiset differs\n stack_safe: {}\n naive     : {}\n",
        show(&a),
        show(&b)
    );
}

/// Documents the one difference: parked frames unwind outermost first.
#[test]
fn panic_drops_parked_frames_outermost_first() {
    assert_eq!(
        panic_log(boom, 3),
        vec![
            "new A3",
            "new A2",
            "new A1",
            "new A0",
            "panicking", //
            "drop A0",   // the frame that panicked, dropped by the unwind itself
            "drop A3",
            "drop A2",
            "drop A1", // the parked frames, in `Vec` order
        ]
    );
    assert_eq!(
        panic_log(boom_naive, 3),
        vec![
            "new A3",
            "new A2",
            "new A1",
            "new A0",
            "panicking", //
            "drop A0",
            "drop A1",
            "drop A2",
            "drop A3",
        ]
    );
}

#[stack_safe]
fn boom(n: u64, log: &Log) -> u64 {
    let a = g(log, nm("A", n));
    if n == 0 {
        note(log, "panicking");
        panic!("deep");
    }
    let r = boom(n - 1, log);
    note(log, &format!("mid {}", a.name));
    r + 1
}

fn boom_naive(n: u64, log: &Log) -> u64 {
    let a = g(log, nm("A", n));
    if n == 0 {
        note(log, "panicking");
        panic!("deep");
    }
    let r = boom_naive(n - 1, log);
    note(log, &format!("mid {}", a.name));
    r + 1
}

fn panic_log(f: impl FnOnce(u64, &Log) -> u64, n: u64) -> Vec<String> {
    let log: Log = RefCell::new(Vec::new());
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let r = catch_unwind(AssertUnwindSafe(|| f(n, &log)));
    std::panic::set_hook(prev);
    assert!(r.is_err(), "expected a panic");
    log.into_inner()
}

// ===========================================================================
// Documented differences. These pin down *current* behaviour where it is not
// observably equal to the recursive original. None of them leaks or double
// drops; all of them are drop *timing*.
// ===========================================================================

/// A local live across a recursive call but never mentioned after it is not
/// parked, so it is dropped *before* the call rather than after it returns.
/// An RAII guard held across the recursion therefore protects nothing.
#[test]
fn unmentioned_local_is_dropped_before_the_call_not_after() {
    #[stack_safe]
    fn f(n: u64, log: &Log) -> u64 {
        if n == 0 {
            return 0;
        }
        let _guard = g(log, nm("A", n));
        let r = f(n - 1, log);
        note(log, "after");
        r + 1
    }
    fn naive(n: u64, log: &Log) -> u64 {
        if n == 0 {
            return 0;
        }
        let _guard = g(log, nm("A", n));
        let r = naive(n - 1, log);
        note(log, "after");
        r + 1
    }

    let (la, lb): (Log, Log) = (RefCell::new(Vec::new()), RefCell::new(Vec::new()));
    assert_eq!(f(2, &la), naive(2, &lb));
    assert_eq!(
        la.into_inner(),
        ["new A2", "drop A2", "new A1", "drop A1", "after", "after"]
    );
    assert_eq!(
        lb.into_inner(),
        ["new A2", "new A1", "after", "drop A1", "after", "drop A2"]
    );
}

/// A temporary in a statement that also contains a recursive call. Plain Rust
/// keeps it until the end of the statement, i.e. across the call; `cps_seq`
/// binds the operand to a `let`, so the temporary dies before the call.
#[test]
fn temporary_before_a_call_dies_before_it() {
    fn takes(a: u64, b: u64) -> u64 {
        a + b
    }
    #[stack_safe]
    fn f(n: u64, log: &Log) -> u64 {
        if n == 0 {
            note(log, "base");
            return 0;
        }
        takes(g(log, nm("T", n)).name.len() as u64, f(n - 1, log))
    }
    fn naive(n: u64, log: &Log) -> u64 {
        if n == 0 {
            note(log, "base");
            return 0;
        }
        takes(g(log, nm("T", n)).name.len() as u64, naive(n - 1, log))
    }

    let (la, lb): (Log, Log) = (RefCell::new(Vec::new()), RefCell::new(Vec::new()));
    assert_eq!(f(2, &la), naive(2, &lb));
    assert_eq!(
        la.into_inner(),
        ["new T2", "drop T2", "new T1", "drop T1", "base"]
    );
    assert_eq!(
        lb.into_inner(),
        ["new T2", "new T1", "base", "drop T1", "drop T2"]
    );
}

/// A local of an inner *block* that is parked in a frame is dropped when the
/// resume arm finishes, which is after the code following the block has run —
/// not at the end of the block.
#[test]
fn parked_block_local_outlives_its_block() {
    #[stack_safe]
    fn f(n: u64, log: &Log) -> u64 {
        if n == 0 {
            return 0;
        }
        let r = {
            let a = g(log, nm("A", n));
            let v = f(n - 1, log);
            note(log, &format!("blk {}", a.name));
            v
        };
        note(log, "after");
        r + 1
    }
    fn naive(n: u64, log: &Log) -> u64 {
        if n == 0 {
            return 0;
        }
        let r = {
            let a = g(log, nm("A", n));
            let v = naive(n - 1, log);
            note(log, &format!("blk {}", a.name));
            v
        };
        note(log, "after");
        r + 1
    }

    let (la, lb): (Log, Log) = (RefCell::new(Vec::new()), RefCell::new(Vec::new()));
    assert_eq!(f(1, &la), naive(1, &lb));
    assert_eq!(la.into_inner(), ["new A1", "blk A1", "after", "drop A1"]);
    assert_eq!(lb.into_inner(), ["new A1", "blk A1", "drop A1", "after"]);
}

/// A lowered `for` loop's iterator lives in the loop's entry payload, so it is
/// dropped when the arm that exhausts the loop finishes — after the code that
/// follows the loop, not at loop exit.
#[test]
fn lowered_loop_iterator_is_dropped_after_the_loop_epilogue() {
    struct DropIter<'a> {
        n: u64,
        log: &'a Log,
    }
    impl Iterator for DropIter<'_> {
        type Item = u64;
        fn next(&mut self) -> Option<u64> {
            if self.n == 0 {
                None
            } else {
                self.n -= 1;
                Some(self.n)
            }
        }
    }
    impl Drop for DropIter<'_> {
        fn drop(&mut self) {
            self.log.borrow_mut().push("drop iter".to_string());
        }
    }

    #[stack_safe]
    fn f(n: u64, log: &Log) -> u64 {
        if n == 0 {
            return 0;
        }
        let mut acc = 0;
        for _i in (DropIter { n: 2, log }) {
            acc += f(n - 1, log);
        }
        note(log, "post-loop");
        acc + 1
    }
    fn naive(n: u64, log: &Log) -> u64 {
        if n == 0 {
            return 0;
        }
        let mut acc = 0;
        for _i in (DropIter { n: 2, log }) {
            acc += naive(n - 1, log);
        }
        note(log, "post-loop");
        acc + 1
    }

    let (la, lb): (Log, Log) = (RefCell::new(Vec::new()), RefCell::new(Vec::new()));
    assert_eq!(f(1, &la), naive(1, &lb));
    assert_eq!(la.into_inner(), ["post-loop", "drop iter"]);
    assert_eq!(lb.into_inner(), ["drop iter", "post-loop"]);
}
