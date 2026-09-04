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
//! A definition is addressed by its root and the *ordinals* of the definitions that lead to it
//! there, so that a member of a cycle can be taken back out of the body that declares it. An
//! ordinal is a definition's position among the ones its host body declares, counted in the order
//! the walk visits them and without descending into any of them, since each is a scope of its own.
//! A block is descended into like anything else, so a `fn` inside an `if`, a `match` arm or a bare
//! `{ }` is found and addressed like one written at the top of the body — which it has to be, or a
//! cycle running through it would be left to the native stack.
//!
//! The one thing an ordinal has to survive is a member of a cycle being *taken*, since the
//! definitions after it in the same body keep their addresses. What is left behind is therefore
//! counted too: see [`addressable`].
//!
//! A `fn` declared in a block is treated as being in scope throughout the body that holds the
//! block, which is one step more generous than Rust — the name is really only in scope inside the
//! block. It costs nothing here: the extra candidates it admits are functions of the same name in
//! one body, and a call to one of them is a call to *some* function of that name in that body,
//! which is a member of the same scope either way.

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
    /// The ordinals leading to it from that root's body down. Empty for the root.
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
        let own = Opts::take_from(&mut root.attrs)?;
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
        let mut found: Vec<(usize, Ident, Option<Opts>)> = Vec::new();
        let mut failed: Option<syn::Error> = None;
        with_def_mut(&mut roots[owner], &path, &mut |host| {
            match nested_mut(host) {
                Ok(nested) => found = nested,
                Err(e) => failed = Some(e),
            }
        });
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
                // Its own marker shadows the body's; without one it wants what the body wants.
                opts: own.unwrap_or(opts),
                marked: own.is_some(),
            });
        }
        next += 1;
    }
    Ok(defs)
}

/// The functions this body declares, with their ordinals, their names, and whatever their own
/// markers asked for — taken off as they are read, since the walk is what hands options down.
///
/// Anywhere in the body, not just at the top of it: a `fn` inside an `if` or a `match` arm is a
/// definition of the body like any other. Not descended into, since each one is a scope of its own
/// and gets its turn.
fn nested_mut(func: &mut ItemFn) -> syn::Result<Vec<(usize, Ident, Option<Opts>)>> {
    struct V {
        found: Vec<(usize, Ident, Option<Opts>)>,
        next: usize,
        failed: Option<syn::Error>,
    }

    impl VisitMut for V {
        fn visit_item_mut(&mut self, item: &mut Item) {
            if !addressable(item) {
                // Not a definition this addresses, and not descended into either: a `mod` or an
                // `impl` block in a body is a scope of its own, expanded on its own.
                return;
            }
            let ordinal = self.next;
            self.next += 1;
            let Item::Fn(func) = item else { return };
            match Opts::take_from(&mut func.attrs) {
                // The first failure wins, as it would if this returned a `Result`.
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

/// Does this item take up an ordinal in the body that holds it?
///
/// A `fn` does, being what a path addresses. So does the placeholder [`take`] leaves behind, and
/// the tokens [`put_back`] writes there: the definitions after one that has been taken keep the
/// addresses they were given, so what stands in its place has to keep its slot.
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

/// The function at this ordinal among the ones the body declares.
fn nth<'a>(block: &'a Block, ordinal: usize) -> Option<&'a ItemFn> {
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
            if ordinal == self.want {
                if let Item::Fn(func) = item {
                    self.found = Some(func);
                }
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

/// Run `act` on the definition this path addresses, the root itself for an empty one.
///
/// `&mut dyn` rather than a generic, since the walk down a path is recursive and a generic closure
/// would ask the compiler to instantiate the recursion once per level, without end.
fn with_def_mut(root: &mut ItemFn, path: &[usize], act: &mut dyn FnMut(&mut ItemFn)) {
    match path.is_empty() {
        true => act(root),
        false => with_item_mut(root, path, &mut |item| match item {
            Item::Fn(func) => act(func),
            _ => unreachable!("a path addresses a nested function"),
        }),
    }
}

/// Run `act` on the *item* a non-empty path addresses, so that it can be replaced as well as read.
fn with_item_mut(root: &mut ItemFn, path: &[usize], act: &mut dyn FnMut(&mut Item)) {
    struct V<'a> {
        /// The ordinals still to follow; never empty.
        path: &'a [usize],
        next: usize,
        /// Taken when it runs, so nothing can run it twice.
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

/// Take a nested definition out of the body that declares it.
///
/// A member of a cycle is rewritten into a call into the shared driver, and the driver has to
/// be written beside the outermost member rather than inside a body that has itself become
/// one of its arms. A definition nested in another has to be taken before the one holding it,
/// so the caller works from the deepest one up.
///
/// What is left behind is an empty item rather than nothing at all, so that every other
/// path stays valid: a definition is addressed by its ordinal among the ones its body declares,
/// and removing one would shift the definitions after it.
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

/// Write tokens where a definition was taken from: the rewritten cycle — the shared driver, with
/// the members that came from deeper in written inside it and the outermost one beside it.
pub(super) fn put_back(func: &mut ItemFn, path: &[usize], tokens: TokenStream) {
    with_item_mut(func, path, &mut |item| *item = placeholder(tokens.clone()));
}

fn placeholder(tokens: TokenStream) -> Item {
    Item::Verbatim(tokens)
}

/// One edge per call from one definition in scope to another.
///
/// The name is resolved the way Rust resolves it: to the innermost definition in scope
/// that carries it, so a nested function shadowing an outer one takes the edge.
/// `edges[i][j]` is set when `i`'s body mentions `j`: a call the transform can rewrite, or a
/// name inside a macro it cannot — see [`mentioned`].
///
/// `assoc` says the roots are associated items of an impl block, which changes what a name can
/// mean: `self.g(..)` and `Self::g(..)` reach them and a bare `g(..)` does not. `host` is the name
/// the scope goes by where it is written — the impl block's own type, or the annotated module's
/// ident — since a call may spell that out instead of saying `Self` or `self`.
///
/// Beside the edges come the calls that name a definition in scope through a path the *rewriter*
/// cannot follow. They are no edges: an edge says the call becomes an entry into a driver, and one
/// left as a native call would make a half-flattened function that still grows the stack. What
/// becomes of them is [`scan`](super::scan)'s to report.
pub(super) fn edges(
    roots: &[ItemFn],
    defs: &[Def],
    assoc: bool,
    host: Option<&Ident>,
) -> (Vec<Vec<bool>>, Vec<Blocked>) {
    let mut rows = Vec::with_capacity(defs.len());
    let mut blocked = Vec::new();
    for (i, d) in defs.iter().enumerate() {
        let mut row = vec![false; defs.len()];
        for m in mentioned(at(&roots[d.owner], &d.path), assoc, host) {
            let Some(j) = resolve(defs, i, &m, assoc) else {
                continue;
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
    (rows, blocked)
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
    /// The path as written, when it says plainly enough what it names for the scan to resolve it,
    /// but is not one of the shapes the transform *rewrites* — `T::g(..)` inside `impl T`,
    /// `<Self>::g(..)`, `crate::m::g(..)` inside `#[stack_safe] mod m`. Such a call is reported
    /// rather than turned into an edge, since an edge whose call site stays a native call would
    /// leave a function half flattened and still growing the stack. With the span of the path, for
    /// the report.
    unrewritable: Option<(String, Span)>,
}

/// The ways a call can name what it calls, as far as resolution is concerned.
#[derive(PartialEq)]
enum Written {
    /// `g(..)`: a path of one segment. It reaches a function declared in a body, or a free
    /// function — never an associated item, which is not in scope under a bare name.
    Bare,
    /// `self::g(..)`, and `super::m::g(..)` or `crate::a::m::g(..)` inside `#[stack_safe] mod m`:
    /// this module's `g`, whichever body the call is written in. It reaches a free root, since that
    /// is what a module holds, and never a function declared in a body, which no module path names.
    /// A bare `crate::g` or `super::g` is *not* this: a macro does not know its own module path, so
    /// unless the path names the annotated module on the way through it cannot tell whether they
    /// name something in this scope.
    InThisModule,
    /// `self.g(..)`, `Self::g(self, ..)`, `other.g(..)`, `T::g(..)` inside `impl T`, or any
    /// `<..>::g(..)`: only an associated item.
    OnAValue,
    /// A name inside a macro invocation, where nothing says how it is used. It reaches
    /// anything, which is what lets a cycle running through a macro be reported as such
    /// rather than quietly recursing on the native stack.
    InAMacro,
}

/// The names this body might be calling.
///
/// Not descended into: a nested function, which is a scope of its own and gets its own row.
///
/// `host` is what the scope is called where it is written — the impl block's own type, or the
/// annotated module's ident. A call may name a member through it (`T::g(..)`, `crate::m::g(..)`)
/// rather than through `Self` or `self`, and such a call has to be seen, or the cycle it takes part
/// in is not found and the recursion is left on the native stack with nothing said about it.
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

        /// A call the scan can resolve and the rewriter cannot follow. See
        /// [`Mention::unrewritable`].
        fn blocked(&mut self, name: Ident, written: Written, path: &syn::ExprPath) {
            self.found.push(Mention {
                name,
                written,
                unrewritable: Some((pretty_path(path), path.span())),
            });
        }

        /// How a call names what it calls, where the path has more than one segment or a qself.
        ///
        /// Only the shapes that say plainly that they name *this* scope. Anything else may name
        /// anything at all — a macro resolves no paths — and reading it as a member would put a
        /// function that never recurses into a cycle.
        fn qualified(&mut self, name: Ident, p: &syn::ExprPath) {
            let segments = &p.path.segments;
            let last = segments.len() - 1;
            let leading = |ty: &syn::Type| match ty {
                syn::Type::Path(tp) => tp.path.segments.first().map(|s| s.ident.clone()),
                _ => None,
            };
            // A qself is always an associated item: `<Self>::g`, `<T as Tr>::g`. Only one naming
            // the type this block is for can be a member of it.
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
            // `T::g(..)` inside `impl T`, however the path reached `T`.
            if self.assoc {
                self.blocked(name, Written::OnAValue, p);
                return;
            }
            // `m::g(..)` inside `mod m`, but only where the path is rooted somewhere that could
            // lead back to this very module: another `m` elsewhere in the crate is not this one.
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

    let mut v = V {
        found: Vec::new(),
        assoc,
        host,
    };
    v.visit_block(&func.block);
    v.found
}

/// A path as the user wrote it, for a message that has to quote it back: `to_token_stream`
/// renders `crate::m::g` as `crate :: m :: g`.
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
