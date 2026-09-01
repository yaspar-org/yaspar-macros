// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use yaspar_macros::stack_safe;

struct Tree {
    kids: Vec<Tree>,
}

trait Bump {
    fn bump(&mut self) -> u64;
}

// The member asks for an option of its own, which does not make room for a rewritten body either.
#[stack_safe]
impl Bump for Tree {
    #[stack_safe(use_nonlinear_mut)]
    fn bump(&mut self) -> u64 {
        let mut n = 1;
        for i in 0..self.kids.len() {
            n += self.kids[i].bump();
        }
        n
    }
}

fn main() {}
