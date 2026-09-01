// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use yaspar_macros::stack_safe;

#[stack_safe]
fn host(n: u64) -> u64 {
    #[stack_safe(data_in_frame)]
    fn helper(n: u64) -> u64 {
        n + 1
    }
    if n == 0 { 0 } else { helper(0) + host(n - 1) }
}

fn main() {}
