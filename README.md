# yaspar-macros

A package for useful procedural macros.

This package is a workspace of two crates. `yaspar-macros` holds the procedural macros themselves, and
`yaspar-macros-defs` holds the fixed definitions their expansions refer to, since a proc-macro crate may export nothing
but macros. A crate using these macros therefore depends on both.

This package features two groups of procedural macros:

* `#[stack_safe]`: rewrite (mutually) recursive functions into an iterative state machine whose frames live on the heap,
  so that recursion depth is bounded by available memory rather than by the native stack.
* `#[delegatable_trait]`, `#[delegate_trait]`: implement trait delegation to simulate an object-oriented programming
  style for traits.

Include the following in your Cargo.toml to use these macros:

```toml
[dependencies]
yaspar-macros = "0.1"
yaspar-macros-defs = "0.1" # only need this for #[stack_safe]
```

## Stack-safe Recursions

### TLDR

Tag all your recursive functions with `#[stack_safe]` and enjoy stack-overflow freedom! The transformation is **almost**
zero-cost!

Caveats:
1. Trait impls cannot get involved in recursion.
2. Certain recursive styles require special unsafe options, which are documented below.

### The problem

Recursion is the most natural way to write a tree walk or a backtracking search, and it states a program's *denotation*,
which makes it easier to prove correct than the equivalent loop. What it cannot do is survive its input:

```rust
fn sum(xs: &[u64]) -> u64 {
    match xs.split_first() {
        None => 0,
        Some((head, tail)) => head + sum(tail),
    }
}
```

Every call takes a stack frame, so a long enough slice exhausts the process stack:

```text
thread 'main' has overflowed its stack
fatal runtime error: stack overflow, aborting
```

Raising the limit is no fix — some input is always bigger — and an overflow cannot be caught, so the process dies. The
program is correct; the runtime is what refuses to run it. Rewriting each such function as a loop by hand works, and is
tedious enough that nobody does it consistently.

### What the macro does

A recursive call cuts a body in two: the work before it, and the work after — the *continuation*. `#[stack_safe]` makes
those halves arms of one `match`, and the locals a parked half still needs become a value in a `Vec`. Running a sequence
of arms grows no stack, so depth is bounded by memory instead of by the stack. Turning continuations into enum variants
this way is called *defunctionalization*; it dates to the 70s.

`sum` above becomes roughly the following. There is one entry arm per function and one resume arm per call site, and the
`Vec` holds what the stack used to:

```rust
fn sum(xs: &[u64]) -> u64 {
    enum Entry<A0> { E0(A0) }          // one variant per entry point
    enum Frame<F0> { R0(F0) }          // one per call site, carrying the locals live across it
    enum In<A, R> { Enter(A), Resume(R) }

    let mut frames = Vec::new();
    let mut state = In::Enter(Entry::E0((xs,)));
    let out: u64 = 'drive: loop {
        state = match state {
            // the work *before* the call
            In::Enter(Entry::E0((xs,))) => match xs.split_first() {
                None => In::Resume(0),                    // nothing to recurse into: hand 0 down
                Some((head, tail)) => {
                    frames.push(Frame::R0((head,)));      // `head` is live across the call
                    In::Enter(Entry::E0((tail,)))         // and descend
                }
            },
            // the work *after* it, with the value the call produced
            In::Resume(v) => match frames.pop() {
                None => break 'drive v,                   // the outermost call has finished
                Some(Frame::R0((head,))) => In::Resume(head + v),
            },
        };
    };
    out
}
```

Each arm *answers* with the next state, and that is the whole machine: no call, no closure, nothing on the stack. It all
lives inside the original `fn sum`, so neither its signature nor its call sites change. The enums are generic because a
proc macro cannot write a frame's payload types down — only inference knows what is live across a call.

### Performance

Two examples measure it, and they disagree — which is the useful part.

`cargo run --release --example perf_contrast` sums a balanced 524 287-node tree, so every call returns within about 19
levels and ordinary recursion is never near the stack limit. `manual` is a hand-written worklist over the same tree:

```text
524287 nodes, so that many calls

naive            0.67 ms      1.3 ns/call           0 allocs            0 bytes
manual           1.26 ms      2.4 ns/call           5 allocs          488 bytes
stack_safe       1.23 ms      2.4 ns/call           1 alloc          1536 bytes

leaving the stack at all costs 1.9x (manual / naive)
the macro's encoding costs a further 1.0x (stack_safe / manual)
overhead per call: 1 ns
```

Where the stack is not a constraint, the stack *is* the fastest place to be: a native frame costs nothing to push and the
recursion stays in cache. Leaving it costs about 1 ns per call — and that cost is the leaving, not the macro. The
transform lands level with the hand-written worklist, which is the comparison that isolates its encoding. One allocation
covers the whole descent, since frames are allocated 64 at a time.

Deep recursion reverses it. `cargo run --release --example perf_dispatch_width` walks a 1024-deep chain, where a native
frame per level starts to hurt:

```text
depth 1024, native 8048 ns
   3 call sites:     3820 ns   0.47x native
   9 call sites:     3706 ns   0.46x native
```

So the transform is roughly twice as fast as the recursion it replaces once the depth is real, and about 1 ns/call slower
when it is not. Tag recursive functions by default: the case where it costs you is the case where you did not need it.

### Using it

Annotate the function. Its signature does not change:

```rust
use yaspar_macros::stack_safe;

#[stack_safe]
fn sum(xs: &[u64]) -> u64 {
    match xs.split_first() {
        None => 0,
        Some((head, tail)) => head + sum(tail),
    }
}

let xs: Vec<u64> = (1..=1_000_000).collect();
assert_eq!(sum(&xs), 500_000_500_000);      // a million deep, on any stack size
```

**Mutual recursion.** Rewriting `is_even` needs the body of `is_odd`, so put the attribute on the scope that holds both —
a module or an impl block — and it finds the cycles itself:

```rust
#[stack_safe]
mod parity {
    pub fn is_even(n: u64) -> bool { if n == 0 { true } else { is_odd(n - 1) } }
    pub fn is_odd(n: u64) -> bool { if n == 0 { false } else { is_even(n - 1) } }
    pub fn describe(n: u64) -> &'static str { if is_even(n) { "even" } else { "odd" } }
}

assert!(parity::is_even(1_000_000));
assert!(is_even(1_000_000));                 // also available unqualified
```

The members of a cycle share one machine and differ only in which entry it is seeded at. Anything the scan finds no cycle
for — `describe` here — is emitted exactly as written. Nested modules, impl blocks and functions declared *inside a body*
are all scanned, to any depth, so one attribute covers a module tree. Members may return different types; the macro joins
them into an enum and each wrapper unwraps its own again. Methods join a cycle through `self.g(..)` or `Self::g(self, ..)`.

**`&mut` parameters.** One cannot ride in a frame: at depth *n* there would be *n* frames each holding a `&mut` to the
same object. Such a parameter becomes a *context* the loop owns and lends out for one step at a time, so it stays usable
after a call returns:

```rust
#[stack_safe]
fn collect(n: u64, out: &mut Vec<u64>) {
    if n == 0 { return; }
    out.push(n);
    collect(n / 2, out);
    collect(n / 3, out);
    out.push(n);           // `out` is still usable here
}
```

**The two opt-in options.** Both trade a reference for a raw pointer, and both emit `unsafe` into *your* crate, where your
own `#![forbid(unsafe_code)]` will not see it. Neither is on by default. The invariants are argued in `SAFETY:` comments
and tested under both of Miri's aliasing models — but not proved, so opting in is a decision.

`use_nonlinear_mut` allows recursing into a place *derived* from a `&mut` parameter, where the parent's place must be held
while the child's is lent out. The frame keeps a pointer, swapped in before the call and restored on resume:

```rust
#[stack_safe(use_nonlinear_mut)]
fn bump(t: &mut Tree) -> u64 {
    t.v += 1;
    let mut acc = t.v;
    for i in 0..t.kids.len() { acc += bump(&mut t.kids[i]); }
    acc
}
```

`data_in_frame` allows lending the callee something built at the call site. Natively that value is a temporary of the
caller, whose frame outlives the call; here the arm that built it has already returned, so the value moves into a store
the loop owns, which never moves what it holds and drops it exactly when the callee's subtree ends:

```rust
#[stack_safe(data_in_frame)]
fn rec(n: usize, stack: &Stack<'_, Vec<usize>>) -> usize {
    if stack.len() >= n { n } else { 1 + rec(n, &Stack::Cons(vec![], stack)) }
}
```

Without the option each case is a compile error that names the option, rather than a borrow-check error blamed on the
attribute. The untransformed body is also emitted beside the machine, so your program's *source-level* borrows are still
checked and a program the compiler would have refused is still refused.

### What it handles

Inside a body: `if`, `match`, blocks; `for`, `while`, `while let`, `loop`, with `break` and `continue`; `return` from any
depth; `?` on a `Result`, an `Option`, a `ControlFlow`, or a carrier of your own that implements the two stand-in traits
in `yaspar-macros-defs`; parameters that destructure; `&mut` parameters and `&self` / `&mut self` methods; generics and
where-clauses; `#[cfg]` on a statement, a match arm or a struct field, which travels to every piece the construct is cut
into; and any number of recursive call sites.

Semantics are preserved as well as syntax — argument evaluation order, `&&` / `||` laziness, an iterator expression
evaluated exactly once, every value dropped exactly once even when a panic unwinds through parked frames. Each of those
is checked against a hand-written twin in `tests/observable.rs`.

### What it refuses

Rejections are compile errors on the offending span, and each one's message is pinned in `tests/ui/`. The categories:

* **signatures** it cannot rewrite: `async fn`, `const fn`, variadics, a by-value `self`, `-> !`;
* **call positions** it cannot cut: inside a closure or a macro, in a match guard, an `if let` scrutinee, a `let ... else`
  initialiser, the left side of an assignment, or any position needing a `let` first — and a recursive function *named*
  without being called;
* **names it cannot resolve**, since a macro resolves no paths: a call through `T::f(..)` or `crate::m::f(..)`, two
  members of one name, two same-named `fn`s in sibling blocks, a binding that shadows the function, a turbofish naming a
  *different* instantiation;
* **scopes it cannot carry**: an item declared in a block that then recurses, and a binding that shadows an outer one
  which is read again after the recursion;
* **an attribute that does nothing**: `#[stack_safe]` on a scope where nothing recurses, or on an item that is not a
  function, module or impl block.

Two cases are *not* caught, and both leave a working program that still overflows deeply: a reference hidden behind a type
alias, which the macro cannot see through, and a cycle only partly covered by one attribute, since everything outside its
reach is opaque. Put the attribute on a scope containing the whole cycle.

### Where it differs from ordinary recursion

The suite compares the transform against ordinary recursion on everything it can reach. What follows is what still
differs; each is a test in `tests/adversarial.rs` that pins *both* answers, so a fix fails loudly there.

**Drop timing.** Locals live in frames, not on the stack, and a local nothing after the call mentions is not carried at
all — so it drops *before* the call rather than after:

```rust
#[stack_safe]
fn walk(n: u64) {
    let _g = Guard(n);              // prints on drop
    if n > 0 { walk(n - 1); }
}
```

`walk(2)` prints `leave0 leave1 leave2` natively and `leave2 leave1 leave0` here. An RAII guard held across a recursive
call does not protect that call. Mention it after the call, or scope it in an inner block.

**Things read after the recursion that Rust reads before it.** A method call whose receiver is a place, with a recursive
call among its arguments, reads that place late: a by-value `self` sees what the recursion wrote, and a user `Deref` or an
overloaded `Index` resolves at the wrong time. The same holds for an argument beside a later recursive one, whose coercion
waits. **Hoist the call** — `let v = recurse(..); receiver.method(v)`. These cannot be rejected: the shapes that
misbehave are written exactly like the ones that work, and only a type tells them apart.

**A same-named method on an unrelated receiver.** Inside an annotated impl, every `.g(..)` whose name is a member's is
read as a call into the cycle — which is what makes recursing down a structure work. Call the unrelated one through its
type: `Other::g(&other, ..)`.

**Source locations** name the attribute's line rather than their own, since the body is inside a macro expansion. That
covers `line!()`, `panic!` and `assert!` locations, and `Location::caller()` in anything the body calls. A
`#[track_caller]` function that is itself rewritten is fine: it reports its external call site.

**Addresses**, under the two options. `data_in_frame` moves a lent value into the loop's store and `use_nonlinear_mut`
re-derives a `&mut` parameter on resume, so a program comparing addresses across a call sees them change. Both are what
those options do.

**A grouped module's re-export.** The generated `use m::f` outranks a glob import, so an unqualified `f(..)` that used to
resolve through `use other::*` resolves to `m::f` once the attribute is added. Name the one you mean.

## Trait Delegation and Object Orientation

### Reuse without Inheritance

Object-oriented languages let us reuse an implementation by extending it. We subclass, we override the one method we
care about, and every other method is inherited for free. Rust has no inheritance, and composition takes its place: we
put the old value in a field of the new one, and implement the trait again.

The catch is that a trait impl must supply *every* required method. Suppose we have a small key-value trait:

```rust
#[delegatable_trait]
trait Store {
    fn get(&self, k: u32) -> Option<u64>;
    fn put(&mut self, k: u32, v: u64);
    fn len(&self) -> usize;
}
```

and a wrapper that wants to change `put` alone, e.g. to double what is stored. Overriding one method out of three costs
us two forwarders that say nothing:

```rust
struct Doubling {
    inner: Map
}

impl Store for Doubling {
    fn put(&mut self, k: u32, v: u64) { self.inner.put(k, v * 2); }
    // Everything below is boilerplate.
    fn get(&self, k: u32) -> Option<u64> { self.inner.get(k) }
    fn len(&self) -> usize { self.inner.len() }
}
```

Three methods make this merely annoying. A trait of twenty makes it a maintenance problem, since every method added to
the trait has to be forwarded again in every wrapper. What we want is to write the override and to say that the rest are
inherited, which is exactly what `#[delegate_trait]` does:

```rust
#[delegate_trait(target = inner)]
impl Store for Doubling {
    fn put(&mut self, k: u32, v: u64) { self.inner.put(k, v * 2); }
}
```

The [`delegate`](https://docs.rs/delegate/latest/delegate/) crate addresses the same boilerplate with a `delegate!`
macro, which we invoke inside the impl block and give one bare signature per method to forward:

```rust
impl Store for Doubling {
    fn put(&mut self, k: u32, v: u64) { self.inner.put(k, v * 2); }
    delegate! {
        to self.inner {
            fn get(&self, k: u32) -> Option<u64>;
            fn len(&self) -> usize;
        }
    }
}
```

It is considerably more flexible about *where* a call goes, e.g. to an arbitrary expression, to a `match` over an enum's
variants, or through another trait by UFCS. Nevertheless, we still apply the macro explicitly and enumerate what to
forward, so a method added to `Store` has to be added to every wrapper again, which is the maintenance problem we
started with.

`#[delegate_trait]` requires neither. There is no macro to apply inside the impl block and no list of signatures,
because the required methods come from the trait itself: whatever we do not write is delegated. Thus a new method in
`Store` needs no change in `Doubling` at all. The price of that is the second attribute on the trait, which the next
subsection explains.

### Why Two Attributes

An attribute on an impl block sees only that impl block. It cannot know which methods `Store` requires, so it cannot
know which ones are missing, and the trait may not even live in this crate. The signatures therefore have to travel from
the trait to the impl, and the only carrier a procedural macro can emit that a *later* expansion still sees is a
`macro_rules!` macro.

Hence the pair. `#[delegatable_trait]` emits the trait unchanged, plus a hidden macro holding one arm per required
method, and `#[delegate_trait]` emits the methods we wrote plus an invocation of that macro, which fills in the
remainder:

```text
#[delegatable_trait]        ->  trait Store { .. }                       // unchanged
trait Store { .. }              macro_rules! __delegate_impl_Store { .. }   // one arm per method

#[delegate_trait(..)]       ->  impl Store for Doubling {
impl Store for Doubling {           fn put(..) { .. }                    // ours
    fn put(..) { .. }               __delegate_impl_Store!(
}                                       __delegate_impl_Store, [inner], [put], Store);
                                }                                        // the rest
```

The skip list, i.e. `[put]` above, is matched inside the helper macro rather than in the attribute, because that is the
only place where both halves are known: the attribute knows the names we wrote, and the macro knows the signatures.

The result for `Doubling` is what we would have written by hand:

```rust
impl Store for Doubling {
    fn put(&mut self, k: u32, v: u64) { self.inner.put(k, v * 2); }
    #[inline]
    fn get(&self, k: u32) -> Option<u64> { <_ as Store>::get(&self.inner, k) }
    #[inline]
    fn len(&self) -> usize { <_ as Store>::len(&self.inner) }
}
```

Each forwarder is `#[inline]`, so the hop costs nothing. Note also that the call goes through the trait, as `<_ as
Store>::get(..)`, rather than through `self.inner.get(..)`: an inherent method of the same name on the field's type
would otherwise win the lookup and silently be called instead.

### Usage and Examples

`target` names a *field*, not an expression, so we write `target = inner` and not `target = self.inner`. A tuple
struct's field is named by its index, `target = 0`, and a field of a field by the path to it, `target = a.b`. An empty
impl block delegates everything, which is how we obtain a newtype that behaves exactly like its field:

```rust
struct Wrapper {
    inner: Map
}

#[delegate_trait(target = inner)]
impl Store for Wrapper {}
```

A generic trait works too. Replaying a signature verbatim would emit `fn lookup(&self, k: K)` into `impl Keyed<u32> for
Wrap`, where `K` names nothing, so the helper macro carries the substitution instead: each of the trait's parameters
becomes a metavariable, and the impl passes its trait arguments positionally.

```rust
#[delegatable_trait]
trait Keyed<K> {
    fn lookup(&self, k: K) -> u64;
}

struct Wrap {
    inner: Base
}

#[delegate_trait(target = inner)]
impl Keyed<u32> for Wrap {}
```

Parameters of every kind travel this way, in declaration order. A lifetime becomes a `lifetime` fragment. A const
parameter becomes an `expr`, since there is no `const` fragment, and every use of it is braced, e.g. `[u8; { $n }]`,
which is accepted where a const argument is expected. A defaulted parameter may be left out by the impl, since the trait
knows its own defaults and emits an extra arm that fills them in.

### Supports

All three receiver kinds, i.e. `&self`, `&mut self` and a by-value `self`; generic methods with their own where-clauses;
and a generic trait whose parameters are lifetimes, types, consts, or any interleaving of those, with or without
defaults, including a default that mentions an earlier parameter, as in `trait Pair<A, B = Vec<A>>`. The impl block may
override every method, some of them, or none.

Three things travel with a method that are easy to lose. Its attributes, `#[cfg]` included, so a configured-out method
is not delegated. Its `unsafe`, so the forwarder discharges the obligation onto its own caller instead of leaving an
unsilenceable `unsafe_op_in_unsafe_fn` warning in yours. And a parameter written as a pattern rather than a name, e.g.
`fn b(&self, _: u32)`, which is given a name of its own, since a pattern cannot be replayed as an argument.

### Limitations

Only required *methods* are delegated:

* a required associated type or associated const is not, so the impl block has to supply it as usual. That one is not
  incidental: the attribute sits on the *impl* block, which names `Self` but never the field's type, and `type Item = <_
  as Trait>::Item;` is not allowed, so there is nothing for the macro to write;
* a method with a default body is left to that default, and is delegated only if we override it ourselves;
* a method with no `self` receiver has no field to be forwarded to, so it has to be written in the impl block. The macro
  says so, naming the method, rather than emitting a `self` that is not there.

The helper macro is addressed through the trait's own path: the trait emits an alias beside itself, and
`#[delegate_trait]` swaps the last segment of the trait path for it. Thus a trait from another crate is delegated with
nothing to import:

```rust
#[delegate_trait(target = inner)]
impl other_crate::a::Store for Wrapper {}
```

Naming the trait *bare*, after importing it, leaves no path to follow, and that form works only inside the crate that
defines the trait, where the helper's own `#[macro_export]`ed name is in scope. Writing the path is the fix.

That export brings two items neither written by you nor shown in `cargo doc`: `macro_rules! __delegate_impl_<Trait>`,
which `#[macro_export]` places at the crate root whatever module the trait sits in, and a `pub use` of it as
`__delegate_path_<Trait>` beside the trait. Both are `#[doc(hidden)]`, so `#[delegatable_trait]` on a trait in a private
module still grows the crate's public macro namespace. `local` keeps both `pub(crate)`.

Two `#[delegatable_trait]` traits of the same name in one crate would collide with an `E0428`, since that exported name
lands at the crate root. For that case there is `local`, which keeps the helper out of the root entirely:

```rust
mod first {
    #[delegatable_trait(local)]
    pub trait Named { fn value(&self) -> u64; }
}
mod second {
    #[delegatable_trait(local)]
    pub trait Named { fn value(&self) -> u64; }
}

#[delegate_trait(target = a)]
impl first::Named for BothWrapper {}

#[delegate_trait(target = b)]
impl second::Named for BothWrapper {}
```

The impl side is unchanged, since it addresses the alias by path in either case.

The trade is that a `local` trait cannot be delegated from another crate at all, and the attempt is an `E0603` naming
the private macro. A `macro_rules!` that is not `#[macro_export]`ed is crate-private, and `pub use` of one is itself
rejected with `E0364`, so `pub(crate)` is as far as its alias can reach. That is also why the export cannot simply be
dropped for everyone.

Finally, the trait must carry `#[delegatable_trait]`, since the helper macro is where the signatures come from. A trait
we cannot edit, e.g. `std::fmt::Write`, is therefore not delegatable, whereas one from another crate that carries the
attribute is.

### More Tests and Examples

`tests/delegate_trait.rs` covers partial and empty impl blocks, all receiver kinds, generic methods and where-clauses,
generic traits of each parameter kind including interleaved and defaulted ones, default-bodied methods, and the inherent
method that must not shadow the trait method.
