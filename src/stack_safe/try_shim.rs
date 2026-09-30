// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Desugaring of `?` so early exits finish the current body instead of returning.
//!
//! Goes through `yaspar_macros_defs::{Try, FromResidual}`, a stable stand-in for the unstable
//! `try_trait_v2`, implemented for `Result`, `Option`, and `ControlFlow`. Other carriers work
//! once they implement both (see `tests/carrier.rs`).

use proc_macro2::TokenStream;
use quote::quote;
use syn::Expr;
use syn::visit_mut::VisitMut;

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

/// Desugar every `?` in `func` through the shim, as the rewrite does, so its uncalled copy
/// accepts the same carriers. Paths are absolute since the copy has no imports.
pub(super) fn desugar(func: &mut syn::ItemFn) {
    struct V;

    impl VisitMut for V {
        fn visit_expr_mut(&mut self, e: &mut Expr) {
            syn::visit_mut::visit_expr_mut(self, e);
            if let Expr::Try(t) = e {
                let inner = &t.expr;
                *e = syn::parse_quote_spanned! {t.question_token.span=>
                    match ::yaspar_macros_defs::Try::branch(#inner) {
                        ::core::result::Result::Ok(__ss_v) => __ss_v,
                        ::core::result::Result::Err(__ss_res) => {
                            return ::yaspar_macros_defs::FromResidual::from_residual(__ss_res)
                        }
                    }
                };
            }
        }
    }

    V.visit_item_fn_mut(func);
}
