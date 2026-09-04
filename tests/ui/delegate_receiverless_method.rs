// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! A required method with no `self` receiver cannot be forwarded to a field: there is
//! no `self` to read the field out of. Saying so names the method and the way out;
//! left to the expansion it is `E0424 expected value, found module self` blamed on the
//! trait's own attribute, complete with a `fn version&self()` suggestion.
//!
//! The `E0046` beside it is the cost of reporting rather than silently dropping the
//! method, and it points at the impl block that has to gain one.

use yaspar_macros::{delegatable_trait, delegate_trait};

#[delegatable_trait]
trait Cfg {
    fn version() -> u32;
}

struct Base;

impl Cfg for Base {
    fn version() -> u32 {
        1
    }
}

struct Wrapper {
    inner: Base,
}

#[delegate_trait(target = inner)]
impl Cfg for Wrapper {}

fn main() {}
