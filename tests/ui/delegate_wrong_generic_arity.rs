// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! An impl that passes an argument count matching no arm of the helper macro. Without
//! the guard arm this is a wall of "no rules expected this token".
//!
//! `Pair`'s second parameter is defaulted, so what it accepts is a *range*: an impl may
//! write one argument or two. The message has to say so — "takes 2" sends the reader
//! looking for an argument they are entitled to leave out.

use yaspar_macros::{delegatable_trait, delegate_trait};

#[delegatable_trait]
trait Pair<A, B = u16> {
    fn left(&self, a: A) -> u64;
    fn right(&self, b: B) -> u64;
}

struct Base;

impl Pair<u8, u16> for Base {
    fn left(&self, a: u8) -> u64 {
        a as u64
    }
    fn right(&self, b: u16) -> u64 {
        b as u64
    }
}

struct Wrapper {
    inner: Base,
}

#[delegate_trait(target = inner)]
impl Pair<u8, u16, u32> for Wrapper {}

fn main() {}
