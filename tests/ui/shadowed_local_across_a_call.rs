// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

// An inner `x` shadowing an outer one, in a block that recurses. The frame's payload is chosen by
// name, so the two would be one slot and the resumed code would read the inner value where the
// source had gone back to the outer binding.
use yaspar_macros::stack_safe;

#[stack_safe]
fn f(n: u64) -> u64 {
    if n == 0 {
        0
    } else {
        let x = 10 + n;
        {
            let x = 100 + n;
            let _ = f(n - 1);
            let _ = x;
        }
        x
    }
}

fn main() {}
