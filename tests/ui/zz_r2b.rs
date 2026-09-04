// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use yaspar_macros::stack_safe;
#[stack_safe]
fn f(n: u64) -> u64 {
    if n < u64::MAX {
        fn go(n: u64) -> u64 {
            if n == 0 { 0 } else { 1 + f(n - 1) }
        }
        if n == 0 { 0 } else { go(n - 1) }
    } else {
        0
    }
}
fn main() {
    let _ = f(3);
}
