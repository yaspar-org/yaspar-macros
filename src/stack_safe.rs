// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Implementation of `#[stack_safe]`: rewrite recursive functions (or cycles of them) into
//! an iterative state machine whose frames live on the heap.
//!
//! # Design
//!
//! - **CPS.** Each recursive call becomes a request to run the body on new arguments plus a
//!   continuation for the rest. An inlined driver loop (see `driver`) keeps parked
//!   continuations in a `Vec`.
//! - **Frame enum.** Continuations are defunctionalized: one variant per call site, carrying
//!   the locals live across the call (see [`loop_state::solve_payloads`]). Payload types are
//!   left generic and inferred.
//! - **Entry enum.** One variant for the function (`E0`) and one per loop that recurses. A loop
//!   iteration re-enters the body at that entry without parking a frame, threading the
//!   iterator by value.
//!   `for i in a..b` steps the iterator at the end of each iteration, so `i` can be read back
//!   out of it (see `walk::Derived`).
//! - **Context.** `&mut` parameters (including `&mut self`, after
//!   [`analyze::desugar_receiver`]) live in a tuple the driver owns and reborrows per step.
//!   `use_nonlinear_mut` turns a slot into a raw pointer so derived places can be passed.
//! - **Runtime.** The fixed parts (`Frames`, `push`, `Pin`, `Try` / `FromResidual`, range
//!   helpers) live in `yaspar-macros-defs` and are imported under `__ss` names
//!   (see [`names::defs_imports`]).
//!
//! Modules: `scan` / `scope` parse the annotated item, `group` finds cycles, `analyze` checks
//! and classifies, `walk` / `cps` / `loop_state` transform bodies, `emit` / `driver` / `leaf` /
//! `context` / `names` / `try_shim` generate output.

use proc_macro2::{Ident, TokenStream, TokenTree};
use syn::spanned::Spanned;

mod analyze;
mod context;
mod cps;
mod driver;
mod emit;
mod group;
mod leaf;
mod loop_state;
mod names;
mod scan;
mod scope;
mod try_shim;
mod walk;

/// Expand `#[stack_safe]` on a function, module, or impl block. A function is treated as a
/// container of one.
pub fn expand_attr(attr: TokenStream, item: TokenStream) -> syn::Result<TokenStream> {
    already_expanded(&item)?;
    scan::Scope::parse(item)?.expand_annotated(Opts::parse(attr)?)
}

/// Reject input containing a reserved `__ss` / `__Ss` name (raw spellings included). This
/// catches re-expansion through an aliased marker (`#[ss]`) and user names that would clash.
fn already_expanded(item: &TokenStream) -> syn::Result<()> {
    fn generated(tokens: TokenStream) -> Option<Ident> {
        fn reserved(id: &Ident) -> bool {
            let name = id.to_string();
            let name = name.strip_prefix("r#").unwrap_or(&name);
            name.starts_with("__ss") || name.starts_with("__Ss")
        }

        for tt in tokens {
            match tt {
                TokenTree::Ident(id) if reserved(&id) => {
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
                "`{id}` is a name `#[stack_safe]` reserves: everything it generates is \
                 `__ss`-prefixed, and a raw spelling is the same identifier. Two things bring you \
                 here. Either this item has already been rewritten, which is what a marker behind \
                 an alias does — `use yaspar_macros::stack_safe as ss;` and then `#[ss]` is not \
                 recognised, since a macro resolves no paths, so it is left in place and runs \
                 again on the rewritten body; write the inner marker as `#[stack_safe(..)]` or \
                 `#[yaspar_macros::stack_safe(..)]`. Or the name is one you wrote yourself, in \
                 which case rename it"
            ),
        )),
    }
}

/// `#[stack_safe(..)]` flags.
#[derive(Default, Clone, PartialEq, Eq)]
pub(super) struct Opts {
    /// Allow passing a place derived from a context parameter (`walk(&mut t.kids[i])`).
    /// `analyze::scan_context_args` sets `CtxEntry::raw` on the affected slots.
    pub(super) use_nonlinear_mut: bool,
    /// Allow passing a reference to a value built at the call site
    /// (`rec(n, &Node::Cons(v, rest))`). The value is stored by the driver and reached through
    /// a raw pointer.
    pub(super) data_in_frame: bool,
    /// Appended to a root's name to name its uncalled copy as written (see
    /// `scan::originals`). `None` means [`DEFAULT_ORIGINAL_SUFFIX`].
    pub(super) original_suffix: Option<String>,
}

/// The suffix of an original's copy when `original_suffix` is not given.
const DEFAULT_ORIGINAL_SUFFIX: &str = "_orig";

/// All options, in error-message order.
const FLAGS: [&str; 3] = ["use_nonlinear_mut", "data_in_frame", "original_suffix"];

/// Error for an unknown option, suggesting the nearest valid one.
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
            "unknown `#[stack_safe]` option `{written}`; the options are `{}`, `{}` and \
             `{}`{hint}",
            FLAGS[0], FLAGS[1], FLAGS[2],
        ),
    )
}

/// Closest option by Levenshtein distance, within a third of its length.
fn nearest(written: &str) -> Option<&'static str> {
    fn distance(a: &str, b: &str) -> usize {
        let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
        // `row[j]` is the distance from `a[..i]` to `b[..j]`.
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
    /// Parse the attribute arguments. Parsed as `Meta` so `flag = value` gets a named error.
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
            if name == "original_suffix" {
                opts.parse_original_suffix(meta)?;
                continue;
            }
            let flag = match name.as_str() {
                "use_nonlinear_mut" => &mut opts.use_nonlinear_mut,
                "data_in_frame" => &mut opts.data_in_frame,
                _ => return Err(unknown_flag(path)),
            };
            if !matches!(meta, syn::Meta::Path(_)) {
                return Err(syn::Error::new(
                    meta.span(),
                    format!("`{name}` is a flag and takes no value: write `#[stack_safe({name})]`"),
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

    /// Parse `original_suffix = "..."`, which must extend any identifier to another.
    fn parse_original_suffix(&mut self, meta: &syn::Meta) -> syn::Result<()> {
        let usage = "`original_suffix` takes a string: write \
                     `#[stack_safe(original_suffix = \"_orig\")]`";
        let syn::Meta::NameValue(nv) = meta else {
            return Err(syn::Error::new(meta.span(), usage));
        };
        let syn::Expr::Lit(syn::ExprLit {
            lit: syn::Lit::Str(lit),
            ..
        }) = &nv.value
        else {
            return Err(syn::Error::new(nv.value.span(), usage));
        };
        if self.original_suffix.is_some() {
            return Err(syn::Error::new(
                nv.path.span(),
                "`original_suffix` is given twice; give it once",
            ));
        }
        let suffix = lit.value();
        if suffix.is_empty() {
            return Err(syn::Error::new(
                lit.span(),
                "`original_suffix` cannot be empty: the copy would take the function's own name",
            ));
        }
        if syn::parse_str::<Ident>(&format!("f{suffix}")).is_err() {
            return Err(syn::Error::new(
                lit.span(),
                format!(
                    "`original_suffix` must be made of identifier characters, so that \
                     `f{suffix}` names a function; `{suffix}` is not"
                ),
            ));
        }
        self.original_suffix = Some(suffix);
        Ok(())
    }

    /// The suffix naming an original's copy.
    pub(super) fn original_suffix(&self) -> &str {
        self.original_suffix
            .as_deref()
            .unwrap_or(DEFAULT_ORIGINAL_SUFFIX)
    }

    /// Whether the options that change the rewrite agree. `original_suffix` only names the
    /// copy, so cycle members may differ in it.
    pub(super) fn same_rewrite(&self, other: &Self) -> bool {
        self.use_nonlinear_mut == other.use_nonlinear_mut
            && self.data_in_frame == other.data_in_frame
    }

    /// Whether `attr` is `#[stack_safe]`, matched by last path segment. Aliases are caught later
    /// by [`already_expanded`].
    pub(super) fn is_marker(attr: &syn::Attribute) -> bool {
        attr.path()
            .segments
            .last()
            .is_some_and(|last| last.ident == "stack_safe")
    }

    /// Remove any `#[stack_safe]` markers from `attrs` and return their merged options, or
    /// `None` if there were none.
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

    /// The enabled options, for error messages.
    pub(super) fn flags(&self) -> String {
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

    /// Union of both option sets; a later `original_suffix` wins.
    pub(super) fn merge(self, other: Self) -> Self {
        Opts {
            use_nonlinear_mut: self.use_nonlinear_mut || other.use_nonlinear_mut,
            data_in_frame: self.data_in_frame || other.data_in_frame,
            original_suffix: other.original_suffix.or(self.original_suffix),
        }
    }
}
