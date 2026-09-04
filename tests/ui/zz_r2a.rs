// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use yaspar_macros::stack_safe;
#[stack_safe]
mod b2 {
    pub fn hosts(n: u64) -> u64 {
        if n < u64::MAX {
            fn go(n: u64) -> u64 {
                if n == 0 { 0 } else { 1 + go(n - 1) }
            }
            go(n)
        } else {
            0
        }
    }
}
fn main() {
    let _ = b2::hosts(3);
}
