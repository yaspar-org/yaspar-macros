// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! The functions in scope, and which of them recurse.
//!
//! A body is a scope of item definitions like any other, so a `fn` nested in one can recurse:
//! alone, through the function that hosts it, or through its siblings. Everything in scope is
//! therefore scanned at once — one or more *roots*, being the annotated function or the
//! functions of an annotated container, and whatever their bodies declare — so that a cycle is
//! found wherever it runs.
//!
//! A definition is addressed by its root and the statement indices that lead to it there, so
//! that a member of a cycle can be taken back out of the body that declares it. Only the
//! statements of a body are looked at, not the blocks nested in them: a `fn` inside an `if` is
//! left as written.

use proc_macro2::{Ident, TokenStream, TokenTree};
use syn::visit::Visit;
use syn::{Expr, Item, ItemFn, Stmt};

use super::Opts;

/// A function in scope: one of the roots, or one declared in the body of a root at any depth.
pub(super) struct Def {
    /// Which root it belongs to.
    pub(super) owner: usize,
    /// The statement indices leading to it from that root's body down. Empty for the root.
    pub(super) path: Vec<usize>,
    pub(super) name: Ident,
    /// The options in force where it is declared: its own `#[stack_safe]` marker if it carries
    /// one, and otherwise the ones in force around it. A marker therefore *shadows* rather than
    /// adds — it says what this function and its body want, in full, exactly as an inner binding
    /// of a name says what that name means from there down.
    pub(super) opts: Opts,
    /// Did it carry a marker of its own? Only its own makes a function that turns out not to
    /// recurse *this* function's mistake.
    pub(super) marked: bool,
}

/// Every function in the roots' scope: the roots themselves, then what their bodies declare,
/// shallowest first — so a definition always comes before the ones nested in it.
///
/// This is the walk that visits a scope in declaration order, so it is also where each
/// `#[stack_safe]` marker is read and taken off, and where the options in force are handed down: a
/// definition inherits its host's unless it carries a marker of its own, which then shadows them
/// outright.
pub(super) fn collect(roots: &mut [ItemFn], opts: Opts) -> syn::Result<Vec<Def>> {
    let mut defs: Vec<Def> = Vec::with_capacity(roots.len());
    for (owner, root) in roots.iter_mut().enumerate() {
        let own = Opts::take_from(root)?;
        defs.push(Def {
            owner,
            path: Vec::new(),
            name: root.sig.ident.clone(),
            // Its own marker if it has one, and otherwise what the attribute itself was given.
            opts: own.unwrap_or(opts),
            marked: own.is_some(),
        });
    }
    let mut next = 0;
    while next < defs.len() {
        let (owner, opts) = (defs[next].owner, defs[next].opts);
        // Owned, so that `defs` can grow while the body it addresses is being read.
        let path = defs[next].path.clone();
        // One walk down to the body, not one per function it declares.
        for (index, inner) in nested_mut(at_mut(&mut roots[owner], &path)) {
            let own = Opts::take_from(inner)?;
            let mut path = path.clone();
            path.push(index);
            defs.push(Def {
                owner,
                name: inner.sig.ident.clone(),
                path,
                // Its own marker shadows the body's; without one it wants what the body wants.
                opts: own.unwrap_or(opts),
                marked: own.is_some(),
            });
        }
        next += 1;
    }
    Ok(defs)
}

/// The functions declared directly in this body, with their statement indices.
fn nested_mut(func: &mut ItemFn) -> impl Iterator<Item = (usize, &mut ItemFn)> {
    func.block
        .stmts
        .iter_mut()
        .enumerate()
        .filter_map(|(i, s)| match s {
            Stmt::Item(Item::Fn(f)) => Some((i, f)),
            _ => None,
        })
}

/// Follow a path down to the definition it addresses.
pub(super) fn at<'a>(func: &'a ItemFn, path: &[usize]) -> &'a ItemFn {
    let mut func = func;
    for &i in path {
        func = match &func.block.stmts[i] {
            Stmt::Item(Item::Fn(inner)) => inner,
            _ => unreachable!("a path addresses a nested function"),
        };
    }
    func
}

pub(super) fn at_mut<'a>(func: &'a mut ItemFn, path: &[usize]) -> &'a mut ItemFn {
    let mut func = func;
    for &i in path {
        func = match &mut func.block.stmts[i] {
            Stmt::Item(Item::Fn(inner)) => inner,
            _ => unreachable!("a path addresses a nested function"),
        };
    }
    func
}

/// Take a nested definition out of the body that declares it.
///
/// A member of a cycle is rewritten into a call into the shared driver, and the driver has to
/// be written beside the outermost member rather than inside a body that has itself become
/// one of its arms. A definition nested in another has to be taken before the one holding it,
/// so the caller works from the deepest one up.
///
/// What is left behind is an empty statement rather than nothing at all, so that every other
/// path stays valid: a definition is addressed by statement index, and removing one would
/// shift the definitions after it.
pub(super) fn take(func: &mut ItemFn, path: &[usize]) -> ItemFn {
    let (&i, parent) = path.split_last().expect("the annotated function stays put");
    let stmt = &mut at_mut(func, parent).block.stmts[i];
    match std::mem::replace(stmt, placeholder(TokenStream::new())) {
        Stmt::Item(Item::Fn(inner)) => inner,
        _ => unreachable!("a path addresses a nested function"),
    }
}

/// Write tokens where a definition was taken from: the rewritten cycle — the shared driver, with
/// the members that came from deeper in written inside it and the outermost one beside it.
pub(super) fn put_back(func: &mut ItemFn, path: &[usize], tokens: TokenStream) {
    let (&i, parent) = path.split_last().expect("the annotated function stays put");
    at_mut(func, parent).block.stmts[i] = placeholder(tokens);
}

fn placeholder(tokens: TokenStream) -> Stmt {
    Stmt::Item(Item::Verbatim(tokens))
}

/// One edge per call from one definition in scope to another.
///
/// The name is resolved the way Rust resolves it: to the innermost definition in scope
/// that carries it, so a nested function shadowing an outer one takes the edge.
/// `edges[i][j]` is set when `i`'s body mentions `j`: a call the transform can rewrite, or a
/// name inside a macro it cannot — see [`mentioned`].
///
/// `assoc` says the roots are associated items of an impl block, which changes what a name can
/// mean: `self.g(..)` and `Self::g(..)` reach them and a bare `g(..)` does not.
pub(super) fn edges(roots: &[ItemFn], defs: &[Def], assoc: bool) -> Vec<Vec<bool>> {
    defs.iter()
        .enumerate()
        .map(|(i, d)| {
            let mut row = vec![false; defs.len()];
            for m in mentioned(at(&roots[d.owner], &d.path)) {
                if let Some(j) = resolve(defs, i, &m, assoc) {
                    row[j] = true;
                }
            }
            row
        })
        .collect()
}

/// Which definition a mention, written inside definition `from`, refers to.
///
/// Two questions, and a candidate has to pass both. *Is it in scope here?* One declared in a
/// body is, from there down, and only inside the body that declares it; a root is, throughout,
/// being a sibling of every root. *Can it be named this way?* A bare `g(..)` reaches a function
/// declared in a body, or a free one, but never an associated item, which is not in scope under
/// a bare name — such a call is the free `g`, not the method beside it. `self.g(..)`,
/// `Self::g(..)` and `other.g(..)` are the other way round: an associated item only. `self::g(..)`
/// names a module's own item, so it reaches a free root and nothing declared in a body.
///
/// Where several candidates remain, the innermost wins, exactly as Rust resolves it.
fn resolve(defs: &[Def], from: usize, m: &Mention, assoc: bool) -> Option<usize> {
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
    defs.iter()
        .enumerate()
        .filter(|(_, d)| d.name == m.name && nameable(d))
        .filter(|(_, d)| match d.path.split_last() {
            Some((_, declared_in)) => d.owner == here.owner && here.path.starts_with(declared_in),
            None => true,
        })
        .max_by_key(|(_, d)| d.path.len())
        .map(|(i, _)| i)
}

/// One entry per cycle, its members in `defs` order — so the shallowest member comes first —
/// and innermost cycle first, since a cycle has to be rewritten before the one that holds its
/// members can take them out.
pub(super) fn cycles(defs: &[Def], reaches: &[Vec<bool>]) -> Vec<Vec<usize>> {
    let mut grouped = vec![false; defs.len()];
    let mut out: Vec<Vec<usize>> = Vec::new();
    for i in 0..defs.len() {
        if grouped[i] || !reaches[i][i] {
            continue;
        }
        // Anything earlier is either already in a cycle or in none, so a member of this one
        // cannot be behind `i`.
        let members: Vec<usize> = (i..defs.len())
            .filter(|&j| reaches[i][j] && reaches[j][i])
            .collect();
        for &j in &members {
            grouped[j] = true;
        }
        out.push(members);
    }
    // `defs` is shallowest first, so the cycles come out outermost first.
    out.reverse();
    out
}

/// A name this body might be calling, and how it was written — which is what says what the
/// name can possibly mean.
struct Mention {
    name: Ident,
    written: Written,
}

/// The ways a call can name what it calls, as far as resolution is concerned.
#[derive(PartialEq)]
enum Written {
    /// `g(..)`: a path of one segment. It reaches a function declared in a body, or a free
    /// function — never an associated item, which is not in scope under a bare name.
    Bare,
    /// `self::g(..)`: this module's `g`, whichever body the call is written in. It reaches a free
    /// root, since that is what a module holds, and never a function declared in a body, which no
    /// module path names. `crate::g` and `super::g` are *not* this: a macro does not know its own
    /// module path, so it cannot tell whether they name something in this scope.
    InThisModule,
    /// `self.g(..)`, `Self::g(self, ..)`, or `other.g(..)`: only an associated item.
    OnAValue,
    /// A name inside a macro invocation, where nothing says how it is used. It reaches
    /// anything, which is what lets a cycle running through a macro be reported as such
    /// rather than quietly recursing on the native stack.
    InAMacro,
}

/// The names this body might be calling.
///
/// Not descended into: a nested function, which is a scope of its own and gets its own row.
fn mentioned(func: &ItemFn) -> Vec<Mention> {
    struct V {
        found: Vec<Mention>,
    }

    impl V {
        fn mention(&mut self, name: Ident, written: Written) {
            self.found.push(Mention { name, written });
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

    impl<'ast> Visit<'ast> for V {
        fn visit_expr(&mut self, e: &'ast Expr) {
            match e {
                Expr::Call(c) => {
                    if let Expr::Path(p) = &*c.func {
                        let segs = &p.path.segments;
                        let name = segs.last().expect("non-empty path").ident.clone();
                        if segs.len() == 1 {
                            self.mention(name, Written::Bare);
                        } else if segs.len() == 2 && segs[0].ident == "Self" {
                            self.mention(name, Written::OnAValue);
                        } else if segs.len() == 2 && segs[0].ident == "self" {
                            self.mention(name, Written::InThisModule);
                        }
                    }
                }
                // Any receiver, not just `self`: a method can recurse on another value of the
                // same type, as `tail.len()` does.
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

    let mut v = V { found: Vec::new() };
    v.visit_block(&func.block);
    v.found
}

/// Transitive closure, so `reaches[i][j]` means "`i` can reach `j`".
pub(super) fn closure(edges: &[Vec<bool>]) -> Vec<Vec<bool>> {
    let n = edges.len();
    let mut reaches = edges.to_vec();
    for k in 0..n {
        // Cloned so row `i` can be updated while row `k` is read; `n` is the number of
        // functions in one scope, so this costs nothing.
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
