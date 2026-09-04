// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! `?` for `Result`, `Option`, `ControlFlow`, and any carrier the caller adds.
//!
//! `?` has to be desugared by hand, because it returns early and every early exit
//! must become `return Done(..)` instead. The obvious desugaring hardcodes `Ok` /
//! `Err` / `From::from`, which is wrong for an `Option`.
//!
//! What `?` really does is `Try::branch` followed by `FromResidual::from_residual`
//! on the break path, but both traits are unstable (`try_trait_v2`), so a macro that
//! aims at stable cannot name them. `yaspar-macros-defs` carries a two-trait stand-in
//! with one impl per supported carrier, and this module desugars `?` through it. The
//! residual is what distinguishes the carriers: an `Err(e)` carries `e` so `From::from`
//! can widen it, whereas a `None` carries nothing.
//!
//! The desugaring only names those traits by path, so a carrier the stand-in has never heard of
//! works as soon as its author implements the pair — see `tests/carrier.rs`. One that implements
//! only the unstable `core::ops::Try` does not, and the error names
//! `yaspar_macros_defs::Try`.

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
