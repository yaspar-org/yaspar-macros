// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

// A recursive call that names a *different* instantiation. The machine is one loop compiled for
// one instantiation, so re-entering it cannot change the generic arguments.
use yaspar_macros::stack_safe;

#[stack_safe]
fn f<const N: u64>(n: u64) -> u64 {
    if n == 0 { N } else { f::<7>(n - 1) }
}

fn main() {}
