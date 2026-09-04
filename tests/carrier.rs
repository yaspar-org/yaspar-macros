// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! `?` on a carrier this crate has never heard of.
//!
//! `#[stack_safe]` desugars `?` through the `Try` / `FromResidual` stand-in in
//! `yaspar-macros-defs`, since the real traits are unstable. Those two traits are
//! public and unsealed, so a carrier of one's own joins by implementing them — the
//! orphan rule is satisfied because the carrier is the implementor's own type.
//!
//! That is the crate's one extension point for `?`, and it is the kind of thing that
//! breaks silently: a change to how the desugaring names those traits, or to what it
//! expects of them, would still pass every `Result` and `Option` test in the suite.
//! Hence this file, which drives a hand-written carrier through both halves of the
//! desugaring — the value path (`Try::branch`) and the early exit
//! (`FromResidual::from_residual`) — on the same tiny stack as the rest of the suite.
//!
//! `ControlFlow` is here for the same reason from the other direction: `core` gives `?`
//! three carriers, so the stand-in owes all three, and nobody should have to write the
//! pair by hand for one of them.

use core::ops::ControlFlow;
use yaspar_macros::stack_safe;
use yaspar_macros_defs::{FromResidual, Try};

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
// A carrier of our own: an `Option` by another name, so that nothing in the
// expansion can be picking it up by matching on `Result` or `Option` itself.
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq, Eq)]
enum Maybe<T> {
    Just(T),
    Nothing,
}

/// The residual of a `Maybe`, which carries nothing, exactly as an `Option`'s does.
struct NothingLeft;

impl<T> Try for Maybe<T> {
    type Output = T;
    type Residual = NothingLeft;

    fn branch(self) -> Result<T, NothingLeft> {
        match self {
            Maybe::Just(v) => Ok(v),
            Maybe::Nothing => Err(NothingLeft),
        }
    }
}

impl<T> FromResidual<NothingLeft> for Maybe<T> {
    fn from_residual(_: NothingLeft) -> Self {
        Maybe::Nothing
    }
}

/// Counts down to zero, and `?`s on every level. `Nothing` below `stop`, so the
/// early exit runs at depth `n - stop` and has to travel back out through every
/// parked frame.
#[stack_safe]
fn countdown(n: u64, stop: u64) -> Maybe<u64> {
    if n == 0 {
        return Maybe::Just(0);
    }
    if n < stop {
        return Maybe::Nothing;
    }
    let below = countdown(n - 1, stop)?;
    Maybe::Just(below + 1)
}

fn countdown_naive(n: u64, stop: u64) -> Maybe<u64> {
    if n == 0 {
        return Maybe::Just(0);
    }
    if n < stop {
        return Maybe::Nothing;
    }
    let below = match countdown_naive(n - 1, stop) {
        Maybe::Just(v) => v,
        Maybe::Nothing => return Maybe::Nothing,
    };
    Maybe::Just(below + 1)
}

#[test]
fn question_mark_on_a_hand_written_carrier_agrees_with_plain_recursion() {
    for (n, stop) in [(0, 0), (1, 0), (5, 0), (5, 3), (5, 6), (17, 9)] {
        assert_eq!(
            countdown(n, stop),
            countdown_naive(n, stop),
            "countdown({n}, {stop})"
        );
    }
}

#[test]
fn deep_question_mark_on_a_hand_written_carrier_is_stack_safe() {
    // The value path all the way down and back up: 200 000 `?`s, none of which
    // takes the early exit.
    assert_eq!(
        on_tiny_stack(|| countdown(200_000, 0)),
        Maybe::Just(200_000)
    );
}

// ---------------------------------------------------------------------------
// `ControlFlow`, the third carrier `core` gives `?`. It is a carrier the caller
// does not have to write, so the shim carries it, and the shape is the `Result`
// one rather than the `Option` one: the residual holds the value broken with.
// ---------------------------------------------------------------------------

#[stack_safe]
fn first_over(xs: &[u64], i: usize, limit: u64) -> ControlFlow<u64, u64> {
    if i == xs.len() {
        return ControlFlow::Continue(0);
    }
    if xs[i] > limit {
        return ControlFlow::Break(xs[i]);
    }
    let rest = first_over(xs, i + 1, limit)?;
    ControlFlow::Continue(rest + xs[i])
}

#[test]
fn question_mark_on_control_flow_propagates_the_break_value() {
    assert_eq!(first_over(&[1, 2, 3], 0, 9), ControlFlow::Continue(6));
    assert_eq!(first_over(&[1, 20, 3], 0, 9), ControlFlow::Break(20));
}

#[test]
fn deep_question_mark_on_control_flow_is_stack_safe() {
    let xs: Vec<u64> = core::iter::repeat_n(1, 200_000).collect();
    assert_eq!(
        on_tiny_stack(move || first_over(&xs, 0, 9)),
        ControlFlow::Continue(200_000)
    );

    // And the break path, produced 200 000 frames down.
    let mut ys: Vec<u64> = core::iter::repeat_n(1, 200_000).collect();
    ys.push(42);
    assert_eq!(
        on_tiny_stack(move || first_over(&ys, 0, 9)),
        ControlFlow::Break(42)
    );
}

#[test]
fn deep_early_exit_on_a_hand_written_carrier_is_stack_safe() {
    // The break path: `Nothing` is produced 200 000 frames deep — `n` reaches 4,
    // which is below `stop` and above the `n == 0` base case — and has to be handed
    // back out through every one of the parked frames.
    assert_eq!(on_tiny_stack(|| countdown(200_000, 5)), Maybe::Nothing);
    assert_eq!(countdown_naive(20, 5), Maybe::Nothing);
}
