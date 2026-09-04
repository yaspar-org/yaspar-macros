// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use yaspar_macros::stack_safe;
use yaspar_macros::stack_safe as ss;
struct N {
    v: u64,
    kids: Vec<N>,
}
#[stack_safe]
mod m {
    use super::N;
    use yaspar_macros::stack_safe as ss;
    #[ss(use_nonlinear_mut)]
    pub fn bump(t: &mut N) -> u64 {
        let mut acc = t.v;
        for i in 0..t.kids.len() {
            acc += bump(&mut t.kids[i]);
        }
        acc
    }
}
fn main() {}
