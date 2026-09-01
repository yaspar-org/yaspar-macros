# yaspar-macros

A package for useful procedural macros.

This package is a workspace of two crates. `yaspar-macros` holds the procedural macros themselves, and
`yaspar-macros-defs` holds the fixed definitions their expansions refer to, since a proc-macro crate may export nothing
but
macros. A crate using these macros therefore depends on both.

This package features two groups of procedural macros:

* `#[stack_safe]`: implement stack-safe transformations on (mutually) recursive functions, so
  that they become stack safe, i.e. never cause stack overflow on any input size. The scan is arbitrarily deep.
* `#[delegatable_trait]`, `#[delegate_trait]`: implement trait delegation to simulate an object-oriented programming
  style for traits.

Include the following in your Cargo.toml to use these macros:

```toml
[dependencies]
yaspar-macros = "0.1"
yaspar-macros-defs = "0.1" # only need this for #[stack_safe]
```

## Stack-safe Recursions

### Recursion: Good and Bad

Recursion is pervasive in computer science. It is arguably the most natural way to perform certain tasks, e.g. tree
walks,
backtracking search, etc. In addition, recursion also reveals the **denotation** of a program, making it simpler to
prove
correctness of the given program than its equivalent iterative form.

For example, the following function sums a slice of `u64`:

```rust
fn sum(xs: &[u64]) -> u64 {
    match xs.split_first() {
        None => 0,
        Some((head, tail)) => head + sum(tail),
    }
}
```

This function is pretty standard in functional programming and is mathematically correct.

Until we start to execute the function. This is the bad part of recursions: recursion depth is bound by the process
stack size.
Each recursive call grows a stack frame, and the operating system would just send a `SIGSEGV` signal if we exceed the
stack
limit:

```
thread 'main' (900263) has overflowed its stack
fatal runtime error: stack overflow, aborting
```

Growing the stack size by tuning the OS config is not an ideal fix; it is always possible to develop an input size to
blow
up the process stack. Worse yet, a stack overflow is fatal. There is no way to recover from it, and we must immediately
abort the process. This is unfortunate, because our program is mathematically correct, but the runtime prevents us from
applying it universally to any input.

### Manual Transformation of Recursion to Iteration

One solution to this problem is to manually rewrite the `sum` function into an iterative form:

```rust
fn sum_iter(xs: &[u64]) -> u64 {
    let mut acc = 0;
    let mut rest = xs;
    while let Some((head, tail)) = rest.split_first() {
        acc += head;
        rest = tail;
    }
    acc
}
```

Nevertheless, rewriting every recursive function is not always natural, and is tedious at least. Thus, it is motivated
to
develop a procedure to automatically perform such transformation, so that we can write recursions freely without having
to worry about stacks in execution time.

### Generalization: Continuation Passing Style (CPS) Transformation

In general, in a recursive function, we can segment its body into multiple parts by its recursive calls, i.e.
checkpoints.
In other words, we can view any recursive definition as a sequence of "work to be done for the next recursive call" and
"work to do after the next recursive call". These two kinds alternate until the execution of the recursive function
finishes.
Abstracting this "work" gives us the concept of a **continuation**. A programming style that manipulates continuations
as
basic building blocks is called continuation passing style (CPS).

By viewing a function (any, not just a recursive one) as a sequence of continuations, it is not hard to see that
executing
them in order does not grow any local stack. Thus, CPS gives us a way to perform stack-safe recursion with a competitive
performance parity. Roughly, we split a recursive function into chunks. Each chunk is a continuation and is represented
as
a case in an enum. We carry the program state, which is implicitly managed by stack, in one case of the enum and rely on
a state transition table to encode how a continuation moves to another. This source transformation is called CPS
transformation,
and is the heart of what the `#[stack_safe]` procedural macro does.

The technique of defining continuations as an enum is called **defunctionalization**, which dates back to the 70s. Each
call
site becomes a variant of a frame enum carrying the locals that are live across that call, and the code after the call
becomes a `match` arm. The driver's stack is then a `Vec` of plain values.

By adding `#[stack_safe]` to the `sum` function, the program is transformed as follows. Names are shortened here, and
the
context argument that carries `&mut` parameters is omitted, since `sum` has none:

```rust
// What the driver hands to the body.
enum In<A, F, R> { Enter(A), Resume(F, R) }
// What the body hands back to the driver.
enum Step<A, F, R> { Done(R), Call(A, F), Tail(A) }

// The driver converts recursions into loops. It is generic, i.e. it knows nothing about `sum`.
fn drive<A, F, R>(init: A, mut body: impl FnMut(In<A, F, R>) -> Step<A, F, R>) -> R {
    let mut stack: Vec<F> = Vec::new();            // the recursion, on the heap
    let mut step = body(In::Enter(init));
    loop {
        match step {
            Step::Tail(args) => step = body(In::Enter(args)),
            Step::Call(args, frame) => {
                stack.push(frame);
                step = body(In::Enter(args));
            }
            Step::Done(r) => match stack.pop() {
                None => return r,                  // the outermost call has finished
                Some(frame) => step = body(In::Resume(frame, r)),
            },
        }
    }
}

fn sum(xs: &[u64]) -> u64 {
    // One variant per entry point; here, only `sum` itself.
    enum Entry<A0> { E0(A0) }
    // One variant per recursive call site, carrying the locals live across it.
    enum Frame<F0> { R0(F0) }

    let out: u64 = drive(Entry::E0((xs,)), |input| match input {
        // The body, entered with the arguments of a call.
        In::Enter(Entry::E0((xs, ))) => match xs.split_first() {
            None => Step::Done(0),
            Some((head, tail)) => {
                let v0 = head;                         // the left operand of `+`
                Step::Call(Entry::E0((tail,)), Frame::R0((v0,)))
            }
        },
        // The rest of the body, resumed with the result of that call.
        In::Resume(Frame::R0((v0, )), v1) => Step::Done(v0 + v1),
    });
    out
}
```

Everything lives inside the original `fn sum`, so neither its signature nor its call sites change. `In`, `Step` and
`drive` are shown inline here to keep the example readable. A real expansion imports them from `yaspar-macros-defs`,
since they are the same for every function. Only the entry and frame enums are nested items, i.e. the halves that vary
per function. `out` is simply the value that the driver returns, i.e. the value that `sum` returns.

Note that the closure captures nothing. Every value it needs arrives either in an entry payload, e.g. `xs`, or in a
frame
payload, e.g. `v0`. This is precisely why the recursion can live in a `Vec` on the heap.

We can read the arms against the original. `None => 0` becomes `Done(0)`, and `head + sum(tail)` becomes two arms: the
`Call` says to evaluate the tail and to remember `head` as `v0`, and the `Resume` arm adds `v0` to the result once it
arrives. `head` travels in the frame because it is the only local live across the call, i.e. exactly what a stack frame
would have held. The `Tail` variant is unused here, since `sum` contains no loop.

Note also that `head` is hoisted into `v0` before the `Call` is issued, instead of being read after it. The reason is
that Rust evaluates operands and arguments from left to right, while the transformation cuts the body at the recursive
call. Whatever sits to the left of that cut must therefore run before the `Call` step, and its value travels in the
frame;
whatever sits to the right of it lands in the `Resume` arm and runs once the result arrives.

For example, `f(a(), sum(n - 1), b())` is transformed into two arms:

```rust
In::Enter(Entry::E0((n,))) => {
let v0 = a();                                     // left of the cut, so it runs before the call
Step::Call(Entry::E0((n - 1,)), Frame::R0((v0,)))
}
In::Resume(Frame::R0((v0,)), v1) => Step::Done(f(v0, v1, b())),   // right of the cut, so it runs after
```

Here `a()` cannot be left in the `Resume` arm, or it would run after the recursion, and it cannot be evaluated twice
either, since it may have side effects. Thus its value is what travels. If `a` and `b` print their names, then `sum(2)`
prints `a a b b` under both the original and the transformed program.

`sum` is minimal, in that it has one entry point, one call site, and one local live across that call. Larger bodies
scale
in three directions:

* **Call sites**: each one gets its own frame variant. A body with two recursive calls gets `R0` and `R1`, where the arm
  of `R0` holds the code between the two calls and issues the second one, and the arm of `R1` holds the code after both.
  Every frame carries only what its own arm still needs, e.g. `R0` carries `n` when the second call's argument mentions
  it, while `R1` no longer does.
* **Loops**: each loop gets its own entry variant, and one iteration is a `Tail` step, i.e. a re-entry that pushes no
  frame. Thus an iteration costs no stack at all. The iterator of a `for` loop travels in the payload of that entry,
  together with every local that the next iteration still needs.
* **Payloads**: their types are never written down. The macro sees tokens rather than types, so the enums are generic
  and
  inference fills them in from the construction sites. What the macro must compute for itself is which locals are live
  at
  each point; had we kept continuations as closures, capture inference would have done that for us.

### Performance Comparison (with Release Flag)

`cargo run --release --example perf_contrast` sums a 524 287-node tree in three ways, where `manual` is a hand-written
worklist over the same tree:

```
524287 nodes, so that many calls

naive            1.47 ms      2.8 ns/call           0 allocs (0.00/call)            0 bytes   sum 262144
manual           2.08 ms      4.0 ns/call           5 allocs (0.00/call)          488 bytes   sum 262144
stack_safe       1.43 ms      2.7 ns/call           4 allocs (0.00/call)         1440 bytes   sum 262144
```

The transformation is highly optimized and performs virtually identical to the native recursive implementation, and
better than manually transformed version. It allocates on the heap with a minimal amount and has an amortized linear
growth to the call depth. In practice, it is always recommended to tag recursive functions with this macro if no option
is needed.

### Usage and Examples

The simplest application is to annotate a function with `#[stack_safe]`. Its signature does not change, so callers are
unaffected:

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
assert_eq!(sum(&xs), 500_000_500_000);      // 1 000 000 deep, on any stack size
```

#### Mutually recursive functions

Functions can also recurse through each other, e.g. `is_even` calls `is_odd` and `is_odd` calls `is_even`. The
transformation has to see every body of such a cycle at once, since turning `is_odd(..)` inside `is_even` into a step of
the
same state machine requires the body of `is_odd`. So we put the macro on the scope that holds them both, a module or an
impl
block, and it works out the cycles by itself:

```rust
#[stack_safe]
mod parity {
    pub fn is_even(n: u64) -> bool { if n == 0 { true } else { is_odd(n - 1) } }
    pub fn is_odd(n: u64) -> bool { if n == 0 { false } else { is_even(n - 1) } }
    pub fn describe(n: u64) -> &'static str { if is_even(n) { "even" } else { "odd" } }
}

assert!(parity::is_even(1_000_000));
assert!(is_even(1_000_000));                // also available unqualified
```

Every member of a cycle receives its own entry variant, and all of them are compiled to one and the same body:

```rust
enum Entry<A0, A1> { E0(A0), E1(A1) }         // `E0` is `is_even`, and `E1` is `is_odd`

In::Enter(Entry::E0((n,))) => if n == 0 { Step::Done(true) }
else { Step::Call(Entry::E1((n - 1, )), Frame::R0(())) },
In::Enter(Entry::E1((n,))) => if n == 0 { Step::Done(false) }
else { Step::Call(Entry::E0((n - 1, )), Frame::R1(())) },
```

A call from one member into another is therefore just another step of the driver, and the two functions differ only in
which entry the driver is seeded with, `E0` for `is_even` and `E1` for `is_odd`. Note that such a call still parks a
frame, i.e. it is not turned into a tail call, but that frame lives in the `Vec` instead of on the native stack, which
is
exactly the point.

To find the cycles, the macro adds one edge per syntactic call among the functions in scope, i.e. the container's own
and everything their bodies declare, and takes the transitive closure. Two functions belong to the same group if each of
them reaches the other, and a function is
recursive
at all if it reaches itself. Thus `describe` above is emitted exactly as written, since it calls `is_even` without ever
being called back.

Nested modules and impl blocks are scanned to any depth and are grouped separately, so one macro covers a whole
module tree. Methods join a cycle through `self.g(..)` or `Self::g(self, ..)`. Since the members of a group share one
body, they must also agree on their `&mut` parameters, and a mismatch is a compile error that names the pair.

Mutually recursive functions can have different return types:

```rust
#[stack_safe]
mod m {
    pub fn is_even(n: u64) -> bool {
        if n == 0 { true } else { count(n - 1) % 2 == 1 }
    }
    pub fn count(n: u64) -> u64 {
        if n == 0 { 0 } else if is_even(n - 1) { 1 } else { 2 }
    }
}
```

The driver has one result, so the macro joins the members' return types into an enum of its own, one variant per member.
Each member wraps its answer on the way out and its wrapper unwraps it again, so every signature survives.

When a return type is an opaque type, i.e. `impl Trait`, the transformation only works if a function is self-recursive.
In a mutually recursive case, we are not able to fit opaque types in enums, so a group of such mutual recursions is
rejected. The fix is to write down explicit return types or use `Box`.

A parameter type is under less pressure than a return type. The seed enum carries the members' own generic parameters,
i.e. the union of them, keyed by name. Thus a generic cycle, one naming a lifetime, and one passing `&dyn Trait` all
share a single machine. We compare bounds as sets, and a where-clause counts as bounds, so `T: Copy + Into<u64>`, `T:
Into<u64> + Copy` and `T` with `where T: Into<u64> + Copy` are one requirement. Where the parameters cannot be shared,
the group is simply not lifted: each member gets its own copy of the machine instead, and nothing is rejected. This
happens when two members ask genuinely different things of the same name, or when a parameter is used in no parameter
type at all. An `impl Trait` parameter is the exception. Nothing then pins the payload's type, and the result is an
`E0282` on the body rather than an error from the macro. Writing the parameter as a named generic with a where-clause
both names the type and keeps the group sharing one machine.

Finally, a grouped module re-exports each of its top-level functions beside itself:

```rust
mod parity {
    pub fn is_even(n: u64) -> bool { /* the state machine */ }
}
use parity::is_even;
```

Thus `is_even(..)` can be called unqualified at the scope of the macro, and not only as `parity::is_even(..)`. A `use`is
chosen over a forwarding definition because it never has to reproduce a signature, so a generic, a where-clause, or a
type
that only the module can name all come along for free. Visibility is re-expressed rather than copied, e.g. a
`pub(super) fn` becomes private one level up, so that no name out-reaches its module, and a function the module keeps
private is not re-exported at all.

#### Functions declared in a body

A body is a scope of item definitions like any other, so the scan does not stop at the annotated function. A `fn`
declared inside it is scanned as well, to any depth of nesting. Everything under one `#[stack_safe]` becomes a single
graph: the annotated function, or the container's functions, plus whatever their bodies declare. We then read the
cycles off that one graph, so a cycle is found wherever it runs. It may sit within one body, run from a body into the
function hosting it, or leave a body for a different function of the same container. Below, `step` recurses through
`depth` and `depth` through `step`, so we flatten the two together:

```rust
#[stack_safe]
fn depth(n: u64) -> u64 {
    fn step(n: u64) -> u64 { if n == 0 { 0 } else { 1 + depth(n - 1) } }
    if n == 0 { 0 } else { 1 + step(n - 1) }
}

assert_eq!(depth(1_000_000), 1_000_000);
```

Nested functions in a cycle among themselves get a driver of their own, and one that recurses alone gets one as well.
A nested function in no cycle at all is emitted as written, so a body can hold a mix, just as a module can.

A cycle's driver is written where the outermost of its members was declared.
A nested `fn` prevents the cycle driver from being generic. A driver does carry its members' generic parameters, which
is
how a generic cycle shares one. But a member declared in a body can never name them, since a nested `fn` sees none of
the generics of the one hosting it. It could only call the cycle at some concrete type, which is not what the driver
is. We therefore reject such a cycle, and likewise one naming a lifetime of its own, one whose members take an `impl
Trait` parameter, and one naming a `Self` the driver's signature cannot spell. A trait object is fine, since `&dyn
Trait` is a type the driver can name. Moving the function out to the enclosing scope removes the restriction, as it is
then a member like any other.

Options of the macro are scoped like bindings. Those the attribute itself was given hold throughout, and a
`#[stack_safe(..)]`
written on a function inside it *shadows* them for that function and whatever it contains. We recognise a nested
marker by name, i.e. `#[stack_safe]` as imported, or `yaspar_macros::stack_safe` written out. Note that the macro
might not be recognized under an alias, e.g. given `use yaspar_macros::stack_safe as ss`, using `#[ss]` in a nested
way is not recognized and is left for the compiler to expand on its own.

#### `&mut` parameters

A `&mut` parameter cannot travel in a frame. Every nested activation parks a frame carrying its own live locals, so at
depth `n` we would hold `n` frames, each with a `&mut` to the same object, which the borrow checker rightly rejects.

Such a parameter becomes part of a **context** instead, which the driver owns in a tuple and hands to the body as a
`&mut`
on each step. This is the context argument that was omitted from the expansion above. Since no reborrow outlives a
single
step, nothing is captured, and the parameter stays usable after a recursive call returns:

```rust
#[stack_safe]
fn collect(n: u64, out: &mut Vec<u64>) {
    if n == 0 { return; }
    out.push(n);
    collect(n / 2, out);
    collect(n / 3, out);
    out.push(n);           // `out` is still usable after both calls
}
```

A method is handled by desugaring `self` away: the receiver becomes an ordinary first parameter of a generated
associated function, and the method itself keeps its signature and forwards to it. Thus `&mut self` is a `&mut`
parameter,
governed by everything above, and `&self` is a shared one that simply rides in the payload.

Every recursive call must pass that same reference. If we recurse into a place *derived* from it, e.g.
`walk(&mut t.kids[i])` where the parent and the child are different nodes, then the driver has to keep the parent's
place
while lending out the child's, which is once again two live `&mut` into the same tree. This problem can be overcome by
casting mutable reference to pointers. This operation is explicitly acknowledged by passing the `use_nonlinear_mut` flag
to the macro.

```rust
#[stack_safe(use_nonlinear_mut)]
fn bump(t: &mut Tree) -> u64 {
    t.v += 1;
    let mut acc = t.v;
    for i in 0..t.kids.len() { acc += bump(&mut t.kids[i]); }
    acc
}
```

The frame then holds a raw pointer instead: the child's pointer is swapped in before the call, and the parent's is
restored
by the resume arm. The macro checks what it can see syntactically, i.e. that the argument is `&mut <place>` rooted at a
context parameter, so that `&mut some_local` is rejected. It cannot see types, however, so it becomes our obligation
that
the place stays valid while the subtree of the child runs, e.g. that it is a node reached from the context rather than
something that could be moved or freed in the meantime.

#### Lending the callee a value built at the call site

Sometimes a recursion grows its own argument, e.g. it pushes a node onto a borrowed chain on the way down:

```rust
#[stack_safe(data_in_frame)]
fn rec(n: usize, stack: &Stack<'_, Vec<usize>>) -> usize {
    if stack.len() >= n {
        n
    } else {
        let v = vec![];
        1 + rec(n, &Stack::Cons(v, stack))
    }
}
```

Natively the new node is a temporary of the caller, and the caller's frame outlives the call, so the callee can borrow
it.
The CPS transformation takes that frame away: a recursive call becomes a `Step::Call` handed back to the driver, so the
arm
that built the node has already returned before the callee's arm runs. Hence the flag, without which we get an error
saying
so rather than an `E0515` blamed on the attribute.

Under the flag the node lives in a store the driver owns. The callee is given an *address*, which has to be valid at two
moments the frame cannot cover: while the arm still runs, since the entry carrying it must be complete before the arm
returns; and for the whole subtree of the callee, which pushes frames of its own and so moves the `Vec` that holds them.
The store answers both. It exists before the arm runs, so pushing hands back an address at once, and its chunks are
pre-sized and never regrown, so no value ever moves. That costs one allocation per 64 values rather than one per value.

Each call site records the store's length (a mark) before pushing and truncates back to it on resume, carrying that mark
in its frame, so what a call lends its callee dies exactly with the callee's subtree. There is one store per argument
position, so a call may grow several arguments at once even when their types differ.

The two options compose, including at one call site. A recursion may hand its child a place derived from a `&mut`
parameter *and*, in the same argument list, a reference to a value built there. We park the slot for the child's
subtree and move the value into the driver's store, and the continuation then undoes both. `tests/context.rs` walks a
tree that way, i.e. it mutates each node through the derived reference while carrying the path from the root as a
chain built one link per level, and it is checked under both of Miri's aliasing models.

Both options hand the driver a raw pointer where the original had a reference. That does not put the borrow checker
aside: whatever the original asked of it is still checked, and a program it would have refused is still refused, e.g.
a callee that returns a borrow of a value lent to it. One consequence is worth knowing, since it is visible: a mistake
either option would otherwise have hidden may be reported twice, once against each of the two readings of the body.

#### Supports

Within a function body, the transformation handles `if`, `match` and blocks; `for`, `while`, `while let` and `loop`,
with
`break` and `continue`; `return` from any depth, and `?` on both a `Result` and an `Option`; `&mut` parameters, `&self`
and `&mut self` methods; generics and where-clauses; and any number of recursive call sites.

It preserves semantics as well as syntax: argument evaluation order, `&&` and `||` laziness, compound assignment, and
the
iterator expression of a loop being evaluated exactly once. Every value is dropped exactly once, with no leak and no
double
drop, even when a panic unwinds through parked frames. Each of these is checked in `tests/observable.rs` against a
hand-written twin of the same function.

#### Limitations

Drop *timing* shifts, because locals live in frames instead of on the native stack. The shift that can change what a
program means is the following one: a local that nothing after the call mentions is not carried in a frame at all, so it
is dropped *before* the call instead of after it.

```rust
struct Guard(u64);
impl Drop for Guard { fn drop(&mut self) { print!("leave{} ", self.0); } }

#[stack_safe]
fn walk(n: u64) {
    let _g = Guard(n);
    if n > 0 { walk(n - 1); }
}
```

For `walk(2)`, plain recursion prints `leave0 leave1 leave2`, i.e. innermost first, whereas the transformed version
prints
`leave2 leave1 leave0`. In other words, an RAII guard held across a recursive call does not protect that call. Deciding
otherwise would require knowing that the local implements `Drop`, i.e. would require types, so the remedy is to mention
the guard after the call, or to scope it in an inner block.

Four smaller shifts are recorded in `tests/observable.rs`, none of which changes *which* values are dropped, only when:a
carried local drops at the end of its resume arm, a temporary drops at the hoisted `let`, the iterator of a lowered loop
drops after the epilogue of the loop, and parked frames drop outermost-first when a panic unwinds.

Next come the cases where the macro silently leaves a recursive call as an ordinary call, so that the function compiles,
returns the right answer, and still overflows on a deep input:

* Since the proc-macros only see syntactic stream, a type alias is not expanded. If an alias hides a reference,
  e.g. `type Words<'a> = &'a [&'a str]`, an object of this type alias is not recognized as a reference, and the
  transformation therefore could fail with an obscure error message.
* mutual recursive functions that are not fully captured by a single `#[stack_safe]` will still overflow when input size
  is too large. This is because `#[stack_safe]` can only analyze code within its reach. Other functions are treated
  opaquely.
* a `fn` declared in a block *within* a body, e.g. inside an `if`, since only the statements of a body are taken as its
  definitions; declare it at the top level of the body instead.

Everything else is rejected at compile time, with the error on the offending span. On the signature:

* `async fn`, since the rewritten body is a loop over a frame stack, which an async state machine cannot hold without
  pinning; `const fn`, since the expansion allocates; and variadics;
* a by-value `self`, since the receiver becomes a parameter the driver either lends out or carries, and it can do
  neither
  with an owned value;
* a parameter that destructures, e.g. `f((a, b): (u64, u64))`, which has to be bound inside the body instead;
* a `mut` binding on a `&mut` parameter, e.g. `mut out: &mut Vec<u64>`, since that parameter becomes a context slot
  which
  every step re-derives, so reassigning the binding would not be visible.

On the placement of a recursive call:

* inside a closure, or inside a macro invocation, i.e. anywhere the macro cannot see how the call is reached. This is
  particularly
  the case for collection functions. Please use explicit loops instead;
* inside an `async { .. }` or `const { .. }` block, and any `.await` in the body;
* in a `let ... else` initializer, a match guard, an `if let` or `while let` scrutinee, a struct-update base, or the
  left-hand side of an assignment or of a compound assignment;
* in any other position it cannot be hoisted out of, e.g. an array-repeat expression, which asks for it to be bound to a
  `let` first;
* on a reference to a value built at the call site, e.g. `rec(n, &Node::Cons(v, rest))`, unless we opt in with
  `data_in_frame` — see below;
* inside the place passed for a context parameter, e.g. `f(&mut t.kids[f(..) as usize])`, since that place is taken as a
  pointer before the call is made, so the inner call would recurse natively;
* with the wrong number of arguments, which is reported as such rather than left to the type checker;
* naming a transformed function without calling it, e.g. `let g = f;` or `xs.iter().map(f)`, since the name no longer
  denotes something the driver can be entered at; wrap it in a closure that calls it;
* a labelled `break` or `continue` in a loop that contains a recursive call.

Inside a group:

* two members declaring a type of the same name in their bodies. Since recursive definitions are hoisted in a common
  state machine, internal type definitions with clashing names are also hoisted into the state machine, causing a name
  clashing. This can be addressed by using different names or centralize type definitions in a module;
* a member declared inside another member's body, where the cycle is generic or names a lifetime of its own. The
  driver carries those parameters, and such a function cannot name them. The same holds where a member takes an `impl
  Trait` parameter, or a `Self` the driver's signature cannot spell. Move the function out to the enclosing scope;
* a `default fn` in an impl block;
* a `#[stack_safe]` marker on a function the group finds no cycle for, which would otherwise silently do nothing.

On `?`:

* a carrier other than `Result` or `Option`, e.g. a type with a hand-written `Try` impl, since the early exit is
  desugared
  through a stand-in for the unstable `Try` and `FromResidual`, which has one impl per carrier; the error is a
  missing-impl one naming `yaspar_macros_defs::Try`, with a second naming `FromResidual` for the early-exit half.

Misusing the attribute is also an error: `#[stack_safe]` on an item that is neither a function, a module nor an impl
block; on a bodiless `mod m;`, which shows it no body to scan; and an unknown or malformed option list.

#### More Tests and Examples

| file                  | what it covers                                                                        |
|-----------------------|---------------------------------------------------------------------------------------|
| `tests/transform.rs`  | the core transform: branching, `?`, operators, strict positions, nested scopes, depth |
| `tests/loops.rs`      | `for` / `while` / `while let` / `loop`, nesting, `break` / `continue`                 |
| `tests/context.rs`    | `&mut` parameters, methods, `use_nonlinear_mut` (also the Miri target)                |
| `tests/group.rs`      | mutual recursion, nesting, member bodies, threading out, visibility                   |
| `tests/observable.rs` | side-effect and drop equivalence with plain recursion                                 |
| `tests/ui/`           | every rejection, with its message pinned against a stored `.stderr`                   |

We check the rejections with `trybuild`, which compares the compiler's output against a stored `.stderr` per case. A
`compile_fail` doctest cannot do that, since it passes however the message reads. After changing a message, regenerate
the snapshots with `TRYBUILD=overwrite cargo test --test ui` and read every diff.

Every stack-safety test runs on a thread with a 64 KiB stack, so that a regression to native recursion aborts the test
process instead of failing quietly. Two paths are `unsafe`, namely `use_nonlinear_mut` and `data_in_frame`, and both are
checked under both aliasing models. The depth-only tests are skipped there, since they are about frames rather than
aliasing and Miri would take forever over them:

```
cargo test
MIRIFLAGS="-Zmiri-strict-provenance" cargo +nightly miri test --test context \
    -- --skip deep_ --skip _is_flat --skip _is_stack_safe
MIRIFLAGS="-Zmiri-tree-borrows -Zmiri-strict-provenance" cargo +nightly miri test --test context \
    -- --skip deep_ --skip _is_flat --skip _is_stack_safe
MIRIFLAGS="-Zmiri-strict-provenance" cargo +nightly miri test --test transform \
    -- lends_the_callee two_values_of three_lent_values_with
MIRIFLAGS="-Zmiri-tree-borrows -Zmiri-strict-provenance" cargo +nightly miri test --test transform \
    -- lends_the_callee two_values_of three_lent_values_with
MIRIFLAGS="-Zmiri-strict-provenance" cargo +nightly miri test --test group \
    -- lend unsafe_options --skip _is_flat
MIRIFLAGS="-Zmiri-tree-borrows -Zmiri-strict-provenance" cargo +nightly miri test --test group \
    -- lend unsafe_options --skip _is_flat
```

The two examples are runnable programs rather than tests, since one of them deliberately aborts:

```
cargo run --release --example perf_contrast          # the benchmark quoted above
cargo run --example overflow_contrast -- safe        # 500001
cargo run --example overflow_contrast -- naive       # fatal runtime error: stack overflow
```

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
macro,
which we invoke inside the impl block and give one bare signature per method to forward:

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
started
with.

`#[delegate_trait]` requires neither. There is no macro to apply inside the impl block and no list of signatures,
because
the required methods come from the trait itself: whatever we do not write is delegated. Thus a new method in `Store`
needs
no change in `Doubling` at all. The price of that is the second attribute on the trait, which the next subsection
explains.

### Why Two Attributes

An attribute on an impl block sees only that impl block. It cannot know which methods `Store` requires, so it cannot
know
which ones are missing, and the trait may not even live in this crate. The signatures therefore have to travel from the
trait to the impl, and the only carrier a procedural macro can emit that a *later* expansion still sees is a
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
}                                       __delegate_impl_Store, inner, [put], Store);
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

Each forwarder is `#[inline]`, so the hop costs nothing. Note also that the call goes through the trait, as
`<_ as Store>::get(..)`, rather than through `self.inner.get(..)`: an inherent method of the same name on the field's
type would otherwise win the lookup and silently be called instead.

### Usage and Examples

`target` is a *field name*, not an expression, so we write `target = inner` and not `target = self.inner`. An empty impl
block delegates everything, which is how we obtain a newtype that behaves exactly like its field:

```rust
struct Wrapper {
    inner: Map
}

#[delegate_trait(target = inner)]
impl Store for Wrapper {}
```

A generic trait works too. Replaying a signature verbatim would emit `fn lookup(&self, k: K)` into
`impl Keyed<u32> for Wrap`, where `K` names nothing, so the helper macro carries the substitution instead: each of the
trait's parameters becomes a metavariable, and the impl passes its trait arguments positionally.

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
defaults. The impl block may override every method, some of them, or none.

### Limitations

Only required *methods* are delegated:

* a required associated type or associated const is not, so the impl block has to supply it as usual;
* a method with a default body is left to that default, and is delegated only if we override it ourselves.

The helper macro is addressed through the trait's own path: the trait emits an alias beside itself, and
`#[delegate_trait]`
swaps the last segment of the trait path for it. Thus a trait from another crate is delegated with nothing to import:

```rust
#[delegate_trait(target = inner)]
impl other_crate::a::Store for Wrapper {}
```

Naming the trait *bare*, after importing it, leaves no path to follow, and that form works only inside the crate that
defines the trait, where the helper's own `#[macro_export]`ed name is in scope. Writing the path is the fix.

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
the
private macro. A `macro_rules!` that is not `#[macro_export]`ed is crate-private, and `pub use` of one is itself
rejected
with `E0364`, so `pub(crate)` is as far as its alias can reach. That is also why the export cannot simply be dropped for
everyone.

Finally, the trait must carry `#[delegatable_trait]`, since the helper macro is where the signatures come from. A trait
we
cannot edit, e.g. `std::fmt::Write`, is therefore not delegatable, whereas one from another crate that carries the
attribute is.

### More Tests and Examples

`tests/delegate_trait.rs` covers partial and empty impl blocks, all receiver kinds, generic methods and where-clauses,
generic traits of each parameter kind including interleaved and defaulted ones, default-bodied methods, and the inherent
method that must not shadow the trait method.
