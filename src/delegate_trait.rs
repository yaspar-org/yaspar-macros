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
//!     fn get(&self, k: K);         ($self:path, [$($field:tt)*], .., $__dt_ty_K:ty) => {
//!                                      fn get(&self, k: $__dt_ty_K) { .. } } }
//!
//! impl Store<u32> for W     ->  __delegate_impl_Store!(
//!                                   __delegate_impl_Store, [inner], [], Store<u32>, u32);
//! ```
//!
//! The field travels as a bracketed *token list* rather than an `ident`, because it is not always
//! one: a newtype's field is `0`. A `tt` list splices into `self.$($field)*` for a name, an index
//! and a dotted path alike.
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
//! It does have to tell a mention from a coincidence, which is why the type and const
//! substitutions walk the parsed signature rather than its tokens: an associated-type *binding*
//! puts a bare identifier where a type argument would go, so a token walk turns
//! `Iterator<Item = u8>` into `Iterator<$__dt_ty_Item = u8>` for any trait with a parameter called
//! `Item`. Lifetimes stay a token walk, since a `syn::Lifetime` has nowhere to put a `$name`.
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
//! # What travels with a signature
//!
//! The method's attributes do, all of them: an attribute macro runs *before* `cfg` stripping, so a
//! gated method is recorded here like any other, and dropping its `#[cfg]` would emit it into every
//! impl unconditionally — where the trait, which *was* stripped, no longer has it. Note that a
//! `#[cfg]` on a trait method from another crate is then evaluated against the **consumer's**
//! features, since that is where the expansion happens.
//!
//! # Limitations
//!
//! Only required *methods* are delegated. A required associated type or associated
//! const is not, so a trait that has one must have it supplied by the impl block as
//! usual. Methods with a default body are left to their default.
//!
//! Forwarding needs a `self`, `&self` or `&mut self` receiver: without one there is no `self` to
//! read the field out of, and a typed receiver (`self: Box<Self>`) is a type the field does not
//! have. Either one is
//! rejected by name, and writing the method in the impl block is the way through — the
//! skip list honours it there like any other override.
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
use quote::{ToTokens, format_ident, quote, quote_spanned};
use syn::parse::{Parse, ParseStream};
use syn::punctuated::Punctuated;
use syn::spanned::Spanned;
use syn::visit_mut::VisitMut;
use syn::{
    Attribute, Expr, FnArg, GenericParam, Ident, ItemImpl, ItemTrait, Member, PathArguments,
    ReceiverKind, Safety, Signature, Token, TraitItem, Type, parse_quote,
};

/// `target = <field>` — the field every missing method is forwarded to.
///
/// A dotted list of [`Member`]s rather than a bare name, so that a tuple index (`target = 0`) and a
/// nested field (`target = inner.deep`) work too. A field *path*, not an expression: the tokens are
/// spliced in after `self.` in the helper macro, where nothing else would mean anything.
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
        // `self` is a keyword, so it would never reach the `Member` parse below —
        // and it is the mistake the documentation specifically warns about.
        if input.peek(Token![self]) {
            return Err(input.error(
                "`target` is a field name, not an expression: write `target = inner`, \
                 not `target = self.inner`",
            ));
        }
        let target = Punctuated::parse_separated_nonempty(input)?;
        // Anything left over is a mistake, and saying so beats the bare "unexpected
        // token" that the caller would otherwise get from the attribute parser.
        if !input.is_empty() {
            return Err(input
                .error("`#[delegate_trait]` takes only `target = <field>`, and nothing after it"));
        }
        Ok(DelegateTraitArgs { target })
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
    //
    // The *attributes* travel with the signature. A `#[cfg]`-gated method is recorded
    // here — an attribute macro runs before `cfg` stripping — so dropping the attribute
    // would emit the method into every impl unconditionally, where the trait no longer
    // has it: `E0407 method never is not a member of trait`.
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
        // A defaulted parameter may be left out, so what the trait accepts is a *range*: saying
        // "takes 2" of a `trait Pair<A, B = u8>` sends the reader after an argument they may omit.
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
    /// `trait Store<K = u32>`. Only the trait knows this, so only the trait can
    /// fill it in for an impl that leaves the argument out.
    default: Option<ParamDefault>,
}

impl Param {
    /// The tokens this parameter is replaced by in a recorded signature.
    ///
    /// A const parameter travels as an `expr` fragment, so every use is *braced* — `[u8; { $n }]`,
    /// `Holder<{ $n }>` — which is what lets an expression stand where a const argument goes.
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

/// `trait Store<K = Vec<u8>>` / `trait Buf<const N: usize = 4>`. Kept as a syntax node rather than
/// tokens because it needs the same substitution a signature does: `trait Pair<A, B = Vec<A>>` has
/// to emit `Vec<$__dt_ty_A>`.
enum ParamDefault {
    Type(Type),
    Const(Expr),
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

    /// How many trailing parameters an impl may leave out. Rust requires defaults to
    /// be trailing, so this is a count from the end.
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
                    // A default may name an earlier parameter — `trait Pair<A, B = Vec<A>>` —
                    // where `A` binds nothing at the impl site, so it needs the same rewrite a
                    // signature gets, into the metavariables this arm has just bound.
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

    /// Rewrite a signature so the trait's parameters read as metavariables.
    ///
    /// Every mention of a parameter is the trait's own: a method cannot redeclare
    /// one (`fn get<K>` inside `trait Store<K>` is E0403, and the lifetime form is
    /// E0496), so there is no shadowing to work around.
    fn rewrite_signature(&self, sig: &Signature) -> TokenStream {
        let mut sig = sig.clone();
        Substitute(self).visit_signature_mut(&mut sig);
        self.rewrite_lifetimes(sig.to_token_stream())
    }

    /// The same rewrite, for a defaulted parameter's default.
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

    /// Replace every mention of one of the trait's *lifetime* parameters with the
    /// metavariable that stands for it.
    ///
    /// A token walk, where the type and const substitutions are not: a `syn::Lifetime` is an
    /// identifier behind a tick and cannot hold a `$name`, so
    /// there is nothing to put in its place at the syntax level. It is also the one
    /// case where a token walk is safe, because a tick has exactly one meaning — a
    /// lifetime — and nothing else in a signature is spelled that way.
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
                // A lifetime is two tokens: `'` joined to its identifier.
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

/// Substitutes the trait's type and const parameters, in *type* and *expression*
/// positions only.
///
/// A token walk cannot do this: replacing every identifier that reads like a parameter name also
/// hits an associated-type *binding*, whose name sits exactly where a type argument would. A
/// `trait Feed<Item>` then turns `Box<dyn Iterator<Item = u8>>` into `Iterator<$__dt_ty_Item = u8>`,
/// an `E0220` blamed on the trait — and `Item`, `Output`, `Error` and `Key` are ordinary parameter
/// names. Walking the parsed signature makes the distinction structural, since a binding name is an
/// `Ident` field of `syn::AssocType` and never a [`Type`]. The gap left is a macro invocation, whose
/// body syn keeps as opaque tokens.
struct Substitute<'a>(&'a TraitParams);

impl VisitMut for Substitute<'_> {
    fn visit_type_mut(&mut self, ty: &mut Type) {
        // Children first: the arguments of `K::Assoc<L>` are rewritten before `K` is,
        // and after the replacement there is nothing left to descend into.
        syn::visit_mut::visit_type_mut(self, ty);

        // A single unqualified segment is all a type or const parameter can be written as. Longer
        // — `K::Assoc`, shorthand for `<K as Bound>::Assoc` — is left alone on purpose: the bound
        // does not survive the trip to the impl site, so substituting would trade `E0412 cannot find
        // type K` for an `E0223 ambiguous associated type` about `<u8>::Assoc`.
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

        // Only a const parameter can be named by an expression: an array length `[u8; N]` or a
        // const argument `Holder<N>`. A type parameter's name in expression position is something
        // else and must be left alone.
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
///
/// The trait method's own attributes come first, ahead of the `#[inline]` — all of them, not a
/// `cfg`-only subset, since they are valid on an impl method too and a filter would need an entry
/// per attribute anyone ever wants. A `#[cfg]` on a *cross-crate* trait method therefore evaluates
/// against the **consumer's** features, so a method the trait's crate compiled in can be absent from
/// the wrapper. Gate on a feature the trait's crate re-exports if that matters.
fn delegating_method(attrs: &[Attribute], sig: &Signature, params: &TraitParams) -> TokenStream {
    let method_name = &sig.ident;

    // Without a receiver there is no `self` to read the field out of. Left alone, the emitted
    // `<_ as Trait>::version(self.inner)` comes out as `E0424 expected value, found module self`
    // against the trait's attribute, suggesting `fn version&self()`. A `compile_error!` rather than
    // dropping the method from the recorded set, which would leave only `E0046` — true, but reading
    // as though the delegation were broken. The answer is to write the method in the impl block,
    // which the skip list honours, so this arm is only reached when it has not been.
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
        // Spanned at the trait's declaration, the line that has to change; the impl block that
        // asked for it shows up as the macro backtrace.
        return quote_spanned! { sig.ident.span() => ::core::compile_error!(#msg); };
    }

    // Argument *patterns* are not expressions, so they cannot be replayed as the call's arguments:
    // `fn b(&self, _: u32)` is an ordinary trait method, and `_` in an argument is "in expressions,
    // `_` can only be used on the left-hand side of an assignment". So each is renamed to a fresh
    // binding. `_` is the only pattern a bodyless fn can carry today — `mut n: u32` is rustc's own
    // future-incompatibility warning, anything richer is `E0642` — and the rename covers the rest
    // if that ever loosens.
    let mut sig = sig.clone();
    let mut args: Vec<Ident> = Vec::new();
    for arg in sig.inputs.iter_mut() {
        if let FnArg::Typed(pt) = arg {
            let name = format_ident!("__dt_arg{}", args.len());
            *pt.pat = parse_quote! { #name };
            args.push(name);
        }
    }

    // The trait's parameters have no binding at the impl site, so the recorded
    // signature refers to them through metavariables the invocation fills in.
    let sig_tokens = params.rewrite_signature(&sig);

    let await_tok = if sig.asyncness.is_some() {
        quote! { .await }
    } else {
        quote! {}
    };

    // The receiver of the outer method decides how the field is passed on:
    // `&self` -> `&self.field`, `&mut self` -> `&mut self.field`, `self` -> a move.
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

    // `<_ as Trait>::method` rather than `self.field.method`: it resolves to the
    // trait's method even when an inherent method of the same name exists.
    let call = quote! {
        <_ as $trait_path>::#method_name(#target_expr #(, #args)*) #await_tok
    };
    // An `unsafe fn` body is not itself an unsafe block, so without this the forwarder warns under
    // `unsafe_op_in_unsafe_fn` — unsilenceably, the span being the generated macro's — which is a
    // hard error under `#![deny(warnings)]`.
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
