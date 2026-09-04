// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! A receiver written out as a type — `self: Box<Self>` — is a type the field does not
//! have, so there is nothing to hand the call. Rejected by name, rather than left to
//! the `E0308 expected Box<_>, found Base` the forwarding call would earn, which is
//! reported against the trait's attribute and says nothing about delegation.

use yaspar_macros::{delegatable_trait, delegate_trait};

#[delegatable_trait]
trait Boxed {
    fn consume(self: Box<Self>) -> u32;
}

struct Base;

impl Boxed for Base {
    fn consume(self: Box<Self>) -> u32 {
        1
    }
}

struct Wrapper {
    inner: Base,
}

#[delegate_trait(target = inner)]
impl Boxed for Wrapper {}

fn main() {}
