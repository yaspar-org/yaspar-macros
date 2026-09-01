// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Implementation of `#[delegatable_trait]` and `#[delegate_trait]` — write the trait
//! methods you want to override, and forward the rest to an inner field.
//!
//! # Why two attributes
//!
//! An attribute on the impl block sees only the impl block. It cannot know which
//! methods the trait requires, so it cannot know which ones are missing and need
//! forwarding — the trait definition may not even be in this crate.
//!
//! The signatures therefore have to travel from the trait to the impl, and the only
//! carrier a proc macro can emit that is visible to a *later* macro expansion is a
//! `macro_rules!` macro. So `#[delegatable_trait]` emits the trait unchanged plus a
//! hidden `__delegate_impl_<Trait>!` macro holding one arm per required method, and
//! `#[delegate_trait]` expands to an impl block containing the user's methods and an
//! invocation of that macro, which fills in the remainder.
//!
//! # How a generic trait's parameters travel
//!
//! Replaying a signature verbatim would emit `fn get(&self, k: K)` into
//! `impl Store<u32> for Wrapper`, where `K` names nothing. The two sides again know
//! half each: the trait knows the parameter *names*, the impl knows the actual
//! *arguments*. So the helper macro carries the substitution — each of the trait's
//! parameters becomes a metavariable in the recorded signatures, and
//! `#[delegate_trait]` passes the impl's trait arguments positionally:
//!
//! ```text
//! trait Store<K>            ->  macro_rules! __delegate_impl_Store {
//!     fn get(&self, k: K);         ($self:path, $field:ident, .., $__dt_ty_K:ty) => {
//!                                      fn get(&self, k: $__dt_ty_K) { .. } } }
//!
//! impl Store<u32> for W     ->  __delegate_impl_Store!(
//!                                   __delegate_impl_Store, inner, [], Store<u32>, u32);
//! ```
//!
//! Every arm takes the macro's own path as its first argument and passes it on, rather
//! than recursing through the bare name. A bare name in a macro body resolves at the
//! *call site*, so it only works while the macro lives at the crate root; a path works
//! from anywhere, which is what the addressing below relies on.
//!
//! A lifetime becomes a `lifetime` fragment. A const parameter becomes an `expr` —
//! there is no `const` fragment — and every use of it is *braced*: `[u8; { $n }]`
//! and `Holder<{ $n }>`. Braces are what let an expression stand where a const
//! argument is expected, and they are accepted in both positions, so one rewrite
//! serves them all.
//!
//! The parameters travel in declaration order. Rust puts lifetimes first but lets
//! types and consts interleave, so they are kept in one ordered list rather than
//! grouped by kind — grouping would silently reorder the arguments.
//!
//! A defaulted parameter may be left out by the impl. Only the trait knows the
//! default, so the trait emits an extra arm per omissible argument that forwards to
//! the full form with its own defaults filled in.
//!
//! Nothing has to be excluded from that rewrite: a method cannot redeclare one of
//! the trait's parameters (`fn get<K>(..)` inside `trait Store<K>` is E0403, and the
//! lifetime form is E0496), so every mention is the trait's own.
//!
//! # Why the skip list is matched inside `macro_rules!`
//!
//! `#[delegate_trait]` knows the names to skip (the methods the user wrote) but not
//! the signatures; the helper macro knows the signatures but not the names to skip.
//! Set subtraction has to happen where both are available, which is inside the helper
//! macro — hence the `@maybe` arms, which walk the skip list one element at a time:
//! a literal-ident arm per method name absorbs a match, a generic arm pops a
//! non-match and recurses, and reaching the empty list emits the delegation.
//!
//! # Limitations
//!
//! Only required *methods* are delegated. A required associated type or associated
//! const is not, so a trait that has one must have it supplied by the impl block as
//! usual. Methods with a default body are left to their default.
//!
//! # How the impl finds the helper macro
//!
//! `#[macro_export]` puts the helper at the *root* of the defining crate, so its bare
//! name resolves anywhere in that crate. It does not, however, put it in a dependent
//! crate's scope: a consumer would have to import it by hand, and its name is hidden
//! precisely so that nobody has to know it.
//!
//! So the trait also emits an alias beside itself, under a *second* name:
//!
//! ```text
//! mod a {
//!     pub trait Store { .. }
//!     macro_rules! __delegate_impl_Store { .. }                    // the helper
//!     pub use __delegate_impl_Store as __delegate_path_Store;      // and a path to it
//! }
//! ```
//!
//! An impl then addresses the helper exactly as it addresses the trait: the last segment
//! of the trait path is swapped for the alias, so `impl libx::a::Store for W` invokes
//! `libx::a::__delegate_path_Store!`. That works in this crate and from a dependent one,
//! with nothing to import.
//!
//! A trait named *bare*, because it was imported, leaves nothing to qualify with, so
//! that case falls back to the crate-root name. Within the defining crate this is
//! equivalent; from another crate it is the one form that still needs an import, and
//! writing the trait path instead is the easier fix.
//!
//! The alias has to be a *relative* `use`. Referring to the macro as
//! `crate::__delegate_impl_Store` is rejected with "macro-expanded `macro_export` macros
//! from the current crate cannot be referred to by absolute paths", since the
//! `macro_rules!` is itself produced by this macro's expansion.
//!
//! # Two traits of the same name: `local`
//!
//! The exported helper lands at the crate root under a name derived from the trait's
//! last path segment, so two `#[delegatable_trait]` traits of the same name in one crate
//! collide with an `E0428` naming `__delegate_impl_<Trait>`.
//!
//! `#[delegatable_trait(local)]` drops the export, leaving only the module-local
//! `macro_rules!` and a `pub(crate)` alias. Nothing reaches the crate root, so the two
//! traits coexist, and the impl side is unchanged: it addresses the alias by path as
//! always. The trade is that a macro which is not `#[macro_export]`ed is crate-private
//! and cannot be re-exported out (`E0364`, and `pub use` of it does not compile), so a
//! `local` trait cannot be delegated from another crate; the attempt is an `E0603`.
//!
//! That is also why the export cannot simply be dropped for everyone, and why the two
//! forms are exclusive rather than both emitted: a second `macro_rules!` of the same
//! name would be an `E0428` in its own right.

use proc_macro2::{Span, TokenStream, TokenTree};
use quote::{ToTokens, format_ident, quote};
use syn::parse::{Parse, ParseStream};
use syn::spanned::Spanned;
use syn::{GenericParam, Ident, ItemImpl, ItemTrait, PathArguments, Token, TraitItem};

/// `target = <field>` — the field every missing method is forwarded to.
struct DelegateTraitArgs {
    target: Ident,
}

impl Parse for DelegateTraitArgs {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        if input.is_empty() {
            return Err(input.error(
                "`#[delegate_trait]` needs the field to forward to, as in \
                 `#[delegate_trait(target = inner)]`",
            ));
        }
        let key: Ident = input.parse()?;
        if key != "target" {
            return Err(syn::Error::new(
                key.span(),
                format!(
                    "expected `target`, found `{key}`; `#[delegate_trait]` takes only \
                     `target = <field>`"
                ),
            ));
        }
        input.parse::<Token![=]>()?;
        // `self` is a keyword, so it would never reach the `Ident` parse below —
        // and it is the mistake the documentation specifically warns about.
        if input.peek(Token![self]) {
            return Err(input.error(
                "`target` is a field name, not an expression: write `target = inner`, \
                 not `target = self.inner`",
            ));
        }
        Ok(DelegateTraitArgs {
            target: input.parse()?,
        })
    }
}

/// Entry point for the `#[delegatable_trait]` attribute.
///
/// Emits the trait unchanged, plus a hidden helper macro that knows all of the
/// trait's required method signatures.
pub fn expand_trait_def(attr: TokenStream, item: TokenStream) -> syn::Result<TokenStream> {
    let local = if attr.is_empty() {
        false
    } else {
        let flag = syn::parse2::<Ident>(attr.clone()).map_err(|_| {
            syn::Error::new(
                attr.span(),
                "`#[delegatable_trait]` takes no arguments, or the single flag `local`",
            )
        })?;
        if flag != "local" {
            return Err(syn::Error::new(
                flag.span(),
                format!("expected `local`, found `{flag}`"),
            ));
        }
        true
    };
    let trait_def = syn::parse2::<ItemTrait>(item)?;
    let trait_name = &trait_def.ident;
    let helper_macro_name = helper_macro_name(trait_name);
    let params = TraitParams::collect(&trait_def.generics);

    // Methods with a default body keep it: delegating them would silently override
    // the trait author's intent for every wrapper.
    let method_sigs: Vec<_> = trait_def
        .items
        .iter()
        .filter_map(|item| match item {
            TraitItem::Fn(method) if method.default.is_none() => Some(method.sig.clone()),
            _ => None,
        })
        .collect();

    let method_names: Vec<Ident> = method_sigs.iter().map(|sig| sig.ident.clone()).collect();
    let method_bodies: Vec<TokenStream> = method_sigs
        .iter()
        .map(|sig| delegating_method(sig, &params))
        .collect();

    // Both are empty for a non-generic trait, leaving those expansions unchanged.
    let matcher = params.matcher_tail();
    let forward = params.forward_tail();

    // Arms for impls that leave defaulted arguments out, and a last-resort arm for
    // an argument count that matches nothing, which would otherwise fail with a
    // wall of "no rules expected this token".
    let defaulted_arms = params.defaulted_arms();
    let arity_guard = if params.is_empty() {
        TokenStream::new()
    } else {
        let expected = params.0.len();
        let msg = format!(
            "`#[delegate_trait]` for `{trait_name}`: the impl block passes the wrong number of \
             generic arguments; `{trait_name}` takes {expected}",
        );
        quote! {
            ($($__dt_unmatched:tt)*) => { ::core::compile_error!(#msg); };
        }
    };

    // The helper is emitted once and then given a *path* beside the trait, under the
    // second name so that the alias does not redefine the first one. An impl can then
    // address it exactly as it addresses the trait — `libx::a::Store` pairs with
    // `libx::a::__delegate_path_Store` — with nothing to import.
    //
    // `local` additionally keeps the macro out of the crate root, which is the only
    // place two same-named traits can collide. It cannot be the default, since a macro
    // that is not exported cannot leave its crate at all.
    let path_alias = path_alias_name(trait_name);
    let (export, alias) = if local {
        (
            TokenStream::new(),
            quote! {
                #[doc(hidden)]
                #[allow(unused_imports)]
                pub(crate) use #helper_macro_name as #path_alias;
            },
        )
    } else {
        (
            quote! { #[macro_export] },
            // `#[macro_export]` puts the macro at the crate root, so that is where the
            // alias reads it from. `pub`, so a dependent crate can follow the path too.
            quote! {
                #[doc(hidden)]
                pub use #helper_macro_name as #path_alias;
            },
        )
    };

    // Every arm takes the macro's own path and passes it on, rather than recursing
    // through the bare name: a bare name resolves at the *call site*, which only
    // works while the macro lives at the crate root.
    Ok(quote! {
        #trait_def

        #[doc(hidden)]
        #export
        macro_rules! #helper_macro_name {
            ($self:path, $field:ident, [$($skip:ident),*], $trait_path:path #matcher) => {
                #(
                    $self!(
                        @maybe $self, #method_names, $field, [$($skip),*], $trait_path #forward
                    );
                )*
            };

            // Skip: method name matches the head of the skip list.
            #(
                (@maybe $self:path, #method_names, $field:ident, [#method_names $(, $($rest:ident),*)?], $trait_path:path #matcher) => {};
            )*

            // No match on the head: pop it and recurse.
            (@maybe $self:path, $method:ident, $field:ident, [$first:ident $(, $($rest:ident),*)?], $trait_path:path #matcher) => {
                $self!(@maybe $self, $method, $field, [$($($rest),*)?], $trait_path #forward);
            };

            // Skip list exhausted: the user did not write this method, so delegate it.
            #(
                (@maybe $self:path, #method_names, $field:ident, [], $trait_path:path #matcher) => {
                    #method_bodies
                };
            )*

            #defaulted_arms
            #arity_guard
        }
        #alias
    })
}

/// Entry point for the `#[delegate_trait]` attribute.
///
/// Re-emits the impl block with the user's items kept and an invocation of the
/// trait's helper macro appended, which supplies every method the user left out.
pub fn expand_trait_impl(attr: TokenStream, item: TokenStream) -> syn::Result<TokenStream> {
    let args = syn::parse2::<DelegateTraitArgs>(attr)?;
    let impl_block = syn::parse2::<ItemImpl>(item)?;

    let Some((trait_path, _)) = &impl_block.trait_ else {
        return Err(syn::Error::new(
            Span::call_site(),
            "`#[delegate_trait]` requires a trait impl block",
        ));
    };

    let target = &args.target;
    let self_ty = &impl_block.self_ty;
    let impl_attrs = &impl_block.attrs;
    let unsafety = &impl_block.unsafety;
    let (impl_generics, _, where_clause) = impl_block.generics.split_for_impl();

    // Names the helper macro must not emit: the user has written them already.
    let override_idents: Vec<Ident> = impl_block
        .items
        .iter()
        .filter_map(|item| match item {
            syn::ImplItem::Fn(m) => Some(m.sig.ident.clone()),
            _ => None,
        })
        .collect();

    let user_items = &impl_block.items;

    let last = trait_path
        .segments
        .last()
        .expect("a parsed trait path has at least one segment");
    let helper_macro_name = helper_macro_name(&last.ident);

    // The trait's generic arguments, positionally: the helper macro substitutes
    // them for the trait's parameters in every recorded signature.
    let generic_args: Vec<TokenStream> = match &last.arguments {
        PathArguments::None => Vec::new(),
        PathArguments::AngleBracketed(ab) => ab.args.iter().map(|a| quote! { #a }).collect(),
        PathArguments::Parenthesized(p) => {
            return Err(syn::Error::new(
                p.span(),
                "`#[delegate_trait]` does not support the `Fn(..)` sugar in the trait path",
            ));
        }
    };
    let args_tail = if generic_args.is_empty() {
        TokenStream::new()
    } else {
        quote! { , #(#generic_args),* }
    };

    // The helper is addressed the same way the trait is: `impl libx::a::Store for W`
    // reaches it as `libx::a::__delegate_path_Store`, which works across crates and
    // needs no import. A trait named bare — because it was imported — leaves nothing to
    // qualify with, so that case falls back to the crate-root name, which is in scope
    // anywhere in the defining crate.
    let helper_path = if trait_path.segments.len() > 1 {
        let prefix = trait_path.segments.iter().rev().skip(1).rev();
        let leading = trait_path.leading_colon;
        let alias = path_alias_name(&last.ident);
        quote! { #leading #(#prefix ::)* #alias }
    } else {
        quote! { #helper_macro_name }
    };

    Ok(quote! {
        #(#impl_attrs)*
        #unsafety impl #impl_generics #trait_path for #self_ty #where_clause {
            #(#user_items)*
            #helper_path!(#helper_path, #target, [#(#override_idents),*], #trait_path #args_tail);
        }
    })
}

/// One of the trait's own generic parameters.
struct Param {
    kind: ParamKind,
    /// As written, without the tick for a lifetime.
    name: String,
    /// The metavariable it becomes in the recorded signatures.
    meta: Ident,
    /// `trait Store<K = u32>`. Only the trait knows this, so only the trait can
    /// fill it in for an impl that leaves the argument out.
    default: Option<TokenStream>,
}

enum ParamKind {
    Lifetime,
    Type,
    Const,
}

/// The trait's own generic parameters, in declaration order — which is the order an
/// impl must write its arguments in, and they are matched positionally. Rust puts
/// lifetimes first but lets types and consts interleave, so one ordered list is the
/// only representation that cannot get them out of step.
struct TraitParams(Vec<Param>);

impl TraitParams {
    fn collect(generics: &syn::Generics) -> Self {
        TraitParams(
            generics
                .params
                .iter()
                .map(|param| match param {
                    GenericParam::Lifetime(l) => Param {
                        kind: ParamKind::Lifetime,
                        name: l.lifetime.ident.to_string(),
                        meta: format_ident!("__dt_lt_{}", l.lifetime.ident),
                        default: None,
                    },
                    GenericParam::Type(t) => Param {
                        kind: ParamKind::Type,
                        name: t.ident.to_string(),
                        meta: format_ident!("__dt_ty_{}", t.ident),
                        default: t.default.as_ref().map(|(_, ty)| quote! { #ty }),
                    },
                    GenericParam::Const(c) => Param {
                        kind: ParamKind::Const,
                        name: c.ident.to_string(),
                        meta: format_ident!("__dt_ct_{}", c.ident),
                        default: c.default.as_ref().map(|(_, expr)| quote! { #expr }),
                    },
                })
                .collect(),
        )
    }

    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// `, $__dt_lt_a:lifetime, $__dt_ty_K:ty, $__dt_ct_N:expr` — the matcher tail
    /// every arm carries. Empty for a non-generic trait, so those expansions are
    /// unchanged.
    ///
    /// A const parameter travels as an `expr`: there is no `const` fragment, and an
    /// expression is what both of its uses — an array length and a const argument —
    /// accept once braced.
    fn matcher_tail(&self) -> TokenStream {
        self.tail(|p| {
            let m = &p.meta;
            match p.kind {
                ParamKind::Lifetime => quote! { $#m:lifetime },
                ParamKind::Type => quote! { $#m:ty },
                ParamKind::Const => quote! { $#m:expr },
            }
        })
    }

    /// `, $__dt_lt_a, $__dt_ty_K, $__dt_ct_N` — passing them on to a nested arm.
    fn forward_tail(&self) -> TokenStream {
        self.tail(|p| {
            let m = &p.meta;
            quote! { $#m }
        })
    }

    fn tail(&self, one: impl Fn(&Param) -> TokenStream) -> TokenStream {
        if self.is_empty() {
            return TokenStream::new();
        }
        let items = self.0.iter().map(one);
        quote! { , #(#items),* }
    }

    /// Extra top-level arms for an impl that leaves defaulted arguments out —
    /// `impl Store for W` where the trait is `Store<K = u32>`. Rust requires
    /// defaults to be trailing, so each arm drops one more from the end and
    /// forwards to the full form with the trait's own defaults supplied.
    fn defaulted_arms(&self) -> TokenStream {
        let defaults = self
            .0
            .iter()
            .rev()
            .take_while(|p| p.default.is_some())
            .count();

        let arms = (1..=defaults).map(|dropped| {
            let kept = self.0.len() - dropped;
            let decls = self.0[..kept].iter().map(|p| {
                let m = &p.meta;
                match p.kind {
                    ParamKind::Lifetime => quote! { $#m:lifetime },
                    ParamKind::Type => quote! { $#m:ty },
                    ParamKind::Const => quote! { $#m:expr },
                }
            });
            let matcher = if kept == 0 {
                TokenStream::new()
            } else {
                quote! { , #(#decls),* }
            };

            let args = self.0.iter().enumerate().map(|(i, p)| {
                if i < kept {
                    let m = &p.meta;
                    quote! { $#m }
                } else {
                    p.default.clone().expect("trailing params have defaults")
                }
            });

            quote! {
                ($self:path, $field:ident, [$($skip:ident),*], $trait_path:path #matcher) => {
                    $self!($self, $field, [$($skip),*], $trait_path, #(#args),*);
                };
            }
        });
        quote! { #(#arms)* }
    }

    /// Rewrite a signature so the trait's parameters read as metavariables.
    ///
    /// Every mention of a parameter is the trait's own: a method cannot redeclare
    /// one (`fn get<K>` inside `trait Store<K>` is E0403, and the lifetime form is
    /// E0496), so there is no shadowing to work around.
    fn rewrite(&self, tokens: TokenStream) -> TokenStream {
        let find = |name: &str, want_lifetime: bool| {
            self.0
                .iter()
                .find(|p| p.name == name && matches!(p.kind, ParamKind::Lifetime) == want_lifetime)
        };

        let mut out = TokenStream::new();
        let mut trees = tokens.into_iter().peekable();
        while let Some(tree) = trees.next() {
            match tree {
                // A lifetime is two tokens: `'` joined to its identifier.
                TokenTree::Punct(ref p) if p.as_char() == '\'' => {
                    let found = match trees.peek() {
                        Some(TokenTree::Ident(id)) => find(&id.to_string(), true),
                        _ => None,
                    };
                    match found {
                        Some(param) => {
                            let m = &param.meta;
                            trees.next();
                            out.extend(quote! { $#m });
                        }
                        None => out.extend([tree]),
                    }
                }
                TokenTree::Ident(ref id) => match find(&id.to_string(), false) {
                    Some(param) => {
                        let m = &param.meta;
                        match param.kind {
                            // A const argument has to be braced to accept an
                            // expression, and braces are allowed wherever one can
                            // appear: `[u8; { $n }]` and `Holder<{ $n }>`.
                            ParamKind::Const => out.extend(quote! { { $#m } }),
                            _ => out.extend(quote! { $#m }),
                        }
                    }
                    None => out.extend([tree]),
                },
                TokenTree::Group(g) => {
                    let inner = self.rewrite(g.stream());
                    out.extend([TokenTree::Group(proc_macro2::Group::new(
                        g.delimiter(),
                        inner,
                    ))]);
                }
                other => out.extend([other]),
            }
        }
        out
    }
}

fn helper_macro_name(trait_name: &Ident) -> Ident {
    format_ident!("__delegate_impl_{}", trait_name)
}

/// The alias that gives the helper a *path* beside the trait, under a second name so
/// that it does not redefine the exported one. This is what lets an impl address the
/// helper the same way it addresses the trait, in this crate or in a dependent one.
fn path_alias_name(trait_name: &Ident) -> Ident {
    format_ident!("__delegate_path_{}", trait_name)
}

/// One method body, forwarding to `self.$field`.
///
/// `$field` and `$trait_path` are left as `macro_rules!` metavariables: this token
/// stream is emitted *inside* the helper macro, which is where they are bound.
fn delegating_method(sig: &syn::Signature, params: &TraitParams) -> TokenStream {
    let method_name = &sig.ident;
    // The trait's parameters have no binding at the impl site, so the recorded
    // signature refers to them through metavariables the invocation fills in.
    let sig_tokens = params.rewrite(sig.to_token_stream());

    let args: Vec<_> = sig
        .inputs
        .iter()
        .filter_map(|arg| match arg {
            syn::FnArg::Typed(pt) => Some(&pt.pat),
            syn::FnArg::Receiver(_) => None,
        })
        .collect();

    let await_tok = if sig.asyncness.is_some() {
        quote! { .await }
    } else {
        quote! {}
    };

    // The receiver of the outer method decides how the field is passed on:
    // `&self` -> `&self.field`, `&mut self` -> `&mut self.field`, `self` -> a move.
    let reference = sig.receiver().and_then(|r| match &r.kind {
        syn::ReceiverKind::Reference(_, _, mutability) => Some(mutability.is_some()),
        _ => None,
    });
    let (has_ref, has_mut) = (reference.is_some(), reference == Some(true));
    let target_expr = if has_ref && has_mut {
        quote! { &mut self.$field }
    } else if has_ref {
        quote! { &self.$field }
    } else {
        quote! { self.$field }
    };

    // `<_ as Trait>::method` rather than `self.field.method`: it resolves to the
    // trait's method even when an inherent method of the same name exists.
    quote! {
        #[inline]
        #sig_tokens {
            <_ as $trait_path>::#method_name(#target_expr, #(#args),*) #await_tok
        }
    }
}
