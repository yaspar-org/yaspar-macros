// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

// A binding that shadows the function being rewritten. A call is recognised by name, so the call to
// the binding would be rewritten into a recursion.
use yaspar_macros::stack_safe;

#[stack_safe]
fn f(n: u64) -> u64 {
    if n == 0 {
        return 1;
    }
    let below = f(n - 1);
    let f: fn(u64) -> u64 = |_| 100;
    below + f(0)
}

fn main() {}
