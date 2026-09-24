// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! The loop, and the three transitions the body reaches it by.
//!
//! The loop is written *here*, into the rewritten function, rather than being a generic function
//! in `yaspar-macros-defs` that the body is handed to as a closure. Both shapes run the same
//! machine; the difference is that a transition is now the push, the pop or the `break` itself,
//! emitted where the body arrives at it, instead of a `Step` value built to be taken apart again
//! by a second `match` one call frame up. On a three-recursive-call-site function that is worth
//! about 2x (see `PERFORMANCE.md`, and `ladder1_one_loop` in
//! `examples/perf_dispatch_width.rs`, which is this shape by hand).
//!
//! What the body is is unchanged: one `match` over [`names::input_ty`], with an arm per entry
//! point and per resume point. What it *answers* with is gone — a transition now stores the next
//! input and jumps to the top of the loop itself, so every arm diverges and the loop is
//!
//! ```text
//! let mut __ss_frames = __SsFrames::new();
//! let mut __ss_input = __SsIn::Enter(__ss_entry);
//! '__ss_drive: loop {
//!     match __ss_input { .. }
//! }
//! ```
//!
//! Storing rather than answering is also what keeps the body type-checking as it did. A payload
//! is a tuple of inferred types, so `Entry::E0((tail,))` needs the *expected* type at hand to
//! coerce `tail` — a `&&Stack` a `match` handed out, where the payload holds a `&Stack` — and a
//! coercion does not reach inside a generic afterwards. An assignment to a local whose type is
//! already known provides that expectation, exactly as the closure's return type used to.

use proc_macro2::TokenStream;
use quote::quote;

use super::names::{done_local, drive_label, frame_local, frames_local, input_local, input_ty};

/// This body is finished: hand the value to the frame below, or answer with it.
///
/// The pop is here rather than in a driver, so the common case — a frame is waiting — reads the
/// tag it is about to switch on straight out of the `Vec` and goes round again. The empty case is
/// the whole recursion's answer, and the only way out of the loop.
pub(super) fn done(v: TokenStream) -> TokenStream {
    let (frames, frame, val) = (frames_local(), frame_local(), done_local());
    let (input, input_local, drive) = (input_ty(), input_local(), drive_label());
    quote! {
        {
            let #val = #v;
            match #frames.pop() {
                ::core::option::Option::None => break #drive #val,
                ::core::option::Option::Some(#frame) => {
                    #input_local = #input::Resume(#frame, #val);
                    continue #drive;
                }
            }
        }
    }
}

/// Park `frame` and enter `entry`: a recursive call.
///
/// The entry is stored *before* the frame is parked, because the two are cut out of one source
/// expression and that is the order it had: the frame takes ownership of the locals live across
/// the call, and an argument may still have to read one of them (`walk(n - 1, ids.clone(), ..)`
/// for an `ids` the continuation also uses).
pub(super) fn call(entry: TokenStream, frame: TokenStream) -> TokenStream {
    let (frames, input, input_local, drive) =
        (frames_local(), input_ty(), input_local(), drive_label());
    quote! {
        {
            #input_local = #input::Enter(#entry);
            #frames.push(#frame);
            continue #drive;
        }
    }
}

/// Enter `entry` without parking anything: one iteration of a lowered loop, whose result belongs
/// to whichever frame is already on top.
pub(super) fn tail(entry: TokenStream) -> TokenStream {
    let (input, input_local, drive) = (input_ty(), input_local(), drive_label());
    quote! {
        {
            #input_local = #input::Enter(#entry);
            continue #drive;
        }
    }
}

/// The loop around one group's body: `arms` entered at `entry`, run to completion.
///
/// `input_ann` names what the macro knows of the frame enum's payload types, for the same reason
/// the closure's parameter annotation named them: a payload type only the arms construct is
/// otherwise still an inference variable where the arms need it. The stack is derived from the
/// state rather than annotated in turn, so that the frame type is said once — see [`In::frames`].
///
/// [`In::frames`]: yaspar_macros_defs::In::frames
pub(super) fn machine(
    entry: &TokenStream,
    input_ann: &TokenStream,
    arms: &[TokenStream],
) -> TokenStream {
    let (frames, input, input_ty) = (frames_local(), input_local(), input_ty());
    let drive = drive_label();
    quote! {
        {
            let mut #input #input_ann = #input_ty::Enter(#entry);
            let mut #frames = #input.frames();
            #drive: loop {
                match #input { #(#arms)* }
            }
        }
    }
}
