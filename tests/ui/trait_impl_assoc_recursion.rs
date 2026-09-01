// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use yaspar_macros::stack_safe;

struct T;

trait Depth {
    fn depth(n: u64) -> u64;
}

// A member with no receiver needs no splitting, so the transform could rewrite it in place. It is
// rejected all the same: recursion in a trait impl is not supported, whatever the member's shape.
#[stack_safe]
impl Depth for T {
    fn depth(n: u64) -> u64 {
        if n == 0 { 0 } else { 1 + Self::depth(n - 1) }
    }
}

fn main() {}
