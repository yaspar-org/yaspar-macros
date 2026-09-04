// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Reassigning the binding of a `&mut` parameter.
//!
//! A `&mut` parameter is not a value the body holds: it is a context slot the driver owns and
//! every step re-derives. So the binding cannot be walked forward — the next step would re-derive
//! it and never see the new value. Writing *through* it is the ordinary use and is fine, which is
//! why an inert `mut` is now accepted; only an assignment to the binding itself is refused.

use yaspar_macros::stack_safe;

#[stack_safe]
fn reassigns(n: u64, mut out: &mut Vec<u64>, other: &mut Vec<u64>) {
    if n == 0 {
        return;
    }
    out.push(n);
    out = other;
    reassigns(n - 1, out, other);
}

fn main() {}
