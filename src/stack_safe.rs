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

use proc_macro2::{Ident, TokenStream, TokenTree};
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
    already_expanded(&item)?;
    scan::Scope::parse(item)?.expand(Opts::parse(attr)?, true)
}

/// Refuse an item that is already an expansion of this very attribute.
///
/// A marker inside a scope the attribute covers is recognised *by name*, since a macro resolves no
/// paths — so one written under an alias (`use yaspar_macros::stack_safe as ss;` then `#[ss]`) is
/// left where it stands, and the compiler runs it a second time on the body the first run rewrote.
/// Everything that run generated is `__ss`-prefixed, which nothing the user wrote may be, so
/// finding such a name in the input says exactly that. Reported here rather than left to whatever
/// the second run happens to make of an already-transformed body.
fn already_expanded(item: &TokenStream) -> syn::Result<()> {
    fn generated(tokens: TokenStream) -> Option<Ident> {
        for tt in tokens {
            match tt {
                TokenTree::Ident(id)
                    if id.to_string().starts_with("__ss") || id.to_string().starts_with("__Ss") =>
                {
                    return Some(id);
                }
                TokenTree::Group(g) => {
                    if let Some(found) = generated(g.stream()) {
                        return Some(found);
                    }
                }
                _ => {}
            }
        }
        None
    }

    match generated(item.clone()) {
        None => Ok(()),
        Some(id) => Err(syn::Error::new(
            id.span(),
            format!(
                "`#[stack_safe]` has already rewritten this item: `{id}` is one of the names it \
                 generates. A marker inside a scope this attribute covers is recognised by name \
                 only, since a macro resolves no paths, so an alias — `use \
                 yaspar_macros::stack_safe as ss;` and then `#[ss]` — is not recognised, is left \
                 in place, and runs again on the rewritten body. Write the inner marker as \
                 `#[stack_safe(..)]` or `#[yaspar_macros::stack_safe(..)]`"
            ),
        )),
    }
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

/// Every option there is, in the order an error message lists them.
const FLAGS: [&str; 2] = ["use_nonlinear_mut", "data_in_frame"];

/// An option this attribute does not have, with the nearest one it does named where there is one.
///
/// A typo is the usual reason to be here, and the list alone leaves the reader to spot which of
/// them they meant to write.
fn unknown_flag(path: &syn::Path) -> syn::Error {
    let written = path
        .get_ident()
        .map(Ident::to_string)
        .unwrap_or_else(|| quote::ToTokens::to_token_stream(path).to_string());
    let hint = match nearest(&written) {
        Some(flag) => format!(" — did you mean `{flag}`?"),
        None => String::new(),
    };
    syn::Error::new(
        path.span(),
        format!(
            "unknown `#[stack_safe]` option `{written}`; the options are `{}` and `{}`{hint}",
            FLAGS[0], FLAGS[1],
        ),
    )
}

/// The option this was most likely meant to be, if any is close enough.
///
/// Levenshtein distance, bounded at a third of the option's length, so a genuinely different word
/// gets no suggestion at all: `data_in_fram` is `data_in_frame` mistyped, and `keep_frames` is not.
fn nearest(written: &str) -> Option<&'static str> {
    fn distance(a: &str, b: &str) -> usize {
        let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
        // One row of the matrix at a time: `row[j]` is the distance from `a[..i]` to `b[..j]`.
        let mut row: Vec<usize> = (0..=b.len()).collect();
        for (i, ca) in a.iter().enumerate() {
            let mut prev = row[0];
            row[0] = i + 1;
            for (j, cb) in b.iter().enumerate() {
                let cost = usize::from(ca != cb);
                let next = (row[j] + 1).min(row[j + 1] + 1).min(prev + cost);
                prev = row[j + 1];
                row[j + 1] = next;
            }
        }
        row[b.len()]
    }

    FLAGS
        .into_iter()
        .map(|flag| (distance(written, flag), flag))
        .filter(|&(d, flag)| d <= flag.len() / 3)
        .min_by_key(|&(d, _)| d)
        .map(|(_, flag)| flag)
}

impl Opts {
    /// The options as written between the parentheses.
    ///
    /// A `Meta` rather than a bare identifier, so that a value someone tried to give a flag is
    /// *parsed* and then rejected by name: `#[stack_safe(data_in_frame = true)]` otherwise gets
    /// `expected ,` pointing at the `=`, which says nothing about the option or the attribute.
    /// Repeating an option is rejected for the same reason — it means the writer expected it to do
    /// something the second time.
    pub(super) fn parse(attr: TokenStream) -> syn::Result<Self> {
        let mut opts = Opts::default();
        if attr.is_empty() {
            return Ok(opts);
        }
        let metas = syn::parse::Parser::parse2(
            syn::punctuated::Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated,
            attr,
        )?;
        for meta in &metas {
            let path = meta.path();
            let Some(name) = path.get_ident().map(Ident::to_string) else {
                return Err(unknown_flag(path));
            };
            let flag = match name.as_str() {
                "use_nonlinear_mut" => &mut opts.use_nonlinear_mut,
                "data_in_frame" => &mut opts.data_in_frame,
                _ => return Err(unknown_flag(path)),
            };
            if !matches!(meta, syn::Meta::Path(_)) {
                return Err(syn::Error::new(
                    meta.span(),
                    format!(
                        "`{name}` is a flag and takes no value: write `#[stack_safe({name})]`"
                    ),
                ));
            }
            if *flag {
                return Err(syn::Error::new(
                    path.span(),
                    format!("`{name}` is given twice; one `#[stack_safe({name})]` is enough"),
                ));
            }
            *flag = true;
        }
        Ok(opts)
    }

    /// Is this attribute this very one?
    ///
    /// A marker inside the scope is recognised by its *last* segment, since a macro resolves no
    /// paths and so cannot tell one route to this attribute from another: `#[stack_safe]` as
    /// imported, `#[yaspar_macros::stack_safe]`, `#[ym::stack_safe]` under a renamed `extern
    /// crate`, `#[crate::stack_safe]` through a re-export — all of them lead here, and demanding a
    /// particular prefix only means the others are left in the output to expand a second time on a
    /// body that has already been rewritten.
    ///
    /// The price is that an attribute of the same name from somewhere else is read as this one. The
    /// name is specific enough that this is the better trade: the alternative is silently dropping
    /// the options of every marker not spelled one of two ways.
    ///
    /// What no name can catch is this attribute under an *alias* (`use yaspar_macros::stack_safe as
    /// ss;` then `#[ss]`), where nothing in the tokens ties back to this crate at all. That one is
    /// caught after the fact instead, by [`already_expanded`].
    pub(super) fn is_marker(attr: &syn::Attribute) -> bool {
        attr.path()
            .segments
            .last()
            .is_some_and(|last| last.ident == "stack_safe")
    }

    /// Take an item's own `#[stack_safe]`, leaving its other attributes, and read what it
    /// asked for. `Some` therefore means the marker was written by hand — which is worth
    /// knowing, since one that turns out to cover no recursion is a mistake.
    ///
    /// A marker inside a scope the attribute already covers asks for options rather than for an
    /// expansion of its own, so it is removed: left in place it would expand a second time, on a
    /// function with nothing left to rewrite.
    ///
    /// Over the attributes rather than over a function, because the rule is the item's and not the
    /// function's: a `mod` or an `impl` block inside the scope is a subtree of it, and a marker
    /// there says what that subtree wants exactly as one on a `fn` does.
    pub(super) fn take_from(attrs: &mut Vec<syn::Attribute>) -> syn::Result<Option<Self>> {
        let mut found: Option<Self> = None;
        let prev_attrs = std::mem::take(attrs);
        let mut kept = Vec::with_capacity(prev_attrs.len());
        for attr in prev_attrs {
            if !Self::is_marker(&attr) {
                kept.push(attr);
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
        *attrs = kept;
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
