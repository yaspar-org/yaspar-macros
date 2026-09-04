// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use yaspar_macros::stack_safe;
#[stack_safe]
mod m {
    pub fn a(n: u64) -> u64 {
        n + 1
    }
    pub fn b(n: u64) -> u64 {
        a(n)
    }
}
fn main() {
    let _ = m::b(3);
}
