// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! The names an expansion refers to.
//!
//! Two kinds. The identifiers it *generates* are all fixed and `__ss`-prefixed, so they
//! cannot collide with anything the user wrote. The items it *borrows* live in the
//! `yaspar-macros-defs` crate, since they are the same for every function, and are
//! imported once at the top of the rewritten body by [`defs_imports`] — under the same
//! `__ss` names, so the two kinds read alike and neither can shadow anything of the
//! user's.

use proc_macro2::{Ident, TokenStream};
use quote::{format_ident, quote};

/// Bring the fixed half of an expansion into the rewritten body.
///
/// One `use` rather than a fully qualified path at every mention: the expansion is what
/// a reader debugging their own function has to read. The aliases are the `__ss` names,
/// so nothing here can shadow an item the body already uses, and an expansion that
/// happens not to need `Try` or `Pin` is no reason to work out which ones to leave out.
pub(super) fn defs_imports() -> TokenStream {
    let (step, input, drive, pin) = (step_ty(), input_ty(), drive_fn(), pin_ty());
    let (tr, from_residual) = (try_trait(), from_residual_trait());
    quote! {
        #[allow(unused_imports)]
        use ::yaspar_macros_defs::{
            FromResidual as #from_residual, In as #input, Pin as #pin, Step as #step,
            Try as #tr, drive as #drive,
        };
    }
}

pub(super) fn step_ty() -> Ident {
    format_ident!("__SsStep")
}
pub(super) fn entry_ty() -> Ident {
    format_ident!("__SsEntry")
}
pub(super) fn drive_fn() -> Ident {
    format_ident!("__ss_drive")
}
pub(super) fn entry_variant(n: usize) -> Ident {
    format_ident!("E{}", n)
}
/// Placeholder standing in for a lowered loop's state tuple. Substituted once
/// the set of threaded locals is known.
pub(super) fn state_marker(n: usize) -> Ident {
    format_ident!("__ss_st{}", n)
}
/// The frame enum: one variant per *resume point*, carrying the locals live across
/// that call. This is what replaces a boxed continuation.
pub(super) fn frame_ty() -> Ident {
    format_ident!("__SsFrame")
}
/// What the driver hands the body: either an entry, or a frame plus the result the
/// child produced.
pub(super) fn input_ty() -> Ident {
    format_ident!("__SsIn")
}
pub(super) fn frame_variant(r: usize) -> Ident {
    format_ident!("R{}", r)
}
/// Placeholder for a resume point's payload tuple, solved like a loop's state.
pub(super) fn frame_marker(r: usize) -> Ident {
    format_ident!("__ss_fr{}", r)
}
/// The context tuple, lent to the body and to every continuation by the driver.
pub(super) fn ctx_param() -> Ident {
    format_ident!("__ss_ctx")
}
/// The stand-in for a method's `self`, since `self` cannot be rebound.
pub(super) fn self_binding() -> Ident {
    format_ident!("__ss_self")
}
/// Where a swapped context pointer is parked while the child subtree runs.
pub(super) fn saved_slot(n: usize) -> Ident {
    format_ident!("__ss_sv{}", n)
}

/// The driver's pinned store: values a call site built and lent to its callee, kept at
/// a fixed address until the frame that built them is popped.
pub(super) fn pin_ty() -> Ident {
    format_ident!("__SsPin")
}

/// The stand-in for `Try`, so that `?` works on a `Result` and on an `Option` alike.
pub(super) fn try_trait() -> Ident {
    format_ident!("__SsTry")
}

/// The stand-in for `FromResidual`, which builds the early-exit value.
pub(super) fn from_residual_trait() -> Ident {
    format_ident!("__SsFromResidual")
}

/// A lifted group's name: every member it covers, joined.
///
/// One container may hold several groups, and these items are siblings of the members
/// rather than nested inside them, so the name has to distinguish one group from another.
/// Naming every member rather than just the first also says, in the expansion itself,
/// which functions share the machine.
fn group_name(members: &[Ident]) -> String {
    members
        .iter()
        .map(Ident::to_string)
        .collect::<Vec<_>>()
        .join("_")
}

/// The seed enum of a lifted group: one variant per member, carrying that member's own
/// parameters. Its types come from the signatures, so it can be named in a signature.
pub(super) fn seed_ty(members: &[Ident]) -> Ident {
    format_ident!("__SsSeed_{}", group_name(members))
}

/// The one function a lifted group's members all call.
pub(super) fn machine_fn(members: &[Ident]) -> Ident {
    format_ident!("__ss_machine_{}", group_name(members))
}

/// The copy of a function kept beside it for the borrow checker to read, under an unsafe option.
///
/// Generated, hence `__ss`-prefixed like everything else this module mints: nothing the user wrote
/// can collide with it, and a function of their own called `f_orig` stays theirs.
pub(super) fn original(name: &Ident) -> Ident {
    format_ident!("__ss_orig_{}", name)
}

/// The lifetime the seed enum gives every reference among a member's parameters.
pub(super) fn seed_lifetime() -> syn::Lifetime {
    syn::Lifetime::new("'__ss", proc_macro2::Span::call_site())
}

/// The union of a group's return types, when its members answer with different ones.
pub(super) fn ret_union_ty() -> Ident {
    format_ident!("__SsRet")
}
