// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

// The outer `x` read in the *same statement* as the shadowing block, after it. One payload slot
// holds both bindings, so the resumed code would read the inner one here.
use yaspar_macros::stack_safe;

#[stack_safe]
fn f(n: u64) -> u64 {
    if n == 0 {
        return 0;
    }
    let x = 10 + n;
    let y = {
        let x = 100 + n;
        let _ = f(n - 1);
        x
    } + x;
    y
}

fn main() {}
