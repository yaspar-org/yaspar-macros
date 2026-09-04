// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! `target = <field>` is the whole of the attribute, and anything after it is a
//! mistake worth naming. Left to the parser it is a bare "unexpected token" under the
//! comma, which does not say what would have been expected there instead.

use yaspar_macros::{delegatable_trait, delegate_trait};

#[delegatable_trait]
trait Store {
    fn get(&self) -> u32;
}

struct Base;

impl Store for Base {
    fn get(&self) -> u32 {
        1
    }
}

struct Wrapper {
    inner: Base,
}

#[delegate_trait(target = inner, extra)]
impl Store for Wrapper {}

fn main() {}
