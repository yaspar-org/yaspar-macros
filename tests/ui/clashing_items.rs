// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use yaspar_macros::stack_safe;

#[stack_safe]
mod m {
    pub fn up(n: u64) -> u64 {
        struct Local(u64);
        if n == 0 { Local(0).0 } else { down(n - 1) }
    }

    pub fn down(n: u64) -> u64 {
        struct Local(u64);
        if n == 0 { Local(1).0 } else { up(n - 1) }
    }
}

fn main() {}
