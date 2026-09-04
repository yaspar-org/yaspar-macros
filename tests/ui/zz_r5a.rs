// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use yaspar_macros::stack_safe;
pub struct N {
    pub v: u64,
    pub kids: Vec<N>,
}
#[stack_safe]
mod outer {
    #[stack_safe(use_nonlinear_mut)]
    pub mod inner {
        use super::super::N;
        pub fn bump(t: &mut N) -> u64 {
            let mut acc = t.v;
            for i in 0..t.kids.len() {
                acc += bump(&mut t.kids[i]);
            }
            acc
        }
    }
}
fn main() {}
