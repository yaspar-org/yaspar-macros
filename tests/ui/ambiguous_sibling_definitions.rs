// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

// Two `fn step`s in sibling blocks. A definition is addressed by the body that declares it, not by
// the block, so both are equally in scope at each call — Rust tells them apart and this cannot.
use yaspar_macros::stack_safe;

#[stack_safe]
fn f(n: u64, left: bool) -> u64 {
    if left {
        fn step(n: u64) -> u64 {
            if n == 0 { 100 } else { f(n - 1, false) + 1 }
        }
        step(n)
    } else {
        fn step(n: u64) -> u64 {
            if n == 0 { 200 } else { f(n - 1, true) + 10 }
        }
        step(n)
    }
}

fn main() {}
