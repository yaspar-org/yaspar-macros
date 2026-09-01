// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use yaspar_macros::stack_safe;

#[stack_safe]
mod m {
    pub fn up(out: &mut Vec<u64>, n: u64) -> u64 {
        if n == 0 { 0 } else { down(n - 1) }
    }

    pub fn down(n: u64) -> u64 {
        let mut v = Vec::new();
        if n == 0 { 0 } else { up(&mut v, n - 1) }
    }
}

fn main() {}
