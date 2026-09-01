// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use yaspar_macros::stack_safe;

enum Chain<'a> {
    Nil,
    Cons(u64, &'a Chain<'a>),
}

#[stack_safe]
fn grow(n: usize, c: &Chain<'_>) -> usize {
    if n == 0 { 0 } else { grow(n - 1, &Chain::Cons(1, c)) }
}

fn main() {}
