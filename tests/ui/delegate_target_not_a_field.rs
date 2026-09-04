// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! A `target` that names no field of the wrapper. The macro deliberately does *not*
//! check this itself — it cannot see the struct definition — and the compiler's own
//! `E0609` is better than anything it could say, since it lists the fields that do
//! exist. Pinned here so that a change to how the field is spliced cannot quietly
//! turn it into something less useful.

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

#[delegate_trait(target = nosuch)]
impl Store for Wrapper {}

fn main() {}
