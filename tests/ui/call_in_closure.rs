// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use yaspar_macros::stack_safe;

#[stack_safe]
fn f(n: u64) -> u64 {
    (0..n).map(|k| f(k)).sum()
}

fn main() {}
