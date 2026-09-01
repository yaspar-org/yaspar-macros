// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use yaspar_macros::stack_safe;

struct Tree {
    kids: Vec<Tree>,
}

#[stack_safe]
fn bump(t: &mut Tree) -> u64 {
    let mut n = 1;
    for i in 0..t.kids.len() {
        n += bump(&mut t.kids[i]);
    }
    n
}

fn main() {}
