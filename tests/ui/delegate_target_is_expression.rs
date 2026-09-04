// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! `target` names a field, and the expansion writes the `self.` itself. Writing
//! `self.inner` is the natural mistake, and it has to keep being caught by name even
//! though the target now parses as a dotted field path — `self` is a keyword, so it
//! would otherwise fail as "expected identifier or integer", which reads like the
//! field name were at fault.

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

#[delegate_trait(target = self.inner)]
impl Store for Wrapper {}

fn main() {}
