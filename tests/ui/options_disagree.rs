// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use yaspar_macros::stack_safe;

struct Tree {
    kids: Vec<Tree>,
}

#[stack_safe]
mod m {
    use super::Tree;

    #[stack_safe(use_nonlinear_mut)]
    pub fn down(t: &mut Tree, n: u64) -> u64 {
        if n == 0 { 0 } else { up(t, n - 1) }
    }

    pub fn up(t: &mut Tree, n: u64) -> u64 {
        if n == 0 { 0 } else { down(t, n - 1) }
    }
}

fn main() {}
