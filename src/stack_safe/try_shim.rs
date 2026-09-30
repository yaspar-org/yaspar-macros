// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Desugaring of `?` so early exits finish the current body instead of returning.
//!
//! Goes through `yaspar_macros_defs::{Try, FromResidual}`, a stable stand-in for the unstable
//! `try_trait_v2`, implemented for `Result`, `Option`, and `ControlFlow`. Other carriers work
//! once they implement both (see `tests/carrier.rs`).

use proc_macro2::TokenStream;
use quote::quote;

use super::names::{from_residual_trait, try_trait};

/// `expr?`'s scrutinee: `Ok(v)` for the value, `Err(r)` for the early exit.
pub(super) fn branch(inner: TokenStream) -> TokenStream {
    let tr = try_trait();
    quote! { #tr::branch(#inner) }
}

/// The value to hand back on the early exit, built from the residual.
pub(super) fn from_residual(residual: TokenStream) -> TokenStream {
    let tr = from_residual_trait();
    quote! { #tr::from_residual(#residual) }
}
