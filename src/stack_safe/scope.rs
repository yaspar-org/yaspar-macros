// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Finds every function in scope (the roots plus functions nested in their bodies, at any
//! depth) and which of them recurse.
//!
//! A nested function is addressed by its root and a path of *ordinals*: its position among the
//! functions its host body declares, counting ones inside nested blocks. [`take`] leaves a
//! placeholder so later ordinals stay valid.
//!
//! A nested `fn` counts as in scope throughout its host body, not just its block.

use proc_macro2::{Ident, Span, TokenStream, TokenTree};
use syn::spanned::Spanned;
use syn::visit::Visit;
use syn::visit_mut::VisitMut;
use syn::{Block, Expr, Item, ItemFn, Stmt};

use super::Opts;

/// A function in scope: one of the roots, or one declared in the body of a root at any depth.
pub(super) struct Def {
    /// Which root it belongs to.
    pub(super) owner: usize,
    /// Ordinals from the root's body down; empty for the root.
    pub(super) path: Vec<usize>,
    pub(super) name: Ident,
    /// Its own marker's options if it has one (replacing, not merging), else its host's.
    pub(super) opts: Opts,
    /// Whether it carried its own `#[stack_safe]` marker.
    pub(super) marked: bool,
}

/// Every function in scope, shallowest first, so a host precedes what it declares.
///
/// Also strips each `#[stack_safe]` marker and resolves the options in force.
pub(super) fn collect(roots: &mut [ItemFn], opts: Opts) -> syn::Result<Vec<Def>> {
    let mut defs: Vec<Def> = Vec::with_capacity(roots.len());
    for (owner, root) in roots.iter_mut().enumerate() {
        let own = Opts::take_from(&mut root.attrs)?;
        defs.push(Def {
            owner,
            path: Vec::new(),
            name: root.sig.ident.clone(),
            opts: own.unwrap_or(opts),
            marked: own.is_some(),
        });
    }
    let mut next = 0;
    while next < defs.len() {
        let (owner, opts) = (defs[next].owner, defs[next].opts);
        // Cloned so `defs` can grow below.
        let path = defs[next].path.clone();
        let mut found: Vec<(usize, Ident, Option<Opts>)> = Vec::new();
        let mut failed: Option<syn::Error> = None;
        with_def_mut(
            &mut roots[owner],
            &path,
            &mut |host| match nested_mut(host) {
                Ok(nested) => found = nested,
                Err(e) => failed = Some(e),
            },
        );
        if let Some(e) = failed {
            return Err(e);
        }
        for (ordinal, name, own) in found {
            let mut path = path.clone();
            path.push(ordinal);
            defs.push(Def {
                owner,
                name,
                path,
                opts: own.unwrap_or(opts),
                marked: own.is_some(),
            });
        }
        next += 1;
    }
    Ok(defs)
}

/// The functions declared anywhere in this body (not inside them), with ordinal, name, and
/// own options (stripped from the attributes).
fn nested_mut(func: &mut ItemFn) -> syn::Result<Vec<(usize, Ident, Option<Opts>)>> {
    struct V {
        found: Vec<(usize, Ident, Option<Opts>)>,
        next: usize,
        failed: Option<syn::Error>,
    }

    impl VisitMut for V {
        fn visit_item_mut(&mut self, item: &mut Item) {
            if !addressable(item) {
                // Other items (e.g. `mod`, `impl`) are separate scopes; skip them.
                return;
            }
            let ordinal = self.next;
            self.next += 1;
            let Item::Fn(func) = item else { return };
            match Opts::take_from(&mut func.attrs) {
                // Keep the first error.
                Err(e) => self.failed = self.failed.take().or(Some(e)),
                Ok(own) => self.found.push((ordinal, func.sig.ident.clone(), own)),
            }
        }
    }

    let mut v = V {
        found: Vec::new(),
        next: 0,
        failed: None,
    };
    v.visit_block_mut(&mut func.block);
    match v.failed {
        Some(e) => Err(e),
        None => Ok(v.found),
    }
}

/// Whether this item takes an ordinal: a `fn`, or the placeholder [`take`] leaves.
fn addressable(item: &Item) -> bool {
    matches!(item, Item::Fn(_) | Item::Verbatim(_))
}

/// Follow a path down to the definition it addresses.
pub(super) fn at<'a>(func: &'a ItemFn, path: &[usize]) -> &'a ItemFn {
    let mut func = func;
    for &ordinal in path {
        func = nth(&func.block, ordinal).expect("a path addresses a nested function");
    }
    func
}

/// The function at `ordinal` in this body.
fn nth(block: &Block, ordinal: usize) -> Option<&ItemFn> {
    struct V<'a> {
        want: usize,
        next: usize,
        found: Option<&'a ItemFn>,
    }

    impl<'ast> Visit<'ast> for V<'ast> {
        fn visit_item(&mut self, item: &'ast Item) {
            if self.found.is_some() || !addressable(item) {
                return;
            }
            let ordinal = self.next;
            self.next += 1;
            if ordinal == self.want
                && let Item::Fn(func) = item
            {
                self.found = Some(func);
            }
        }
    }

    let mut v = V {
        want: ordinal,
        next: 0,
        found: None,
    };
    v.visit_block(block);
    v.found
}

/// Run `act` on the definition at `path` (the root if empty).
///
/// `&mut dyn`, not generic, to avoid unbounded monomorphization of the recursion.
fn with_def_mut(root: &mut ItemFn, path: &[usize], act: &mut dyn FnMut(&mut ItemFn)) {
    match path.is_empty() {
        true => act(root),
        false => with_item_mut(root, path, &mut |item| match item {
            Item::Fn(func) => act(func),
            _ => unreachable!("a path addresses a nested function"),
        }),
    }
}

/// Run `act` on the item at a non-empty `path`, so it can be replaced.
fn with_item_mut(root: &mut ItemFn, path: &[usize], act: &mut dyn FnMut(&mut Item)) {
    struct V<'a> {
        /// The ordinals still to follow; never empty.
        path: &'a [usize],
        next: usize,
        /// Taken when run, so it runs at most once.
        act: Option<&'a mut dyn FnMut(&mut Item)>,
    }

    impl VisitMut for V<'_> {
        fn visit_item_mut(&mut self, item: &mut Item) {
            if self.act.is_none() || !addressable(item) {
                return;
            }
            let ordinal = self.next;
            self.next += 1;
            let (&want, rest) = self.path.split_first().expect("a path has an ordinal");
            if ordinal != want {
                return;
            }
            let act = self.act.take().expect("checked above");
            if rest.is_empty() {
                act(item);
                return;
            }
            let Item::Fn(func) = item else {
                unreachable!("a path addresses a nested function")
            };
            V {
                path: rest,
                next: 0,
                act: Some(act),
            }
            .visit_block_mut(&mut func.block);
        }
    }

    debug_assert!(!path.is_empty(), "the annotated function stays put");
    V {
        path,
        next: 0,
        act: Some(act),
    }
    .visit_block_mut(&mut root.block);
}

/// Remove a nested definition, leaving a placeholder so other paths stay valid.
///
/// Take deeper definitions before the ones holding them.
pub(super) fn take(func: &mut ItemFn, path: &[usize]) -> ItemFn {
    let mut taken: Option<ItemFn> = None;
    with_item_mut(func, path, &mut |item| {
        taken = match std::mem::replace(item, placeholder(TokenStream::new())) {
            Item::Fn(inner) => Some(inner),
            _ => unreachable!("a path addresses a nested function"),
        };
    });
    taken.expect("a path addresses a nested function")
}

/// Replace the placeholder at `path` with `tokens` (the rewritten cycle).
pub(super) fn put_back(func: &mut ItemFn, path: &[usize], tokens: TokenStream) {
    with_item_mut(func, path, &mut |item| *item = placeholder(tokens.clone()));
}

fn placeholder(tokens: TokenStream) -> Item {
    Item::Verbatim(tokens)
}

/// The call graph: `edges[i][j]` is set when `i`'s body mentions `j` in a rewritable call or
/// inside a macro. Names resolve to the innermost definition in scope (see [`resolve`]).
///
/// `assoc` means the roots are impl items; `host` is the impl's type or the module's name.
///
/// Also returns calls through paths the rewriter cannot follow ([`Blocked`], reported by
/// [`scan`](super::scan)) and names that resolve ambiguously.
pub(super) fn edges(
    roots: &[ItemFn],
    defs: &[Def],
    assoc: bool,
    host: Option<&Ident>,
) -> (Vec<Vec<bool>>, Vec<Blocked>, Vec<Ident>) {
    let mut rows = Vec::with_capacity(defs.len());
    let mut blocked = Vec::new();
    let mut ambiguous = Vec::new();
    for (i, d) in defs.iter().enumerate() {
        let mut row = vec![false; defs.len()];
        for m in mentioned(at(&roots[d.owner], &d.path), assoc, host) {
            let j = match resolve(defs, i, &m, assoc) {
                Ok(Some(j)) => j,
                Ok(None) => continue,
                Err(name) => {
                    ambiguous.push(name);
                    continue;
                }
            };
            match m.unrewritable {
                None => row[j] = true,
                Some((path, span)) => blocked.push(Blocked {
                    caller: i,
                    callee: j,
                    path,
                    span,
                }),
            }
        }
        rows.push(row);
    }
    (rows, blocked, ambiguous)
}

/// A call that names a definition in scope through a path the transform cannot rewrite.
pub(super) struct Blocked {
    /// The definition the call is written in.
    pub(super) caller: usize,
    /// The definition it names.
    pub(super) callee: usize,
    /// The path as written, for the message.
    pub(super) path: String,
    pub(super) span: Span,
}

/// Which definition a mention inside `from` refers to, if any.
///
/// A candidate must be in scope (a root, or declared in a body enclosing `from`) and nameable as
/// written (see [`Written`]). The innermost wins; a tie (e.g. same name in sibling blocks) is
/// returned as `Err` rather than guessed.
fn resolve(defs: &[Def], from: usize, m: &Mention, assoc: bool) -> Result<Option<usize>, Ident> {
    let here = &defs[from];
    let nameable = |d: &Def| {
        let root = d.path.is_empty();
        match m.written {
            Written::Bare => !root || !assoc,
            Written::InThisModule => root && !assoc,
            Written::OnAValue => root && assoc,
            Written::InAMacro => true,
        }
    };
    let candidates: Vec<(usize, &Def)> = defs
        .iter()
        .enumerate()
        .filter(|(_, d)| d.name == m.name && nameable(d))
        .filter(|(_, d)| match d.path.split_last() {
            Some((_, declared_in)) => d.owner == here.owner && here.path.starts_with(declared_in),
            None => true,
        })
        .collect();
    let Some(depth) = candidates.iter().map(|(_, d)| d.path.len()).max() else {
        return Ok(None);
    };
    let mut innermost = candidates.iter().filter(|(_, d)| d.path.len() == depth);
    let (winner, _) = *innermost.next().expect("the maximum is one of them");
    // Paths don't record blocks, so sibling-block definitions tie and can't be told apart.
    match innermost.next() {
        None => Ok(Some(winner)),
        Some(_) => Err(m.name.clone()),
    }
}

/// The cycles in `reaches`, each with members in `defs` order (shallowest first), deepest cycle
/// first so inner cycles are rewritten before the ones holding them.
pub(super) fn cycles(defs: &[Def], reaches: &[Vec<bool>]) -> Vec<Vec<usize>> {
    let mut grouped = vec![false; defs.len()];
    let mut out: Vec<Vec<usize>> = Vec::new();
    for i in 0..defs.len() {
        if grouped[i] || !reaches[i][i] {
            continue;
        }
        // Earlier defs are already grouped or in no cycle.
        let members: Vec<usize> = (i..defs.len())
            .filter(|&j| reaches[i][j] && reaches[j][i])
            .collect();
        for &j in &members {
            grouped[j] = true;
        }
        out.push(members);
    }
    // Found outermost first; reverse to deepest first.
    out.reverse();
    out
}

/// A name this body might be calling, and how it was written.
struct Mention {
    name: Ident,
    written: Written,
    /// Set (path text, span) for a resolvable call the transform can't rewrite, e.g. `T::g(..)`
    /// inside `impl T`, `<Self>::g(..)`, or `crate::m::g(..)` inside `mod m`.
    unrewritable: Option<(String, Span)>,
}

/// How a call names its target, which limits what it can resolve to.
#[derive(PartialEq)]
enum Written {
    /// `g(..)`: a nested or free function, never an associated item.
    Bare,
    /// `self::g(..)`, or `m::g(..)` / `self::m::g(..)` / `super::..::m::g(..)` /
    /// `crate::..::m::g(..)` inside `mod m`: a free root only. (`crate::g` is not recognised; the
    /// macro doesn't know its module path.)
    InThisModule,
    /// `x.g(..)` (any receiver), `Self::g(..)`, `T::g(..)` inside `impl T`, or `<Self>::g(..)` /
    /// `<T ..>::g(..)`: an associated item only.
    OnAValue,
    /// Any identifier inside a macro invocation. Matches anything, so recursion through a macro
    /// is detected and reported.
    InAMacro,
}

/// The names this body might be calling, not looking inside nested items.
///
/// `host` lets calls like `T::g(..)` or `crate::m::g(..)` be recognised as naming this scope.
fn mentioned(func: &ItemFn, assoc: bool, host: Option<&Ident>) -> Vec<Mention> {
    struct V<'a> {
        found: Vec<Mention>,
        assoc: bool,
        host: Option<&'a Ident>,
    }

    impl V<'_> {
        fn mention(&mut self, name: Ident, written: Written) {
            self.found.push(Mention {
                name,
                written,
                unrewritable: None,
            });
        }

        /// Record a call the rewriter can't follow; see [`Mention::unrewritable`].
        fn blocked(&mut self, name: Ident, written: Written, path: &syn::ExprPath) {
            self.found.push(Mention {
                name,
                written,
                unrewritable: Some((pretty_path(path), path.span())),
            });
        }

        /// Classify a multi-segment or qualified path. Only paths that clearly name this scope
        /// are recorded; anything else is ignored.
        fn qualified(&mut self, name: Ident, p: &syn::ExprPath) {
            let segments = &p.path.segments;
            let last = segments.len() - 1;
            let leading = |ty: &syn::Type| match ty {
                syn::Type::Path(tp) => tp.path.segments.first().map(|s| s.ident.clone()),
                _ => None,
            };
            // `<Self>::g` or `<T ..>::g` in `impl T`: a member, but not rewritable.
            if let Some(qself) = &p.qself {
                let names_host = leading(&qself.ty)
                    .is_some_and(|id| id == "Self" || self.host.is_some_and(|h| *h == id));
                if self.assoc && names_host {
                    self.blocked(name, Written::OnAValue, p);
                }
                return;
            }
            if last == 1 && segments[0].ident == "Self" {
                self.mention(name, Written::OnAValue);
                return;
            }
            if last == 1 && segments[0].ident == "self" {
                self.mention(name, Written::InThisModule);
                return;
            }
            let Some(host) = self.host else { return };
            if segments[last - 1].ident != *host {
                return;
            }
            // `..::T::g(..)` inside `impl T`.
            if self.assoc {
                self.blocked(name, Written::OnAValue, p);
                return;
            }
            // `m::g(..)` inside `mod m`, only if rooted at `self`/`super`/`crate` or just `m::g`.
            let rooted = last == 1
                || matches!(
                    segments[0].ident.to_string().as_str(),
                    "self" | "super" | "crate"
                );
            if rooted {
                self.blocked(name, Written::InThisModule, p);
            }
        }

        fn scan_tokens(&mut self, tokens: TokenStream) {
            for tt in tokens {
                match tt {
                    TokenTree::Ident(id) => self.mention(id, Written::InAMacro),
                    TokenTree::Group(g) => self.scan_tokens(g.stream()),
                    _ => {}
                }
            }
        }
    }

    impl<'ast> Visit<'ast> for V<'_> {
        fn visit_expr(&mut self, e: &'ast Expr) {
            match e {
                Expr::Call(c) => {
                    if let Expr::Path(p) = &*c.func {
                        let segs = &p.path.segments;
                        let name = segs.last().expect("non-empty path").ident.clone();
                        if segs.len() == 1 && p.qself.is_none() {
                            self.mention(name, Written::Bare);
                        } else {
                            self.qualified(name, p);
                        }
                    }
                }
                // Any receiver: a method may recurse on another value, like `tail.len()`.
                Expr::MethodCall(m) => self.mention(m.method.clone(), Written::OnAValue),
                Expr::Macro(m) => self.scan_tokens(m.mac.tokens.clone()),
                _ => {}
            }
            syn::visit::visit_expr(self, e);
        }

        fn visit_stmt(&mut self, s: &'ast Stmt) {
            if let Stmt::Macro(m) = s {
                self.scan_tokens(m.mac.tokens.clone());
            }
            syn::visit::visit_stmt(self, s);
        }

        fn visit_item(&mut self, _: &'ast Item) {}
    }

    let mut v = V {
        found: Vec::new(),
        assoc,
        host,
    };
    v.visit_block(&func.block);
    v.found
}

/// A path rendered without token spacing (`crate::m::g`, not `crate :: m :: g`).
fn pretty_path(p: &syn::ExprPath) -> String {
    let mut out = quote::ToTokens::to_token_stream(p).to_string();
    for (from, to) in [
        (" ::", "::"),
        (":: ", "::"),
        (" <", "<"),
        ("< ", "<"),
        (" >", ">"),
        ("> ", ">"),
    ] {
        out = out.replace(from, to);
    }
    out
}

/// Transitive closure: `reaches[i][j]` means `i` can reach `j`.
pub(super) fn closure(edges: &[Vec<bool>]) -> Vec<Vec<bool>> {
    let n = edges.len();
    let mut reaches = edges.to_vec();
    for k in 0..n {
        // Cloned so rows can be updated while row `k` is read.
        let through_k = reaches[k].clone();
        for row in reaches.iter_mut() {
            if row[k] {
                for (reached, via_k) in row.iter_mut().zip(&through_k) {
                    *reached = *reached || *via_k;
                }
            }
        }
    }
    reaches
}
