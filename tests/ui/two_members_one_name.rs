// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

// An outer `f` and a nested `f` in one cycle. A call is matched by name, so the two cannot be told
// apart and both would enter the same body.
#[yaspar_macros::stack_safe]
mod m {
    pub fn f(n: u8) -> u8 {
        fn f(_: u8) -> u8 {
            1 + self::f(0)
        }

        if n == 0 { 0 } else { f(0) }
    }
}

fn main() {}
