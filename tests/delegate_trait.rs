// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Behavioural tests for `#[delegatable_trait]` / `#[delegate_trait]`.
//!
//! What matters in each case is the *set* of methods the expansion emits: exactly
//! the required trait methods the impl block does not write, and nothing else. A
//! method emitted twice, or one silently dropped, is a compile error rather than a
//! test failure — so these tests compiling is already half the assertion.

use yaspar_macros::{delegatable_trait, delegate_trait};

#[delegatable_trait]
trait Greet {
    fn hello(&self) -> String;
    fn goodbye(&self) -> String;
}

struct Inner;

impl Greet for Inner {
    fn hello(&self) -> String {
        "hello from inner".into()
    }
    fn goodbye(&self) -> String {
        "goodbye from inner".into()
    }
}

struct Wrapper {
    inner: Inner,
}

// `hello` is overridden, `goodbye` is entirely absent -> delegated.
#[delegate_trait(target = inner)]
impl Greet for Wrapper {
    fn hello(&self) -> String {
        "hello from wrapper".into()
    }
}

#[test]
fn partial_impl_delegates_the_rest() {
    let w = Wrapper { inner: Inner };
    assert_eq!(w.hello(), "hello from wrapper");
    assert_eq!(w.goodbye(), "goodbye from inner");
}

// ---------------------------------------------------------------------------
// An empty impl block: every method is delegated.
// ---------------------------------------------------------------------------

#[delegatable_trait]
trait Math {
    fn add(&self, a: u32, b: u32) -> u32;
    fn mul(&self, a: u32, b: u32) -> u32;
}

struct MathImpl;

impl Math for MathImpl {
    fn add(&self, a: u32, b: u32) -> u32 {
        a + b
    }
    fn mul(&self, a: u32, b: u32) -> u32 {
        a * b
    }
}

struct MathWrapper {
    inner: MathImpl,
}

#[delegate_trait(target = inner)]
impl Math for MathWrapper {}

#[test]
fn empty_impl_delegates_everything() {
    let w = MathWrapper { inner: MathImpl };
    assert_eq!(w.add(2, 3), 5);
    assert_eq!(w.mul(4, 5), 20);
}

// ---------------------------------------------------------------------------
// Generics: the impl's own generics and bounds must survive re-emission.
// ---------------------------------------------------------------------------

#[delegatable_trait]
trait Len {
    fn len(&self) -> usize;
    fn is_empty(&self) -> bool;
}

struct GenericWrapper<T: Len> {
    inner: T,
}

// Only `is_empty` is overridden; `len` is delegated.
#[delegate_trait(target = inner)]
impl<T: Len> Len for GenericWrapper<T> {
    fn is_empty(&self) -> bool {
        self.inner.len() > 10
    }
}

struct MyVec(Vec<u8>);

impl Len for MyVec {
    fn len(&self) -> usize {
        self.0.len()
    }
    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

#[test]
fn generic_impl_delegates() {
    let w = GenericWrapper {
        inner: MyVec(vec![1, 2, 3]),
    };
    assert_eq!(w.len(), 3);
    assert!(!w.is_empty());
}

// ---------------------------------------------------------------------------
// A method with a default body is not delegated: it keeps the default, which is
// the trait author's decision, not the wrapper's.
// ---------------------------------------------------------------------------

#[delegatable_trait]
trait WithDefault {
    fn required(&self) -> u32;
    fn optional(&self) -> u32 {
        1 + self.required()
    }
}

struct DefaultInner;

impl WithDefault for DefaultInner {
    fn required(&self) -> u32 {
        42
    }
    fn optional(&self) -> u32 {
        200
    }
}

struct DefaultWrapper {
    inner: DefaultInner,
}

#[delegate_trait(target = inner)]
impl WithDefault for DefaultWrapper {
    fn optional(&self) -> u32 {
        100
    }
}

struct DefaultWrapper2 {
    inner: DefaultInner,
}

#[delegate_trait(target = inner)]
impl WithDefault for DefaultWrapper2 {}

#[test]
fn default_methods_are_not_delegated() {
    let w = DefaultWrapper {
        inner: DefaultInner,
    };
    // `required` is delegated -> inner's 42.
    assert_eq!(w.required(), 42);
    // `optional` is written in the impl block -> 100, *not* inner's 200.
    assert_eq!(w.optional(), 100);

    // Empty impl block: `required` is delegated, `optional` keeps the trait's
    // default, so it sees the *wrapper's* `required`.
    let w2 = DefaultWrapper2 {
        inner: DefaultInner,
    };
    assert_eq!(w2.required(), 42);
    assert_eq!(w2.optional(), 43);
}

// ---------------------------------------------------------------------------
// Receivers other than `&self`, and by-value arguments.
// ---------------------------------------------------------------------------

#[delegatable_trait]
trait Counter {
    fn get(&self) -> u32;
    fn bump(&mut self, by: u32);
    fn into_total(self) -> u32;
}

struct CounterImpl {
    n: u32,
}

impl Counter for CounterImpl {
    fn get(&self) -> u32 {
        self.n
    }
    fn bump(&mut self, by: u32) {
        self.n += by;
    }
    fn into_total(self) -> u32 {
        self.n
    }
}

struct CounterWrapper {
    inner: CounterImpl,
}

#[delegate_trait(target = inner)]
impl Counter for CounterWrapper {}

#[test]
fn all_receiver_kinds_delegate() {
    let mut w = CounterWrapper {
        inner: CounterImpl { n: 1 },
    };
    w.bump(4);
    assert_eq!(w.get(), 5);
    assert_eq!(w.into_total(), 5);
}

// ---------------------------------------------------------------------------
// A required method with no `self` receiver cannot be forwarded — there is no
// `self` to read the field out of — so it has to be written in the impl block.
// Writing it there is enough: the skip list takes it out of the delegation, and
// the methods beside it are delegated as usual. (The other half of this, an
// *unwritten* receiverless method, is rejected by name; see
// `tests/ui/delegate_receiverless_method.rs`.)
// ---------------------------------------------------------------------------

#[delegatable_trait]
trait Cfg {
    fn version() -> u32;
    fn label(&self) -> &'static str;
}

struct CfgInner;

impl Cfg for CfgInner {
    fn version() -> u32 {
        1
    }
    fn label(&self) -> &'static str {
        "inner"
    }
}

struct CfgWrapper {
    inner: CfgInner,
}

#[delegate_trait(target = inner)]
impl Cfg for CfgWrapper {
    // Written by hand, since there is nothing to forward it to.
    fn version() -> u32 {
        2
    }
}

#[test]
fn receiverless_method_written_by_hand_coexists_with_delegation() {
    let w = CfgWrapper { inner: CfgInner };
    assert_eq!(<CfgWrapper as Cfg>::version(), 2);
    assert_eq!(w.label(), "inner");
}

// ---------------------------------------------------------------------------
// A method's attributes travel with its signature. This matters most for `#[cfg]`:
// an attribute macro runs before `cfg` stripping, so `never` below is recorded like
// any other method, and re-emitting it without its attribute would put it in the
// impl block of every wrapper — where the trait, which *was* stripped, does not have
// it (`E0407`).
// ---------------------------------------------------------------------------

#[delegatable_trait]
trait Gated {
    /// A doc comment is an attribute too, and rides along to the forwarder.
    fn always(&self) -> u32;
    #[cfg(any())]
    fn never(&self) -> u32;
    #[cfg(not(any()))]
    fn present(&self) -> u32;
}

struct GatedInner;

impl Gated for GatedInner {
    fn always(&self) -> u32 {
        1
    }
    fn present(&self) -> u32 {
        2
    }
}

struct GatedWrapper {
    inner: GatedInner,
}

#[delegate_trait(target = inner)]
impl Gated for GatedWrapper {}

#[test]
fn cfg_gated_trait_methods_are_gated_in_the_impl_too() {
    let w = GatedWrapper { inner: GatedInner };
    assert_eq!(w.always(), 1);
    // `#[cfg(not(any()))]` holds, so this one is delegated; `never` is absent from both
    // the trait and the impl, which is what lets this file compile at all.
    assert_eq!(w.present(), 2);
}

// ---------------------------------------------------------------------------
// An argument written as `_`, which is an ordinary thing to find in a trait
// declaration and is *not* an expression: replaying the pattern as the forwarded
// call's argument is "error: in expressions, `_` can only be used on the left-hand
// side of an assignment". Each argument is renamed to a fresh binding instead.
// ---------------------------------------------------------------------------

#[delegatable_trait]
trait Ignoring {
    fn discard(&self, _: u32, kept: u32) -> u32;
}

struct IgnoringInner;

impl Ignoring for IgnoringInner {
    fn discard(&self, _: u32, kept: u32) -> u32 {
        kept + 1
    }
}

struct IgnoringWrapper {
    inner: IgnoringInner,
}

#[delegate_trait(target = inner)]
impl Ignoring for IgnoringWrapper {}

#[test]
fn wildcard_argument_patterns_are_forwarded_positionally() {
    let w = IgnoringWrapper {
        inner: IgnoringInner,
    };
    // The second argument has to arrive as the second argument, so a rename that
    // lost the order would show up here rather than only at compile time.
    assert_eq!(w.discard(99, 7), 8);
}

// ---------------------------------------------------------------------------
// `unsafe fn`. A function body is not an unsafe block of its own, so the forwarding
// call has to be wrapped: otherwise the expansion warns under
// `unsafe_op_in_unsafe_fn`, at a span inside the generated macro where nobody can
// silence it — and a warning is an error under `#![deny(warnings)]`. `cargo clippy
// --all-targets -- -D warnings` is what holds this: the warning would be emitted
// while compiling this very file.
// ---------------------------------------------------------------------------

#[delegatable_trait]
trait Raw {
    unsafe fn read_at(&self, p: *const u32) -> u32;
}

struct RawInner;

impl Raw for RawInner {
    unsafe fn read_at(&self, p: *const u32) -> u32 {
        unsafe { *p }
    }
}

struct RawWrapper {
    inner: RawInner,
}

#[delegate_trait(target = inner)]
impl Raw for RawWrapper {}

#[test]
fn unsafe_methods_delegate() {
    let w = RawWrapper { inner: RawInner };
    let n = 42u32;
    assert_eq!(unsafe { w.read_at(&raw const n) }, 42);
}

// ---------------------------------------------------------------------------
// The trait method is reached through the trait, not through an inherent method
// of the same name on the field's type.
// ---------------------------------------------------------------------------

#[delegatable_trait]
trait Named {
    fn name(&self) -> &'static str;
}

struct Shadowed;

impl Shadowed {
    #[allow(dead_code)]
    fn name(&self) -> &'static str {
        "inherent"
    }
}

impl Named for Shadowed {
    fn name(&self) -> &'static str {
        "trait"
    }
}

struct ShadowWrapper {
    inner: Shadowed,
}

#[delegate_trait(target = inner)]
impl Named for ShadowWrapper {}

#[test]
fn inherent_method_does_not_shadow_the_trait_method() {
    let w = ShadowWrapper { inner: Shadowed };
    assert_eq!(w.name(), "trait");
}

// ---------------------------------------------------------------------------
// What `target` may name. It is a *field path*, so a tuple index — the newtype,
// which is the shape the documentation leads with — and a nested field are as good
// as a bare name. `target = self.inner` is still refused, since `target` is a place
// and not an expression; see `tests/ui/delegate_target_is_expression.rs`.
// ---------------------------------------------------------------------------

#[delegatable_trait]
trait Reader {
    fn read(&self) -> u32;
    fn also(&self) -> u32;
}

struct ReaderInner(u32);

impl Reader for ReaderInner {
    fn read(&self) -> u32 {
        self.0
    }
    fn also(&self) -> u32 {
        self.0 + 1
    }
}

/// A newtype: the field is `0`, which is not an identifier at all.
struct NewTypeWrapper(ReaderInner);

#[delegate_trait(target = 0)]
impl Reader for NewTypeWrapper {}

struct Middle {
    deep: ReaderInner,
}

/// A nested field, reached by a dotted path.
struct NestedWrapper {
    middle: Middle,
}

#[delegate_trait(target = middle.deep)]
impl Reader for NestedWrapper {
    fn also(&self) -> u32 {
        999
    }
}

#[test]
fn target_may_be_a_tuple_index_or_a_nested_field() {
    let n = NewTypeWrapper(ReaderInner(5));
    assert_eq!(n.read(), 5);
    assert_eq!(n.also(), 6);

    let d = NestedWrapper {
        middle: Middle {
            deep: ReaderInner(10),
        },
    };
    assert_eq!(d.read(), 10);
    assert_eq!(d.also(), 999);
}

// ---------------------------------------------------------------------------
// Generic methods and `where` clauses on the trait's own methods, as distinct from
// the trait's own parameters, which are covered further down.
// ---------------------------------------------------------------------------

#[delegatable_trait]
trait Store {
    fn get(&self, k: u32) -> u64;
    fn put<V: Into<u64>>(&mut self, v: V);
    fn count(&self) -> usize
    where
        Self: Sized;
}

struct StoreImpl(u64);

impl Store for StoreImpl {
    fn get(&self, k: u32) -> u64 {
        self.0 + k as u64
    }
    fn put<V: Into<u64>>(&mut self, v: V) {
        self.0 = v.into();
    }
    fn count(&self) -> usize
    where
        Self: Sized,
    {
        1
    }
}

struct StoreWrapper {
    inner: StoreImpl,
}

#[delegate_trait(target = inner)]
impl Store for StoreWrapper {}

#[test]
fn generic_methods_and_where_clauses_delegate() {
    let mut w = StoreWrapper {
        inner: StoreImpl(1),
    };
    assert_eq!(w.get(2), 3);
    w.put(5u32);
    assert_eq!(w.get(0), 5);
    assert_eq!(w.count(), 1);
}

// ---------------------------------------------------------------------------
// Generic traits. The trait's parameters have no binding at the impl site, so the
// helper macro records them as metavariables and the impl passes its arguments.
// ---------------------------------------------------------------------------

#[delegatable_trait]
trait Keyed<K> {
    fn get(&self, k: K) -> u64;
    fn put(&mut self, k: K, v: u64);
}

struct KeyedImpl(u64);

impl Keyed<u32> for KeyedImpl {
    fn get(&self, k: u32) -> u64 {
        self.0 + k as u64
    }
    fn put(&mut self, _k: u32, v: u64) {
        self.0 = v;
    }
}

struct KeyedWrapper {
    inner: KeyedImpl,
}

#[delegate_trait(target = inner)]
impl Keyed<u32> for KeyedWrapper {}

/// The argument may itself be one of the *impl's* parameters.
struct KeyedGeneric<T> {
    inner: T,
}

#[delegate_trait(target = inner)]
impl<T: Keyed<u32>> Keyed<u32> for KeyedGeneric<T> {}

#[test]
fn generic_trait_delegates() {
    let mut w = KeyedWrapper {
        inner: KeyedImpl(1),
    };
    assert_eq!(w.get(2), 3);
    w.put(0, 5);
    assert_eq!(w.get(2), 7);

    let g = KeyedGeneric {
        inner: KeyedImpl(10),
    };
    assert_eq!(g.get(5), 15);
}

/// Two parameters, with one method overridden: the substitution has to keep them
/// in the right order.
#[delegatable_trait]
trait Pair<A, B> {
    fn left(&self, a: A) -> u64;
    fn right(&self, b: B) -> u64;
}

struct PairImpl;

impl Pair<u8, u16> for PairImpl {
    fn left(&self, a: u8) -> u64 {
        a as u64
    }
    fn right(&self, b: u16) -> u64 {
        b as u64
    }
}

struct PairWrapper {
    inner: PairImpl,
}

#[delegate_trait(target = inner)]
impl Pair<u8, u16> for PairWrapper {
    fn left(&self, _a: u8) -> u64 {
        999
    }
}

#[test]
fn two_parameter_trait_delegates() {
    let w = PairWrapper { inner: PairImpl };
    assert_eq!(w.left(1), 999);
    assert_eq!(w.right(7), 7);
}

/// A lifetime parameter. Lifetimes are two tokens, and they come before the type
/// arguments in the positional list.
#[delegatable_trait]
trait Peek<'a> {
    fn peek(&self, s: &'a str) -> usize;
}

struct PeekImpl;

impl<'a> Peek<'a> for PeekImpl {
    fn peek(&self, s: &'a str) -> usize {
        s.len()
    }
}

struct PeekWrapper {
    inner: PeekImpl,
}

#[delegate_trait(target = inner)]
impl<'a> Peek<'a> for PeekWrapper {}

#[test]
fn lifetime_parameter_trait_delegates() {
    let w = PeekWrapper { inner: PeekImpl };
    assert_eq!(w.peek("hello"), 5);
}

/// A defaulted parameter, which the impl may leave out. Only the trait knows the
/// default, so the trait emits the arm that fills it in.
#[delegatable_trait]
trait Defaulted<K = u32> {
    fn get(&self, k: K) -> u64;
    fn twice(&self, k: K) -> u64;
}

struct DefaultedImpl;

impl Defaulted for DefaultedImpl {
    fn get(&self, k: u32) -> u64 {
        k as u64
    }
    fn twice(&self, k: u32) -> u64 {
        2 * k as u64
    }
}

struct OmittedWrapper {
    inner: DefaultedImpl,
}

#[delegate_trait(target = inner)]
impl Defaulted for OmittedWrapper {}

struct SpelledOutImpl;

impl Defaulted<u8> for SpelledOutImpl {
    fn get(&self, k: u8) -> u64 {
        100 + k as u64
    }
    fn twice(&self, k: u8) -> u64 {
        200 + k as u64
    }
}

struct SpelledOutWrapper {
    inner: SpelledOutImpl,
}

#[delegate_trait(target = inner)]
impl Defaulted<u8> for SpelledOutWrapper {
    fn get(&self, _k: u8) -> u64 {
        7
    }
}

#[test]
fn defaulted_parameter_delegates_either_way() {
    let w = OmittedWrapper {
        inner: DefaultedImpl,
    };
    assert_eq!(w.get(3), 3);
    assert_eq!(w.twice(3), 6);

    let s = SpelledOutWrapper {
        inner: SpelledOutImpl,
    };
    assert_eq!(s.get(1), 7);
    assert_eq!(s.twice(1), 201);
}

/// Const parameters. They travel as `expr` fragments and every use is braced, which
/// is what lets an expression stand where a const argument is expected.
struct Holder<const N: usize>([u8; N]);

#[delegatable_trait]
trait Buf<const N: usize> {
    fn read(&self) -> [u8; N];
    fn wrap(&self) -> Holder<N>;
    fn size(&self) -> usize;
}

struct BufImpl;

impl Buf<4> for BufImpl {
    fn read(&self) -> [u8; 4] {
        [1, 2, 3, 4]
    }
    fn wrap(&self) -> Holder<4> {
        Holder([9; 4])
    }
    fn size(&self) -> usize {
        4
    }
}

struct BufWrapper {
    inner: BufImpl,
}

#[delegate_trait(target = inner)]
impl Buf<4> for BufWrapper {
    fn size(&self) -> usize {
        999
    }
}

#[test]
fn const_parameter_trait_delegates() {
    let w = BufWrapper { inner: BufImpl };
    assert_eq!(w.read(), [1, 2, 3, 4]);
    assert_eq!(w.wrap().0, [9, 9, 9, 9]);
    assert_eq!(w.size(), 999);
}

/// A const parameter interleaved with a type parameter, plus a defaulted one: the
/// arguments are matched positionally, so declaration order has to be preserved
/// across the kinds.
#[delegatable_trait]
trait Mixed<T, const N: usize, U = i8> {
    fn t(&self, t: T) -> u64;
    fn n(&self) -> [u8; N];
    fn u(&self, u: U) -> i64;
}

struct MixedImpl;

impl Mixed<u8, 2> for MixedImpl {
    fn t(&self, t: u8) -> u64 {
        t as u64
    }
    fn n(&self) -> [u8; 2] {
        [7, 7]
    }
    fn u(&self, u: i8) -> i64 {
        u as i64
    }
}

struct MixedWrapper {
    inner: MixedImpl,
}

#[delegate_trait(target = inner)]
impl Mixed<u8, 2> for MixedWrapper {}

#[test]
fn interleaved_kinds_delegate() {
    let w = MixedWrapper { inner: MixedImpl };
    assert_eq!(w.t(3), 3);
    assert_eq!(w.n(), [7, 7]);
    assert_eq!(w.u(-1), -1);
}

/// A default that *mentions* an earlier parameter. The default is the trait's own
/// text, so `A` in it names the trait's parameter and nothing at the impl site: it
/// needs the same rewrite a signature gets, into the metavariable the arm has just
/// bound. Emitted verbatim it is `E0425 cannot find type A in this scope`, pointing
/// into the trait.
#[delegatable_trait]
trait Chain<A, B = Vec<A>> {
    fn one(&self, a: A) -> u64;
    fn many(&self, b: B) -> u64;
}

struct ChainImpl;

impl Chain<u8> for ChainImpl {
    fn one(&self, a: u8) -> u64 {
        a as u64
    }
    fn many(&self, b: Vec<u8>) -> u64 {
        b.len() as u64
    }
}

struct ChainWrapper {
    inner: ChainImpl,
}

// `B` is left out, so the trait fills in `Vec<A>` — with `A` already substituted.
#[delegate_trait(target = inner)]
impl Chain<u8> for ChainWrapper {}

#[test]
fn a_default_naming_an_earlier_parameter_delegates() {
    let w = ChainWrapper { inner: ChainImpl };
    assert_eq!(w.one(3), 3);
    assert_eq!(w.many(vec![1, 2, 3]), 3);
}

/// The same, with a *const* parameter inside the default: `[u8; N]` has to come out
/// as `[u8; { $__dt_ct_N }]`, since the const travels as an `expr` fragment and only
/// braces let one stand where a const argument is expected.
#[delegatable_trait]
trait Sized2<const N: usize, T = [u8; N]> {
    fn count(&self) -> usize;
    fn buf(&self, t: T) -> usize;
}

struct Sized2Impl;

impl Sized2<3> for Sized2Impl {
    fn count(&self) -> usize {
        3
    }
    fn buf(&self, t: [u8; 3]) -> usize {
        t.iter().map(|b| *b as usize).sum()
    }
}

struct Sized2Wrapper {
    inner: Sized2Impl,
}

#[delegate_trait(target = inner)]
impl Sized2<3> for Sized2Wrapper {}

#[test]
fn a_default_naming_a_const_parameter_delegates() {
    let w = Sized2Wrapper { inner: Sized2Impl };
    assert_eq!(w.count(), 3);
    assert_eq!(w.buf([1, 2, 3]), 6);
}

/// A trait parameter whose name is also an associated type's, beside a binding of
/// that associated type. `Item` is an ordinary name for a parameter, and
/// `Iterator<Item = u8>` is an ordinary thing to return; substituting by token
/// makes the pair into `Iterator<u32 = u8>`, an `E0220` blamed on the trait. The
/// binding's name is not a type, so a substitution that knows the difference leaves
/// it alone.
#[delegatable_trait]
trait Feed<Item> {
    fn one(&self) -> Item;
    fn many(&self) -> Box<dyn Iterator<Item = u8>>;
    /// A binding in argument position, and one nested under a second parameter name.
    fn count(&self, it: Box<dyn Iterator<Item = u8>>) -> usize;
}

struct FeedImpl;

impl Feed<u32> for FeedImpl {
    fn one(&self) -> u32 {
        7
    }
    fn many(&self) -> Box<dyn Iterator<Item = u8>> {
        Box::new([1u8, 2, 3].into_iter())
    }
    fn count(&self, it: Box<dyn Iterator<Item = u8>>) -> usize {
        it.count()
    }
}

struct FeedWrapper {
    inner: FeedImpl,
}

#[delegate_trait(target = inner)]
impl Feed<u32> for FeedWrapper {}

#[test]
fn a_parameter_named_like_an_associated_type_delegates() {
    let w = FeedWrapper { inner: FeedImpl };
    assert_eq!(w.one(), 7);
    assert_eq!(w.many().sum::<u8>(), 6);
    assert_eq!(w.count(Box::new([4u8, 5].into_iter())), 2);
}

// ---------------------------------------------------------------------------
// `local`: two traits of the same name in one crate. The default form exports the
// helper macro to the crate root, where the second one would collide with `E0428`;
// `local` keeps each helper beside its own trait and reaches it by path instead.
// ---------------------------------------------------------------------------

mod first {
    #[yaspar_macros::delegatable_trait(local)]
    pub trait Named {
        fn value(&self) -> u64;
        fn which(&self) -> &'static str;
    }

    pub struct Base;

    impl Named for Base {
        fn value(&self) -> u64 {
            1
        }
        fn which(&self) -> &'static str {
            "first"
        }
    }
}

mod second {
    #[yaspar_macros::delegatable_trait(local)]
    pub trait Named {
        fn value(&self) -> u64;
        fn which(&self) -> &'static str;
    }

    pub struct Base;

    impl Named for Base {
        fn value(&self) -> u64 {
            2
        }
        fn which(&self) -> &'static str {
            "second"
        }
    }
}

struct BothWrapper {
    a: first::Base,
    b: second::Base,
}

// Delegated from a third module, i.e. the helper is reached through the trait's path.
#[delegate_trait(target = a)]
impl first::Named for BothWrapper {
    // An override still works alongside the delegation.
    fn which(&self) -> &'static str {
        "wrapper"
    }
}

#[delegate_trait(target = b)]
impl second::Named for BothWrapper {}

#[test]
fn same_trait_name_in_one_crate_delegates_under_local() {
    let w = BothWrapper {
        a: first::Base,
        b: second::Base,
    };
    assert_eq!(first::Named::value(&w), 1);
    assert_eq!(first::Named::which(&w), "wrapper");
    assert_eq!(second::Named::value(&w), 2);
    assert_eq!(second::Named::which(&w), "second");
}

// A `local` trait delegated from within its own module, by bare name.
mod inside {
    #[yaspar_macros::delegatable_trait(local)]
    pub trait Bare {
        fn v(&self) -> u64;
    }

    pub struct Base;

    impl Bare for Base {
        fn v(&self) -> u64 {
            9
        }
    }

    pub struct Wrap {
        pub inner: Base,
    }

    #[yaspar_macros::delegate_trait(target = inner)]
    impl Bare for Wrap {}
}

#[test]
fn local_helper_is_reachable_by_bare_name_in_its_own_module() {
    use inside::Bare;
    assert_eq!(
        inside::Wrap {
            inner: inside::Base
        }
        .v(),
        9
    );
}

// ---------------------------------------------------------------------------
// A default-mode trait addressed by path from outside its module. This is the
// mechanism a dependent crate uses, where `impl libx::a::Store for W` reaches the
// helper as `libx::a::__delegate_path_Store` with nothing to import. The crate
// boundary itself cannot be tested from inside one crate.
// ---------------------------------------------------------------------------

mod exported {
    #[yaspar_macros::delegatable_trait]
    pub trait Reachable {
        fn one(&self) -> u64;
        fn two(&self) -> u64;
    }

    pub struct Base;

    impl Reachable for Base {
        fn one(&self) -> u64 {
            1
        }
        fn two(&self) -> u64 {
            2
        }
    }
}

struct ByPath {
    inner: exported::Base,
}

// The trait is named by path, so the helper is reached through that path's prefix.
#[delegate_trait(target = inner)]
impl exported::Reachable for ByPath {
    fn two(&self) -> u64 {
        22
    }
}

#[test]
fn helper_is_reachable_through_the_trait_path() {
    let w = ByPath {
        inner: exported::Base,
    };
    assert_eq!(exported::Reachable::one(&w), 1);
    assert_eq!(exported::Reachable::two(&w), 22);
}
