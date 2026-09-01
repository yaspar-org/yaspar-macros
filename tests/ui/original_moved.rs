// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use yaspar_macros::stack_safe;

enum Chain<'a> {
    Nil,
    Cons(u64, &'a Chain<'a>),
}

// Not a borrow but a move: the value is handed to the recursive call and then used again. Both the
// rewritten body and the copy earn the error, so it is reported against each.
#[stack_safe(data_in_frame)]
fn moved(n: usize, c: &Chain<'_>, v: Vec<u64>) -> usize {
    if n == 0 {
        v.len()
    } else {
        moved(n - 1, &Chain::Cons(1, c), v) + v.len()
    }
}

fn main() {}
