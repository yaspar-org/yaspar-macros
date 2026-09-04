// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use yaspar_macros::stack_safe;
#[stack_safe]
mod m {
    pub fn f(n: u64) -> u64 {
        if n == 0 { 0 } else { 1 + f(n - 1) }
    }
    #[cfg(feature = "nope")]
    pub fn gone(n: u64) -> u64 {
        n
    }
}
fn main() {
    let _ = m::f(3);
}
