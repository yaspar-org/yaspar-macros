// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use yaspar_macros::stack_safe;
#[stack_safe]
mod outer {
    #[stack_safe]
    pub mod inner {
        pub fn f(n: u64) -> u64 {
            if n == 0 { 0 } else { 1 + f(n - 1) }
        }
    }
}
fn main() {
    let _ = outer::inner::f(3);
}
