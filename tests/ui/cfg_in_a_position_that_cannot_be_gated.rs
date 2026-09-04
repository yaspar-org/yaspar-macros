// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Where a `#[cfg]` still cannot survive the cut, and what is said about it.
//!
//! A statement, a match arm and a struct-expression field are gated properly — the predicate
//! travels to every piece the construct is cut into, which `tests/cfg_gates.rs` pins. A `#[cfg]`
//! deeper inside an expression has no such position, and `#[cfg_attr]` could expand to anything.

use yaspar_macros::stack_safe;

#[stack_safe]
fn operand(n: u64) -> u64 {
    if n == 0 {
        return 0;
    }
    let v = 1 + (#[cfg(any())] operand(n - 1));
    v
}

#[stack_safe]
fn conditional_attribute(n: u64) -> u64 {
    if n == 0 {
        return 0;
    }
    #[cfg_attr(any(), allow(unused))]
    let v = conditional_attribute(n - 1);
    v + 1
}

#[stack_safe]
fn parameter(n: u64, #[cfg(any())] extra: u64) -> u64 {
    if n == 0 {
        return 0;
    }
    parameter(n - 1) + 1
}

fn main() {}
