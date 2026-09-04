// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use yaspar_macros::stack_safe;
#[stack_safe(data_in_frame, data_in_frame)]
fn f(n: u64) -> u64 {
    if n == 0 { 0 } else { 1 + f(n - 1) }
}
fn main() {
    let _ = f(3);
}
