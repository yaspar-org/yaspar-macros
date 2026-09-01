// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use yaspar_macros::stack_safe;

#[stack_safe]
fn nothing_recurses(n: u64) -> u64 {
    if n == 0 { 0 } else { elsewhere(n - 1) }
}

fn elsewhere(n: u64) -> u64 {
    n
}

fn main() {}
