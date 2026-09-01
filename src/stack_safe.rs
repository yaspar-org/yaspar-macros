// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Implementation of `#[stack_safe]` — rewrite a recursive function, or a whole cycle of
//! mutually recursive ones, into an iterative state machine that keeps its frames on the
//! heap, so recursion depth is bounded by available memory rather than by the native stack.
//!
//! # The idea
//!
//! CPS conversion. Every recursive call is split into
//!
//! 1. a *request* to evaluate the body on new arguments, and
//! 2. a *continuation*: the rest of the body, as a closure taking the result.
//!
//! A driver loop keeps a `Vec` of parked frames and alternates between entering the
//! body and resuming a frame. No native frame is pushed per level of recursion.
//!
//! # What is generated and what is not
//!
//! Only the parts that vary are generated: the entry enum, with a variant per entry
//! point, and the frame enum, with a variant per call site. The rest is the same for
//! every function and lives in `yaspar-macros-defs`, which the rewritten body imports
//! once at its top: `Step` and `In` for the protocol, `drive` for the loop, `Pin` for the
//! store behind `data_in_frame`, and `Try` / `FromResidual` for `?`. They are imported
//! under `__ss` names, so an expansion reads the same as it did when they were emitted
//! into it.
//!
//! # The frame enum
//!
//! The continuation is *defunctionalized*: each call site gets a variant of a frame
//! enum carrying the locals live across that call, and the code after the call
//! becomes an arm of the same `match`. The driver's stack is a plain `Vec` of those
//! frames — no allocation per call, no dynamic dispatch.
//!
//! A proc macro cannot write the payload types down, but it does not have to: the
//! enum is generic over them and inference fills them in from the construction
//! sites, exactly as the entry enum has always done. What it *does* have to compute
//! is liveness, which a boxed closure got for free from capture inference — see
//! [`loop_state::solve_payloads`], which is the price of this encoding and where its
//! sharp edges are.
//!
//! # Loops
//!
//! A loop body that recurses cannot be expressed as a single `FnOnce`
//! continuation — resuming a loop needs a continuation that can run more than
//! once. Making the frame `FnMut` and mutating a stored iterator in place is the
//! obvious fix, but it does not work: an `FnMut` closure cannot move its captures
//! out, so the loop's exhaustion branch could never hand the accumulator to the
//! code after the loop.
//!
//! Instead, the driver's argument type becomes an enum of *entry points*: one for
//! the function itself (`E0`) and one per lowered loop. A loop's state — its
//! iterator plus the locals live across it — travels in that entry's payload, and
//! one iteration is a `Tail` step: re-enter the body at the loop's entry point
//! without pushing a frame. So the iterator does live in the frame, as intended;
//! it is threaded by value instead of mutated in place, which needs no `unsafe`.
//!
//! The locals to thread are found syntactically: the transform tracks which
//! bindings are in scope, and intersects that with the identifiers appearing in
//! the generated entry-point code (see [`loop_state::solve_payloads`]).
//!
//! # `&mut` parameters and methods
//!
//! A `&mut` parameter cannot ride in the argument payload: the payload is moved
//! into a continuation, so two live frames would hold the same `&mut` (E0505).
//! Such a parameter becomes part of a *context* tuple that the driver owns and
//! lends out, one reborrow per body invocation and per continuation resume. Nothing
//! captures it, so nothing has to be unsafe. Shared references stay in the payload;
//! they are `Copy`.
//!
//! A method needs no rule of its own. `self` is desugared away first: the receiver
//! becomes an ordinary first parameter of a generated associated function and the
//! method forwards to it, so `&mut self` is simply a `&mut` parameter and `&self` a
//! shared one (see [`analyze::desugar_receiver`]).
//!
//! The context is shared down the whole recursion, so a recursive call must pass
//! the same reference. Recursing into a place *derived* from it
//! (`walk(&mut t.kids[i])`) needs the slot swapped for the child's subtree and
//! restored afterwards, which a `&mut` cannot express — hence
//! `#[stack_safe(use_nonlinear_mut)]`, under which such a slot holds a raw
//! pointer. See `README.md` for the invariant that opt-in asks of the caller.

use proc_macro2::{Ident, TokenStream};
use syn::ItemFn;
use syn::spanned::Spanned;

mod analyze;
mod context;
mod cps;
mod emit;
mod group;
mod leaf;
mod loop_state;
mod names;
mod scan;
mod scope;
mod try_shim;
mod walk;

/// Entry point for the one attribute, whichever kind of item it is on.
///
/// A function is rewritten on its own. A module or an impl block is a *container*: every
/// function inside that recurses, alone or through the others, is rewritten, and the rest
/// pass through. One attribute serves both because the container case is what mutual
/// recursion needs — expanding `f` requires `g`'s body — and a single function is just a
/// container of one.
pub fn expand_attr(attr: TokenStream, item: TokenStream) -> syn::Result<TokenStream> {
    scan::Scope::parse(item)?.expand(Opts::parse(attr)?, true)
}

/// `#[stack_safe(..)]` flags.
#[derive(Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct Opts {
    /// Allow a recursive call to pass a reference *derived* from a context
    /// parameter (`walk(&mut t.kids[i])`). `analyze::scan_context_args` decides
    /// which slots that forces onto a raw pointer, by setting `CtxEntry::raw`.
    pub(super) use_nonlinear_mut: bool,
    /// Allow a recursive call to pass a reference to a value built *at the call
    /// site* (`rec(n, &Node::Cons(v, rest))`). The value is boxed and owned by the
    /// frame, and the callee reaches it through a raw pointer, since the caller's
    /// frame no longer exists by the time the callee runs.
    pub(super) data_in_frame: bool,
}

impl Opts {
    pub(super) fn parse(attr: TokenStream) -> syn::Result<Self> {
        let mut opts = Opts::default();
        if attr.is_empty() {
            return Ok(opts);
        }
        let flags = syn::parse::Parser::parse2(
            syn::punctuated::Punctuated::<Ident, syn::Token![,]>::parse_terminated,
            attr,
        )?;
        for flag in flags {
            if flag == "use_nonlinear_mut" {
                opts.use_nonlinear_mut = true;
            } else if flag == "data_in_frame" {
                opts.data_in_frame = true;
            } else {
                return Err(syn::Error::new(
                    flag.span(),
                    format!(
                        "unknown `#[stack_safe]` option `{flag}`; the options are \
                         `use_nonlinear_mut` and `data_in_frame`"
                    ),
                ));
            }
        }
        Ok(opts)
    }

    /// Is this attribute this very one?
    ///
    /// A marker on a function inside the scope is recognised by its name, since a macro resolves
    /// no paths: `#[stack_safe]` as imported, or `yaspar_macros::stack_safe` written out. An
    /// attribute of the same name from anywhere else is somebody else's and is left alone — as is
    /// this one under an alias (`use yaspar_macros::stack_safe as ss;` then `#[ss]`), which
    /// nothing in the tokens ties back to this crate.
    pub(super) fn is_marker(attr: &syn::Attribute) -> bool {
        let path = attr.path();
        let segments = &path.segments;
        segments
            .last()
            .is_some_and(|last| last.ident == "stack_safe")
            && match segments.len() {
                1 => true,
                2 => segments[0].ident == "yaspar_macros",
                _ => false,
            }
    }

    /// Take a function's own `#[stack_safe]`, leaving its other attributes, and read what it
    /// asked for. `Some` therefore means the marker was written by hand — which is worth
    /// knowing, since one that turns out to cover no recursion is a mistake.
    ///
    /// A marker inside a scope the attribute already covers asks for options rather than for an
    /// expansion of its own, so it is removed: left in place it would expand a second time, on a
    /// function with nothing left to rewrite.
    pub(super) fn take_from(func: &mut ItemFn) -> syn::Result<Option<Self>> {
        let mut found: Option<Self> = None;
        let prev_attrs = std::mem::take(&mut func.attrs);
        let mut attrs = Vec::with_capacity(prev_attrs.len());
        for attr in prev_attrs {
            if !Self::is_marker(&attr) {
                attrs.push(attr);
                continue;
            }
            let tokens = match &attr.meta {
                syn::Meta::Path(_) => TokenStream::new(),
                syn::Meta::List(list) => list.tokens.clone(),
                syn::Meta::NameValue(nv) => {
                    return Err(syn::Error::new(
                        nv.span(),
                        "`#[stack_safe]` takes a list of options, as in \
                         `#[stack_safe(use_nonlinear_mut)]`",
                    ));
                }
            };
            let new = Opts::parse(tokens)?;
            found = match found {
                None => Some(new),
                Some(prev) => Some(prev.merge(new)),
            };
        }
        func.attrs = attrs;
        Ok(found)
    }

    /// The options as written, for an error message that has to name them.
    pub(super) fn flags(self) -> String {
        let mut names = Vec::new();
        if self.use_nonlinear_mut {
            names.push("use_nonlinear_mut");
        }
        if self.data_in_frame {
            names.push("data_in_frame");
        }
        if names.is_empty() {
            "none".to_owned()
        } else {
            names.join(", ")
        }
    }

    /// Both sets of options, since a group obeys every option any member asked for.
    pub(super) fn merge(self, other: Self) -> Self {
        Opts {
            use_nonlinear_mut: self.use_nonlinear_mut || other.use_nonlinear_mut,
            data_in_frame: self.data_in_frame || other.data_in_frame,
        }
    }
}
