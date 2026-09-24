// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! The loop, and the three transitions the body reaches it by.
//!
//! The loop is written *here*, into the rewritten function, rather than being a generic function
//! in `yaspar-macros-defs` that the body is handed to as a closure. Both shapes run the same
//! machine; the difference is that a transition is now the push, the pop or the `break` itself,
//! emitted where the body arrives at it, instead of a `Step` value built to be taken apart again
//! by a second `match` one call frame up. On a three-recursive-call-site function that is worth
//! about 2x; `ladder1_one_loop` in `examples/perf_dispatch_width.rs` is this shape by hand, and
//! `ladder4_expansion` beside it is what this replaced.
//!
//! What the body is is unchanged: one `match` over [`names::input_ty`], with an arm per entry point
//! and per resume point. What it *answers* with is the next input, so the loop is
//!
//! ```text
//! let mut __ss_frames = __SsFrames::new();
//! let mut __ss_input: __SsIn<_, __SsFrame<..>, _> = __SsIn::Enter(__ss_entry);
//! '__ss_drive: loop {
//!     __ss_input = '__ss_body: { match __ss_input { .. } };
//! }
//! ```
//!
//! *Answering* rather than storing is worth about 1.7x, and it is the largest single thing in the
//! shape. The state is as big as a frame plus a return value — 168 bytes in the benchmark — and a
//! transition that assigns it and jumps back to the top gives it one writer per call site; answered
//! instead, it has exactly one, and LLVM keeps it in registers rather than copying it between stack
//! slots. The assembly says so plainly: the answering form has no block copies in its loop.
//!
//! A transition reached from anywhere but the end of an arm — a `?`, a `return`, a `break` out of a
//! lowered loop — cannot be the arm's value, so it leaves the turn with [`escape`] instead. That is
//! what the `'__ss_body` label is for, and what those places used to `return`.

use proc_macro2::TokenStream;
use quote::quote;

use super::names::{
    body_label, done_local, drive_label, frame_local, frames_local, frames_ty, input_local,
    input_ty, ok_local, res_local, value_local,
};
use super::try_shim;

/// This body is finished: hand the value to the frame below, or answer with it.
///
/// The pop is here rather than in a driver, so the common case — a frame is waiting — reads the
/// tag it is about to switch on straight out of the `Vec` and goes round again. The empty case is
/// the whole recursion's answer, and the only way out of the loop.
pub(super) fn done(v: TokenStream) -> TokenStream {
    let val = done_local();
    let input = input_ty();
    // The pop moved into the resume arm, so this is a plain construction: the state names the
    // answer and nothing else, and whose answer it is is whatever frame is on top. The `let`
    // stays so that the temporaries of `v` die here, where they died when this was a statement.
    quote! {
        {
            let #val = #v;
            #input::Resume(#val)
        }
    }
}

/// The one `Resume` arm: pop the frame here, and dispatch `inner` on it.
///
/// With the frame out of the state the pop has to happen somewhere, and this is the only place
/// that reads it. An empty stack means the value in hand is the whole recursion's answer, so this
/// is also the only way out of the loop — [`done`] no longer has a `break` of its own, and the
/// emptiness is tested once per level here instead of once in every `done`.
fn resume_shell(inner: TokenStream) -> TokenStream {
    let (frames, frame, value) = (frames_local(), frame_local(), value_local());
    let (input_ty, drive) = (input_ty(), drive_label());
    quote! {
        #input_ty::Resume(#value) => match #frames.pop() {
            ::core::option::Option::None => break #drive #value,
            ::core::option::Option::Some(#frame) => #inner,
        },
    }
}

/// One arm per frame, each doing its own carrier check, for a group the shared one does not fit.
///
/// `arms` read the resumed value out of [`names::value_local`], which the shell binds: it used to
/// be a pattern in the state, and the state no longer has a slot for it.
pub(super) fn resume_direct(arms: &[TokenStream]) -> TokenStream {
    let frame = frame_local();
    resume_shell(quote! { match #frame { #(#arms)* } })
}

/// Park `frame` and enter `entry`: a recursive call.
///
/// `args` binds the arguments first, and it has to, for two reasons that both come of the entry
/// being the arm's *value* — which is where it has to be, since answering with the state rather
/// than storing it is what keeps the state in registers.
///
/// The first is order. The frame takes ownership of the locals live across the call, and an
/// argument may still have to read one of them — `walk(n - 1, ids.clone(), ..)` for an `ids` the
/// continuation also uses — so the arguments have to run before the push, which as the value they
/// would not.
///
/// The second is coercion. A payload is a tuple of inferred types, so `E0((tail,))` needs the
/// expected type at hand to coerce `tail`, a `&&Stack` that a `match` handed out, to the `&Stack`
/// the payload holds; a coercion does not reach inside a generic afterwards, and the value position
/// offers no expectation. Each argument's own `let`, annotated with the callee's declared parameter
/// type, is that expectation — the same device the swap path already uses.
pub(super) fn call(args: TokenStream, entry: TokenStream, frame: TokenStream) -> TokenStream {
    let (frames, input) = (frames_local(), input_ty());
    quote! {
        {
            #args
            #frames.push(#frame);
            #input::Enter(#entry)
        }
    }
}

/// One resume arm for every frame, with the `?` every one of them began with done once, above the
/// dispatch on the frame tag.
///
/// This is `ladder1_one_loop`'s shape, and it is the largest single difference between the two in
/// the benchmark: a check per frame arm costs about a third of native, since it puts the carrier's
/// round trip and a copy of the error tail into the hot path once per call site. The caller decides
/// whether it applies: every point has to begin with that check, the members have to share one
/// return type, and no point may have anything to tear down first. See `emit::checks_are_shareable`.
///
/// `inner` is one arm per frame variant, each expecting the checked value in [`names::ok_local`].
/// `drops` is the same dispatch again, for the path where the check fails: the frame is still whole
/// there, and a tuple dropped whole drops its slots front to back, where the recursion this came
/// from dropped its locals in reverse. So that path names them and drops them itself — cold code,
/// and the only thing the lifted check costs.
pub(super) fn resume(inner: &[TokenStream], drops: &[TokenStream]) -> TokenStream {
    let (frame, ok, res) = (frame_local(), ok_local(), res_local());
    let value = value_local();
    let branch = try_shim::branch(quote! { #value });
    let handed_down = done(try_shim::from_residual(quote! { #res }));
    // Both ways out are the arm's *value*: binding the checked value to a `let` first and leaving
    // the turn on the error path would put a second writer back on the state, which is the one
    // thing this shape is careful not to do — and it cost the single-call-site case 2.8x.
    resume_shell(quote! {
        match #branch {
            ::core::result::Result::Ok(#ok) => match #frame { #(#inner)* },
            ::core::result::Result::Err(#res) => {
                match #frame { #(#drops)* }
                #handed_down
            }
        }
    })
}

/// Enter `entry` without parking anything: one iteration of a lowered loop, whose result belongs
/// to whichever frame is already on top.
pub(super) fn tail(entry: TokenStream) -> TokenStream {
    let input = input_ty();
    quote! { #input::Enter(#entry) }
}

/// A transition reached from somewhere other than the end of an arm: a `?`, a `return`, or a
/// `break` out of a lowered loop.
///
/// It leaves the turn with the next state, which is what those places used to `return` the `Step`
/// for. Parenthesised because the state is usually a block, and `break 'a { .. }` reads worse than
/// it parses.
pub(super) fn escape(next: TokenStream) -> TokenStream {
    let body = body_label();
    quote! { break #body (#next) }
}

/// The loop around one group's body: `arms` entered at `entry`, run to completion.
///
/// `input_ann` names what the macro knows of the frame enum's payload types, for the same reason
/// the closure's parameter annotation named them: a payload type only the arms construct is
/// otherwise still an inference variable where the arms need it.
pub(super) fn machine(
    entry: &TokenStream,
    input_ann: &TokenStream,
    frames_ann: &TokenStream,
    arms: &[TokenStream],
) -> TokenStream {
    let (frames, frames_ty) = (frames_local(), frames_ty());
    let (input, input_ty) = (input_local(), input_ty());
    let (drive, body) = (drive_label(), body_label());
    quote! {
        {
            // The frame's payload types are named *here* now, on the one place that holds a
            // frame. They used to be named on the state, and a dead `push`-shaped construction
            // had to tie the two together, because two annotations each had their own `_` holes
            // for the slots only inference can fill and nothing said they were the same holes.
            // With the frame carried nowhere else there is only one annotation, and the resume
            // arm's bindings are inferred straight from it.
            // Room for 64 levels up front. A `Vec` that grows from nothing reallocates and copies
            // at 4, 8, 16, 32 and 64 frames, which a recursion of any depth pays on the way down;
            // one allocation covers all of it. The price is that a call which parks nothing still
            // allocates, where `new()` would not have.
            let mut #frames #frames_ann = #frames_ty::with_capacity(64);
            let mut #input #input_ann = #input_ty::Enter(#entry);
            #drive: loop {
                #input = #body: { match #input { #(#arms)* } };
            }
        }
    }
}
