// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use yaspar_macros::stack_safe;

#[stack_safe(original_suffix = "-orig")]
fn dashed(n: u64) -> u64 {
    if n == 0 { 0 } else { dashed(n - 1) }
}

#[stack_safe(original_suffix = "")]
fn empty(n: u64) -> u64 {
    if n == 0 { 0 } else { empty(n - 1) }
}

#[stack_safe(original_suffix)]
fn bare(n: u64) -> u64 {
    if n == 0 { 0 } else { bare(n - 1) }
}

#[stack_safe(original_suffix = 1)]
fn number(n: u64) -> u64 {
    if n == 0 { 0 } else { number(n - 1) }
}

#[stack_safe(original_suffix = "_a", original_suffix = "_b")]
fn twice(n: u64) -> u64 {
    if n == 0 { 0 } else { twice(n - 1) }
}

fn main() {}
