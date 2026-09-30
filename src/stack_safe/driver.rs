// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! The inlined driver loop and the transitions into it. The outer loop dispatches entries; the
//! inner loop pops frames and resumes them until one makes another call.
//!
//! ```text
//! let mut __ss_frames = __SsFrames::new(); // allocates on the first `__ss_push`
//! let mut __ss_input: Entry = __ss_entry;
//! '__ss_drive: loop {
//!     let mut __ss_value = '__ss_done: { match __ss_input { .. } };
//!     loop {
//!         let Some(__ss_frame) = __ss_frames.pop() else {
//!             break '__ss_drive __ss_value;
//!         };
//!         __ss_value = '__ss_done: { match __ss_frame { .. } };
//!     }
//! }
//! ```

use proc_macro2::TokenStream;
use quote::quote;

use super::names::{
    done_label, done_local, drive_label, frame_local, frames_local, frames_ty, input_local,
    ok_local, push_fn, res_local, value_local,
};
use super::try_shim;

/// Finish the current body: break `'__ss_done` with `v`.
pub(super) fn done(v: TokenStream) -> TokenStream {
    let val = done_local();
    let done = done_label();
    quote! {
        {
            let #val = #v;
            break #done #val
        }
    }
}

/// Resume dispatch with a per-frame `?` check, when the shared one in [`resume`] does not fit.
pub(super) fn resume_direct(arms: &[TokenStream]) -> TokenStream {
    let frame = frame_local();
    quote! { match #frame { #(#arms)* } }
}

/// A recursive call: bind `args`, park `frame`, enter `entry`. Binding first keeps the original
/// evaluation order and coercion sites.
pub(super) fn call(args: TokenStream, entry: TokenStream, frame: TokenStream) -> TokenStream {
    let (frames, input, drive) = (frames_local(), input_local(), drive_label());
    let push = push_fn();
    quote! {
        {
            #args
            #push(&mut #frames, #frame);
            #input = #entry;
            continue #drive
        }
    }
}

/// A tail call: bind `args` and enter `entry` without parking a frame.
pub(super) fn enter(args: TokenStream, entry: TokenStream) -> TokenStream {
    let (input, drive) = (input_local(), drive_label());
    quote! {
        {
            #args
            #input = #entry;
            continue #drive
        }
    }
}

/// Resume dispatch with one shared `?` check. On a residual, drop the frame and finish with
/// `from_residual`.
pub(super) fn resume(inner: &[TokenStream], drops: &[TokenStream]) -> TokenStream {
    let (frame, ok, res) = (frame_local(), ok_local(), res_local());
    let value = value_local();
    let branch = try_shim::branch(quote! { #value });
    let handed_down = done(try_shim::from_residual(quote! { #res }));
    quote! {
        match #branch {
            ::core::result::Result::Ok(#ok) => match #frame { #(#inner)* },
            ::core::result::Result::Err(#res) => {
                match #frame { #(#drops)* }
                #handed_down
            }
        }
    }
}

/// Enter `entry` without parking anything: one iteration of a lowered loop.
pub(super) fn tail(entry: TokenStream) -> TokenStream {
    let (input, drive) = (input_local(), drive_label());
    quote! {
        {
            #input = #entry;
            continue #drive
        }
    }
}

/// Wrap a transition used inside `?`, `return`, or a lowered `break`. It already diverges, so it
/// is only parenthesized.
pub(super) fn escape(next: TokenStream) -> TokenStream {
    quote! { (#next) }
}

/// The driver loop around a group's entry arms and resume dispatch.
pub(super) fn machine(
    entry: &TokenStream,
    input_ann: &TokenStream,
    frames_ann: &TokenStream,
    arms: &[TokenStream],
    resume: &TokenStream,
) -> TokenStream {
    let (frames, frames_ty) = (frames_local(), frames_ty());
    let (input, frame, value) = (input_local(), frame_local(), value_local());
    let (drive, done) = (drive_label(), done_label());
    quote! {
        {
            let mut #frames #frames_ann = #frames_ty::new();
            let mut #input #input_ann = #entry;
            #drive: loop {
                let mut #value = #done: {
                    match #input {
                        #(#arms)*
                    }
                };
                loop {
                    let #frame = match #frames.pop() {
                        ::core::option::Option::None => break #drive #value,
                        ::core::option::Option::Some(#frame) => #frame,
                    };
                    #value = #done: { #resume };
                }
            }
        }
    }
}
