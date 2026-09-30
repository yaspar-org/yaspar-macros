// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Names used by expansions. Everything generated is `__ss` / `__Ss`-prefixed; items from
//! `yaspar-macros-defs` are imported under such names by [`defs_imports`].

use proc_macro2::{Ident, TokenStream};
use quote::{format_ident, quote};

/// The `use` of `yaspar-macros-defs` items for a rewritten body: `Frames` always, the rest only
/// if `body` mentions them.
pub(super) fn defs_imports(body: &TokenStream) -> TokenStream {
    let frames = frames_ty();
    let optional = [
        (pin_ty(), quote! { Pin }),
        (try_trait(), quote! { Try }),
        (from_residual_trait(), quote! { FromResidual }),
        (range_peek_fn(), quote! { range_peek }),
        (range_at_fn(), quote! { range_at }),
        (push_fn(), quote! { push }),
    ];
    let used = optional
        .iter()
        .filter(|(alias, _)| super::analyze::tokens_mention(body, alias))
        .map(|(alias, name)| quote! { #name as #alias, });
    quote! {
        use ::yaspar_macros_defs::{
            Frames as #frames,
            #(#used)*
        };
    }
}

pub(super) fn entry_ty() -> Ident {
    format_ident!("__SsEntry")
}
pub(super) fn entry_variant(n: usize) -> Ident {
    format_ident!("E{}", n)
}
/// Placeholder for a lowered loop's state tuple, substituted once liveness is solved.
pub(super) fn state_marker(n: usize) -> Ident {
    format_ident!("__ss_st{}", n)
}
/// Enum of all values held in the pinned store: one generic variant per shape.
pub(super) fn pinned_ty() -> Ident {
    format_ident!("__SsPinned")
}
pub(super) fn pinned_variant(n: usize) -> Ident {
    format_ident!("P{}", n)
}
pub(super) fn pinned_param(n: usize) -> Ident {
    format_ident!("__SsP{}", n)
}

/// The frame enum: one variant per resume point, carrying the locals live across that call.
pub(super) fn frame_ty() -> Ident {
    format_ident!("__SsFrame")
}
/// The frame stack's type (`Frames`), so expansions need not name `Vec`.
pub(super) fn frames_ty() -> Ident {
    format_ident!("__SsFrames")
}
/// Parks a frame (`push`); the first push reserves `FIRST_FRAMES` at once.
pub(super) fn push_fn() -> Ident {
    format_ident!("__ss_push")
}
/// The frame stack local.
pub(super) fn frames_local() -> Ident {
    format_ident!("__ss_frames")
}
/// The next entry point to execute.
pub(super) fn input_local() -> Ident {
    format_ident!("__ss_input")
}
/// The frame just popped, to be resumed with the value.
pub(super) fn frame_local() -> Ident {
    format_ident!("__ss_frame")
}
/// The callee's answer being unwound; with a shared `?` check, the carrier before the check.
pub(super) fn value_local() -> Ident {
    format_ident!("__ss_value")
}
/// The value after the shared `?` check passed.
pub(super) fn ok_local() -> Ident {
    format_ident!("__ss_ok")
}
/// The residual from a failed shared `?` check.
pub(super) fn res_local() -> Ident {
    format_ident!("__ss_res")
}
/// The finished value bound by `driver::done` before breaking out.
pub(super) fn done_local() -> Ident {
    format_ident!("__ss_done")
}
/// The outer loop: a call sets [`input_local`] and continues it; the final value breaks it.
pub(super) fn drive_label() -> syn::Lifetime {
    syn::Lifetime::new("'__ss_drive", proc_macro2::Span::call_site())
}
/// The block around entry or continuation code, broken with the finished value.
pub(super) fn done_label() -> syn::Lifetime {
    syn::Lifetime::new("'__ss_done", proc_macro2::Span::call_site())
}
pub(super) fn frame_variant(r: usize) -> Ident {
    format_ident!("R{}", r)
}
/// Placeholder for a resume point's payload tuple, solved like a loop's state.
pub(super) fn frame_marker(r: usize) -> Ident {
    format_ident!("__ss_fr{}", r)
}
/// The context tuple the driver lends to the body and continuations.
pub(super) fn ctx_param() -> Ident {
    format_ident!("__ss_ctx")
}
/// Replacement for a method's `self`, which cannot be rebound.
pub(super) fn self_binding() -> Ident {
    format_ident!("__ss_self")
}
/// Saved context pointer while a child subtree runs (`use_nonlinear_mut`).
pub(super) fn saved_slot(n: usize) -> Ident {
    format_ident!("__ss_sv{}", n)
}

/// The pinned store (`Pin`) for `data_in_frame` values, kept at a fixed address until the
/// building frame is popped.
pub(super) fn pin_ty() -> Ident {
    format_ident!("__SsPin")
}

/// Stand-in for `Try` (see `try_shim`).
pub(super) fn try_trait() -> Ident {
    format_ident!("__SsTry")
}

/// Stand-in for `FromResidual` (see `try_shim`).
pub(super) fn from_residual_trait() -> Ident {
    format_ident!("__SsFromResidual")
}

/// `range_peek`: the next value of a lowered `for i in a..b`, without stepping. See
/// `cps::lower_loop`.
pub(super) fn range_peek_fn() -> Ident {
    format_ident!("__ss_range_peek")
}

/// `range_at`: the current `i` of such a loop, recomputed from the carried iterator.
pub(super) fn range_at_fn() -> Ident {
    format_ident!("__ss_range_at")
}

/// A lifted group's name: its members joined with `_`, unique within the container.
fn group_name(members: &[Ident]) -> String {
    members
        .iter()
        .map(Ident::to_string)
        .collect::<Vec<_>>()
        .join("_")
}

/// A lifted group's seed enum: one variant per member, holding its parameters.
pub(super) fn seed_ty(members: &[Ident]) -> Ident {
    format_ident!("__SsSeed_{}", group_name(members))
}

/// The shared function a lifted group's members call.
pub(super) fn machine_fn(members: &[Ident]) -> Ident {
    format_ident!("__ss_machine_{}", group_name(members))
}

/// The unmodified copy of a function kept so the compiler still checks the original.
pub(super) fn original(name: &Ident, suffix: &str) -> Ident {
    format_ident!("{}{}", name, suffix)
}

/// The lifetime the seed enum gives every reference parameter.
pub(super) fn seed_lifetime() -> syn::Lifetime {
    syn::Lifetime::new("'__ss", proc_macro2::Span::call_site())
}

/// Union of a group's return types, when members differ.
pub(super) fn ret_union_ty() -> Ident {
    format_ident!("__SsRet")
}
