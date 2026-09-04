// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use yaspar_macros::stack_safe;
#[stack_safe]
fn f(n: u64) -> u64 {
    mod inner {
        pub fn go(n: u64) -> u64 {
            if n == 0 { 0 } else { 1 + go(n - 1) }
        }
    }
    inner::go(n)
}
fn main() {
    let _ = f(3);
}
