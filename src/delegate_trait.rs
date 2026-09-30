// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! `#[delegatable_trait]` and `#[delegate_trait]`: forward unwritten trait methods to a field.
//!
//! An impl attribute cannot see the trait, so `#[delegatable_trait]` records the required method
//! signatures in a hidden `macro_rules! __delegate_impl_<Trait>`, and `#[delegate_trait]` invokes it
//! with the field, the names to skip (methods the impl wrote), the trait path, and the trait's
//! generic arguments:
//!
//! ```text
//! __delegate_impl_Store!(__delegate_impl_Store, [inner], [], Store<u32>, u32);
//! ```
//!
//! - Each arm passes the macro's own path on instead of recursing by bare name, so the macro works
//!   when reached by path.
//! - The skip list is subtracted inside the macro (`@maybe` arms), the only place that knows both the
//!   signatures and the names.
//! - Trait generic parameters become metavariables (`$__dt_ty_K`, `$__dt_lt_a`, `$__dt_ct_N`),
//!   matched positionally in declaration order. Const uses are braced (`{ $n }`). Omitted defaulted
//!   parameters are filled in by extra arms.
//! - A method's attributes, `#[cfg]` included, are copied to the forwarder. For a trait from another
//!   crate, `#[cfg]` is then evaluated against the consumer's features.
//! - Only required methods are delegated: not default methods, associated types, or consts. Methods
//!   without a `self`/`&self`/`&mut self` receiver produce a `compile_error!`.
//!
//! # Finding the helper
//!
//! Beside the trait, `pub use __delegate_impl_Store as __delegate_path_Store;` gives the helper a
//! path, so `impl libx::a::Store for W` invokes `libx::a::__delegate_path_Store!` with no import. A
//! bare trait name falls back to the crate-root `__delegate_impl_Store`. The alias must be a
//! relative `use`: macro-expanded `#[macro_export]` macros cannot be referred to by absolute path.
//!
//! By default the helper is `#[macro_export]`ed, so two same-named traits in one crate collide
//! (`E0428`). `local` skips the export and makes the alias `pub(crate)`, at the cost of not being
//! usable from other crates.

use proc_macro2::{Span, TokenStream, TokenTree};
use quote::{ToTokens, format_ident, quote, quote_spanned};
use syn::parse::{Parse, ParseStream};
use syn::punctuated::Punctuated;
use syn::spanned::Spanned;
use syn::visit_mut::VisitMut;
use syn::{
    Attribute, Expr, FnArg, GenericParam, Ident, ItemImpl, ItemTrait, Member, PathArguments,
    ReceiverKind, Safety, Signature, Token, TraitItem, Type, parse_quote,
};

/// `target = <field>`: a dotted list of [`Member`]s (`inner`, `0`, `inner.deep`), spliced after `self.`.
struct DelegateTraitArgs {
    target: Punctuated<Member, Token![.]>,
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
        // Catch the common `target = self.inner` mistake with a clear message.
        if input.peek(Token![self]) {
            return Err(input.error(
                "`target` is a field name, not an expression: write `target = inner`, \
                 not `target = self.inner`",
            ));
        }
        let target = Punctuated::parse_separated_nonempty(input)?;
        if !input.is_empty() {
            return Err(input
                .error("`#[delegate_trait]` takes only `target = <field>`, and nothing after it"));
        }
        Ok(DelegateTraitArgs { target })
    }
}

/// `#[delegatable_trait]`: the trait unchanged, plus its helper macro and path alias.
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

    // Only required methods. Attributes are kept: `cfg` is not yet stripped here, so dropping a
    // `#[cfg]` would emit a method the trait no longer has (`E0407`).
    let method_sigs: Vec<(&Vec<Attribute>, &Signature)> = trait_def
        .items
        .iter()
        .filter_map(|item| match item {
            TraitItem::Fn(method) if method.default.is_none() => Some((&method.attrs, &method.sig)),
            _ => None,
        })
        .collect();

    let method_names: Vec<Ident> = method_sigs
        .iter()
        .map(|(_, sig)| sig.ident.clone())
        .collect();
    let method_bodies: Vec<TokenStream> = method_sigs
        .iter()
        .map(|(attrs, sig)| delegating_method(attrs, sig, &params))
        .collect();

    // Both are empty for a non-generic trait.
    let matcher = params.matcher_tail();
    let forward = params.forward_tail();

    // Arms for omitted defaulted arguments, and a fallback arm reporting a wrong argument count.
    let defaulted_arms = params.defaulted_arms();
    let arity_guard = if params.is_empty() {
        TokenStream::new()
    } else {
        // With defaults, the accepted count is a range.
        let most = params.0.len();
        let fewest = most - params.defaults();
        let expected = if fewest == most {
            format!("{most}")
        } else {
            format!("{fewest} to {most}")
        };
        let msg = format!(
            "`#[delegate_trait]` for `{trait_name}`: the impl block passes the wrong number of \
             generic arguments; `{trait_name}` takes {expected}",
        );
        quote! {
            ($($__dt_unmatched:tt)*) => { ::core::compile_error!(#msg); };
        }
    };

    // The alias gives the helper a path beside the trait. `local` skips `#[macro_export]`, so
    // same-named traits don't collide at the crate root, but the helper can't leave the crate.
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
            // `pub`, so dependent crates can follow the path.
            quote! {
                #[doc(hidden)]
                pub use #helper_macro_name as #path_alias;
            },
        )
    };

    // Arms recurse through `$self` (the macro's path), not its bare name.
    Ok(quote! {
        #trait_def

        #[doc(hidden)]
        #export
        macro_rules! #helper_macro_name {
            ($self:path, [$($field:tt)*], [$($skip:ident),*], $trait_path:path #matcher) => {
                #(
                    $self!(
                        @maybe $self, #method_names, [$($field)*], [$($skip),*], $trait_path #forward
                    );
                )*
            };

            // Skip: method name matches the head of the skip list.
            #(
                (@maybe $self:path, #method_names, [$($field:tt)*], [#method_names $(, $($rest:ident),*)?], $trait_path:path #matcher) => {};
            )*

            // No match on the head: pop it and recurse.
            (@maybe $self:path, $method:ident, [$($field:tt)*], [$first:ident $(, $($rest:ident),*)?], $trait_path:path #matcher) => {
                $self!(@maybe $self, $method, [$($field)*], [$($($rest),*)?], $trait_path #forward);
            };

            // Skip list exhausted: the user did not write this method, so delegate it.
            #(
                (@maybe $self:path, #method_names, [$($field:tt)*], [], $trait_path:path #matcher) => {
                    #method_bodies
                };
            )*

            #defaulted_arms
            #arity_guard
        }
        #alias
    })
}

/// `#[delegate_trait]`: the impl block plus a helper invocation supplying the missing methods.
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

    // Methods the user wrote; the helper skips them.
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

    // The trait's generic arguments, passed positionally.
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

    // `libx::a::Store` -> `libx::a::__delegate_path_Store`; a bare name falls back to the
    // crate-root helper, which only resolves in the defining crate.
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
            #helper_path!(#helper_path, [#target], [#(#override_idents),*], #trait_path #args_tail);
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
    /// `trait Store<K = u32>`: filled in when the impl omits the argument.
    default: Option<ParamDefault>,
}

impl Param {
    /// The tokens replacing this parameter in a signature. Consts are braced (`{ $n }`) so the
    /// `expr` fragment is accepted as an array length or const argument.
    fn substitution(&self) -> TokenStream {
        let m = &self.meta;
        match self.kind {
            ParamKind::Const => quote! { { $#m } },
            _ => quote! { $#m },
        }
    }
}

enum ParamKind {
    Lifetime,
    Type,
    Const,
}

/// A parameter default, kept as syntax so it gets the same substitution as a signature
/// (`trait Pair<A, B = Vec<A>>` emits `Vec<$__dt_ty_A>`).
enum ParamDefault {
    Type(Type),
    Const(Expr),
}

/// The trait's generic parameters in declaration order (types and consts may interleave), matching
/// the impl's positional arguments.
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
                        default: t
                            .default
                            .as_ref()
                            .map(|(_, ty)| ParamDefault::Type(ty.clone())),
                    },
                    GenericParam::Const(c) => Param {
                        kind: ParamKind::Const,
                        name: c.ident.to_string(),
                        meta: format_ident!("__dt_ct_{}", c.ident),
                        default: c
                            .default
                            .as_ref()
                            .map(|(_, expr)| ParamDefault::Const(expr.clone())),
                    },
                })
                .collect(),
        )
    }

    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// How many trailing parameters have defaults.
    fn defaults(&self) -> usize {
        self.0
            .iter()
            .rev()
            .take_while(|p| p.default.is_some())
            .count()
    }

    /// The type or const parameter a bare, unqualified name refers to, if any.
    fn named(&self, ident: &Ident) -> Option<&Param> {
        let name = ident.to_string();
        self.0
            .iter()
            .find(|p| p.name == name && !matches!(p.kind, ParamKind::Lifetime))
    }

    /// `, $__dt_lt_a:lifetime, $__dt_ty_K:ty, $__dt_ct_N:expr`, or empty for a non-generic trait.
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

    /// `, $__dt_lt_a, $__dt_ty_K, $__dt_ct_N`, forwarded to a nested arm.
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

    /// One arm per number of omitted trailing defaults, forwarding to the full form with the
    /// defaults filled in.
    fn defaulted_arms(&self) -> TokenStream {
        let arms = (1..=self.defaults()).map(|dropped| {
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
                    // A default may name earlier parameters, so rewrite it too.
                    self.rewrite_default(p.default.as_ref().expect("trailing params have defaults"))
                }
            });

            quote! {
                ($self:path, [$($field:tt)*], [$($skip:ident),*], $trait_path:path #matcher) => {
                    $self!($self, [$($field)*], [$($skip),*], $trait_path, #(#args),*);
                };
            }
        });
        quote! { #(#arms)* }
    }

    /// Replace the trait's parameters in a signature with metavariables. Methods cannot shadow
    /// them (E0403 / E0496), so every mention is the trait's.
    fn rewrite_signature(&self, sig: &Signature) -> TokenStream {
        let mut sig = sig.clone();
        Substitute(self).visit_signature_mut(&mut sig);
        self.rewrite_lifetimes(sig.to_token_stream())
    }

    /// [`Self::rewrite_signature`] for a parameter default.
    fn rewrite_default(&self, default: &ParamDefault) -> TokenStream {
        match default {
            ParamDefault::Type(ty) => {
                let mut ty = ty.clone();
                Substitute(self).visit_type_mut(&mut ty);
                self.rewrite_lifetimes(ty.to_token_stream())
            }
            ParamDefault::Const(expr) => {
                let mut expr = expr.clone();
                Substitute(self).visit_expr_mut(&mut expr);
                self.rewrite_lifetimes(expr.to_token_stream())
            }
        }
    }

    /// Replace the trait's lifetime parameters with metavariables. A token walk, since a
    /// `syn::Lifetime` cannot hold `$name`; safe because `'` only ever starts a lifetime here.
    fn rewrite_lifetimes(&self, tokens: TokenStream) -> TokenStream {
        let find = |name: &str| {
            self.0
                .iter()
                .find(|p| p.name == name && matches!(p.kind, ParamKind::Lifetime))
        };

        let mut out = TokenStream::new();
        let mut trees = tokens.into_iter().peekable();
        while let Some(tree) = trees.next() {
            match tree {
                TokenTree::Punct(ref p) if p.as_char() == '\'' => {
                    let found = match trees.peek() {
                        Some(TokenTree::Ident(id)) => find(&id.to_string()),
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
                TokenTree::Group(g) => {
                    let inner = self.rewrite_lifetimes(g.stream());
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

/// Substitutes type and const parameters in type and expression positions. A syntax walk, not a
/// token walk, so an associated-type binding (`Iterator<Item = u8>` in `trait Feed<Item>`) is not
/// rewritten. Macro invocations in a signature are not substituted.
struct Substitute<'a>(&'a TraitParams);

impl VisitMut for Substitute<'_> {
    fn visit_type_mut(&mut self, ty: &mut Type) {
        syn::visit_mut::visit_type_mut(self, ty);

        // Only a bare single segment. `K::Assoc` is left alone: without the bound, `<u8>::Assoc`
        // would be ambiguous (E0223).
        let Type::Path(path) = ty else { return };
        if path.qself.is_some() || path.path.leading_colon.is_some() {
            return;
        }
        let Some(only) = path.path.segments.first() else {
            return;
        };
        if path.path.segments.len() != 1 || !only.arguments.is_none() {
            return;
        }
        if let Some(param) = self.0.named(&only.ident) {
            *ty = Type::Verbatim(param.substitution());
        }
    }

    fn visit_expr_mut(&mut self, expr: &mut Expr) {
        syn::visit_mut::visit_expr_mut(self, expr);

        // In expression position only const parameters are substituted.
        let Expr::Path(path) = expr else { return };
        if path.qself.is_some() || path.path.leading_colon.is_some() {
            return;
        }
        let Some(only) = path.path.segments.first() else {
            return;
        };
        if path.path.segments.len() != 1 || !only.arguments.is_none() {
            return;
        }
        match self.0.named(&only.ident) {
            Some(param) if matches!(param.kind, ParamKind::Const) => {
                *expr = Expr::Verbatim(param.substitution());
            }
            _ => {}
        }
    }
}

fn helper_macro_name(trait_name: &Ident) -> Ident {
    format_ident!("__delegate_impl_{}", trait_name)
}

/// The helper's alias beside the trait, so an impl can reach it by the trait's path.
fn path_alias_name(trait_name: &Ident) -> Ident {
    format_ident!("__delegate_path_{}", trait_name)
}

/// A method forwarding to `self.$field`, with the trait method's attributes. `$field` and
/// `$trait_path` are metavariables bound by the helper macro this is emitted into.
fn delegating_method(attrs: &[Attribute], sig: &Signature, params: &TraitParams) -> TokenStream {
    let method_name = &sig.ident;

    // No receiver, or a typed one: nothing to forward to. Reached only if the impl didn't
    // write the method itself.
    let unforwardable = match sig.receiver() {
        None => Some(format!(
            "`#[delegate_trait]`: `{method_name}` has no `self` receiver, so there is no field to \
             forward it to; write `{method_name}` in the impl block"
        )),
        Some(r) if matches!(r.kind, ReceiverKind::Typed(_, _)) => Some(format!(
            "`#[delegate_trait]`: `{method_name}` writes its receiver as a type, as in \
             `self: Box<Self>`, and a field is not of that type; write `{method_name}` in the \
             impl block"
        )),
        Some(_) => None,
    };
    if let Some(msg) = unforwardable {
        // Spanned at the trait's method declaration.
        return quote_spanned! { sig.ident.span() => ::core::compile_error!(#msg); };
    }

    // Rename argument patterns (e.g. `_`) to fresh bindings so they can be passed on.
    let mut sig = sig.clone();
    let mut args: Vec<Ident> = Vec::new();
    for arg in sig.inputs.iter_mut() {
        if let FnArg::Typed(pt) = arg {
            let name = format_ident!("__dt_arg{}", args.len());
            *pt.pat = parse_quote! { #name };
            args.push(name);
        }
    }

    let sig_tokens = params.rewrite_signature(&sig);

    let await_tok = if sig.asyncness.is_some() {
        quote! { .await }
    } else {
        quote! {}
    };

    // `&self` -> `&self.field`, `&mut self` -> `&mut self.field`, `self` -> move.
    let reference = sig.receiver().and_then(|r| match &r.kind {
        ReceiverKind::Reference(_, _, mutability) => Some(mutability.is_some()),
        _ => None,
    });
    let (has_ref, has_mut) = (reference.is_some(), reference == Some(true));
    let target_expr = if has_ref && has_mut {
        quote! { &mut self.$($field)* }
    } else if has_ref {
        quote! { &self.$($field)* }
    } else {
        quote! { self.$($field)* }
    };

    // Fully qualified, so an inherent method of the same name is not picked.
    let call = quote! {
        <_ as $trait_path>::#method_name(#target_expr #(, #args)*) #await_tok
    };
    // Avoids `unsafe_op_in_unsafe_fn` in the forwarder of an `unsafe fn`.
    let body = if matches!(sig.safety, Safety::Unsafe(_)) {
        quote! { unsafe { #call } }
    } else {
        call
    };

    quote! {
        #(#attrs)*
        #[inline]
        #sig_tokens { #body }
    }
}
