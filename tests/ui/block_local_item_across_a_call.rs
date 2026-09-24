// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

// An item declared in a block that then recurses: the code after the call becomes another arm,
// where the declaration is not in scope, so `X` would resolve to the outer one.
use yaspar_macros::stack_safe;

const X: u8 = 2;

#[stack_safe]
fn f(n: bool) -> u8 {
    {
        const X: u8 = 1;
        if n { f(false) + X } else { 0 }
    }
}

fn main() {}
