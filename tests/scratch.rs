// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Scratch repros. Deleted before the commit.

use yaspar_macros::stack_safe;

const TINY_STACK: usize = 64 * 1024;

fn on_tiny_stack<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    std::thread::Builder::new()
        .stack_size(TINY_STACK)
        .spawn(f)
        .expect("spawn")
        .join()
        .expect("join")
}

struct T0;

#[stack_safe]
impl T0 {
    pub fn depth(n: u64) -> u64 {
        if n == 0 { 0 } else { 1 + T0::depth(n - 1) }
    }
}

struct T1;

#[stack_safe]
impl T1 {
    pub fn depth(n: u64) -> u64 {
        if n == 0 { 0 } else { 1 + <Self>::depth(n - 1) }
    }
}

#[stack_safe]
pub mod cm {
    pub fn depth(n: u64) -> u64 {
        if n == 0 { 0 } else { 1 + crate::cm::depth(n - 1) }
    }
}

#[test]
fn item1_impl_own_type() {
    assert_eq!(on_tiny_stack(|| T0::depth(200_000)), 200_000);
}

#[test]
fn item1_qself() {
    assert_eq!(on_tiny_stack(|| T1::depth(200_000)), 200_000);
}

#[test]
fn item1_crate_path() {
    assert_eq!(on_tiny_stack(|| cm::depth(200_000)), 200_000);
}
