// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! A recursive call written through a path the macro cannot resolve.
//!
//! A macro resolves no paths, so it recognises a call to something in its own scope only by the
//! shape of the call. `T0::depth`, `<Self>::depth` and `crate::cm::depth` all name a member here,
//! and none of them is a shape the rewriter can turn into a step of the driver. That used to mean
//! no edge in the call graph, so no cycle, so the function was emitted exactly as written: it
//! compiled, it returned the right answer, and it overflowed the stack on a deep input — the one
//! failure the attribute exists to remove. It is now said out loud, with the spelling to use.

use yaspar_macros::stack_safe;

pub struct T0;

#[stack_safe]
impl T0 {
    pub fn depth(n: u64) -> u64 {
        if n == 0 { 0 } else { 1 + T0::depth(n - 1) }
    }
}

pub struct T1;

#[stack_safe]
impl T1 {
    pub fn depth(n: u64) -> u64 {
        if n == 0 {
            0
        } else {
            1 + <Self>::depth(n - 1)
        }
    }
}

#[stack_safe]
pub mod cm {
    pub fn depth(n: u64) -> u64 {
        if n == 0 {
            0
        } else {
            1 + crate::cm::depth(n - 1)
        }
    }
}

fn main() {}
