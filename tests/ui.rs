// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! What `#[stack_safe]` refuses, and what it says about it.
//!
//! Every rejection here is deliberate, and its wording is part of the feature: it has to say what
//! the macro cannot do and what to write instead. A `compile_fail` doctest cannot check that, since
//! it passes however the message reads, so each case below is pinned against a `.stderr` file.
//!
//! Regenerate them all after changing a message, then read every diff:
//!
//! ```text
//! TRYBUILD=overwrite cargo test --test ui
//! ```

#[test]
fn rejections_say_what_is_wrong() {
    trybuild::TestCases::new().compile_fail("tests/ui/*.rs");
}
