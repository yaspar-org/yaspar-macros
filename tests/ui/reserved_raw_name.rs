// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

// A raw spelling of a reserved name is the same identifier as the plain one, so it collides with
// the transform's own temporaries. It used to slip past the check and read the wrong binding.
use yaspar_macros::stack_safe;

#[stack_safe]
fn f(n: u64) -> u64 {
    if n == 0 {
        1
    } else {
        let r#__ss_v0 = 100 + n;
        f(n - 1) + r#__ss_v0
    }
}

fn main() {}
