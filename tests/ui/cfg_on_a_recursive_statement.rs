// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! A `#[cfg]` cannot survive the cut, so it is refused rather than dropped.
//!
//! The transform splits a body at each recursive call, so a statement that recurses
//! does not stay in one piece and there is nothing left for the `#[cfg]` to gate. The
//! attribute used to be silently discarded, which meant the *disabled* code compiled
//! and ran. Each of the four positions below is now its own message.

use yaspar_macros::stack_safe;

#[stack_safe]
fn statement(n: u64) -> u64 {
    if n == 0 {
        return 0;
    }
    #[cfg(any())]
    let _ = statement(n - 1);
    1
}

#[stack_safe]
fn operand(n: u64) -> u64 {
    if n == 0 {
        return 0;
    }
    let v = 1 + (#[cfg(any())] operand(n - 1));
    v
}

struct S {
    a: u64,
    b: u64,
}

#[stack_safe]
fn field(n: u64) -> u64 {
    if n == 0 {
        return 0;
    }
    let s = S {
        a: 1,
        #[cfg(any())]
        b: field(n - 1),
    };
    s.a + s.b
}

fn main() {}
