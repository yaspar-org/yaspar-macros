// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Differential tests: every case runs the `#[stack_safe]` transform against ordinary recursion.
//!
//! Two kinds, and both are in the regular suite — nothing here is ignored. A case named
//! `agrees_with_native` asserts the two compute the same thing; those are the shapes adversarial
//! testing found and the transform now handles. A case named `diverges_from_native` pins *both*
//! answers, because the two genuinely differ: they are the documented limitations, shapes the
//! transform cannot tell apart from ones it handles correctly, since telling them apart needs a type
//! and a proc macro has none. README names each and its workaround. Pinning rather than ignoring
//! means a fix fails here and says so, and that no further drift goes unnoticed.
//!
//! Shapes that *can* be recognised are rejected by the macro instead, and live in `tests/ui/`. Some
//! cases below exist to keep those rejections honest: they are the idioms a broader check would have
//! refused, and they have to keep compiling.
//!
//! None of these tests relies on destructor or `Drop` call ordering.

// These are minimised reproductions, and the shapes *are* the point: a one-iteration loop, a
// spelled-out deref, a function type written out, a shadowing `let`. Clippy's advice would change
// what each one reproduces.
#![allow(dead_code, unused_imports)]
#![allow(
    clippy::never_loop,
    clippy::type_complexity,
    clippy::explicit_auto_deref,
    clippy::let_and_return
)]

use yaspar_macros::stack_safe;

mod finding_02_callable_evaluation_order {
    use super::stack_safe;
    use std::cell::Cell;

    fn add_one(v: u64) -> u64 {
        v + 1
    }

    fn double(v: u64) -> u64 {
        v * 2
    }

    fn choose(calls: &Cell<u64>) -> fn(u64) -> u64 {
        let n = calls.get();
        calls.set(n + 1);
        if n.is_multiple_of(2) { add_one } else { double }
    }

    #[stack_safe]
    fn transformed(n: u64, calls: &Cell<u64>) -> u64 {
        if n == 0 {
            1
        } else {
            choose(calls)(transformed(n - 1, calls))
        }
    }

    fn native(n: u64, calls: &Cell<u64>) -> u64 {
        if n == 0 {
            1
        } else {
            choose(calls)(native(n - 1, calls))
        }
    }

    #[test]
    fn agrees_with_native() {
        assert_eq!(transformed(2, &Cell::new(0)), native(2, &Cell::new(0)));
    }
}

mod finding_03_by_value_place_receiver {
    use super::stack_safe;

    #[derive(Clone, Copy)]
    struct Value(u64);

    impl Value {
        fn plus(self, rhs: u64) -> u64 {
            self.0 + rhs
        }
    }

    #[stack_safe]
    fn transformed(n: u64, value: &mut [Value; 1]) -> u64 {
        if n == 0 {
            value[0].0 = 10;
            1
        } else {
            value[0].plus(transformed(n - 1, value))
        }
    }

    fn native(n: u64, value: &mut [Value; 1]) -> u64 {
        if n == 0 {
            value[0].0 = 10;
            1
        } else {
            value[0].plus(native(n - 1, value))
        }
    }

    #[test]
    fn diverges_from_native() {
        // `plus` takes `self` by value, so Rust copies out of `value[0]` before the argument
        // runs; the transform reads the place afterwards and sees what the recursion wrote.
        assert_eq!(native(2, &mut [Value(0)]), 1);
        assert_eq!(transformed(2, &mut [Value(0)]), 21);
    }
}

mod finding_04_raw_format_capture {
    use super::stack_safe;

    #[stack_safe]
    fn transformed(r#type: u64, n: u64) -> String {
        if n == 0 {
            String::new()
        } else {
            let below = transformed(r#type + 1, n - 1);
            format!("{below}{type},")
        }
    }

    fn native(r#type: u64, n: u64) -> String {
        if n == 0 {
            String::new()
        } else {
            let below = native(r#type + 1, n - 1);
            format!("{below}{type},")
        }
    }

    #[test]
    fn agrees_with_native() {
        assert_eq!(transformed(10, 3), native(10, 3));
    }
}

mod finding_06_unrelated_same_named_method {
    use super::stack_safe;
    use std::ops::Deref;

    struct NativeTarget;
    struct NativeOther;

    static NATIVE_TARGET: NativeTarget = NativeTarget;
    static NATIVE_OTHER: NativeOther = NativeOther;

    impl Deref for NativeOther {
        type Target = NativeTarget;

        fn deref(&self) -> &Self::Target {
            &NATIVE_TARGET
        }
    }

    impl NativeOther {
        fn partner(&self, _: u64) -> u64 {
            100
        }
    }

    impl NativeTarget {
        fn recurse(&self, n: u64) -> u64 {
            if n == 0 {
                NATIVE_OTHER.partner(0)
            } else {
                self.partner(n - 1) + 1
            }
        }

        fn partner(&self, n: u64) -> u64 {
            if n == 0 { 0 } else { self.recurse(n - 1) + 10 }
        }
    }

    struct TransformedTarget;
    struct TransformedOther;

    static TRANSFORMED_TARGET: TransformedTarget = TransformedTarget;
    static TRANSFORMED_OTHER: TransformedOther = TransformedOther;

    impl Deref for TransformedOther {
        type Target = TransformedTarget;

        fn deref(&self) -> &Self::Target {
            &TRANSFORMED_TARGET
        }
    }

    impl TransformedOther {
        fn partner(&self, _: u64) -> u64 {
            100
        }
    }

    #[stack_safe]
    impl TransformedTarget {
        fn recurse(&self, n: u64) -> u64 {
            if n == 0 {
                TRANSFORMED_OTHER.partner(0)
            } else {
                self.partner(n - 1) + 1
            }
        }

        fn partner(&self, n: u64) -> u64 {
            if n == 0 { 0 } else { self.recurse(n - 1) + 10 }
        }
    }

    #[test]
    fn diverges_from_native() {
        // `OTHER.partner(0)` is a method of an unrelated type reached by `Deref`, and the
        // transform reads every `.partner(..)` in the impl as a call into the cycle.
        assert_eq!(NATIVE_TARGET.recurse(0), 100);
        assert_eq!(TRANSFORMED_TARGET.recurse(0), 0);
    }
}

mod finding_08_borrowed_receiver_autoderef {
    use super::stack_safe;
    use std::cell::Cell;
    use std::ops::Deref;

    struct Value(u64);

    impl Value {
        fn plus(&self, rhs: u64) -> u64 {
            self.0 + rhs
        }
    }

    struct Switch<'a> {
        changed: &'a Cell<bool>,
        before: Value,
        after: Value,
    }

    impl Deref for Switch<'_> {
        type Target = Value;

        fn deref(&self) -> &Self::Target {
            if self.changed.get() {
                &self.after
            } else {
                &self.before
            }
        }
    }

    #[stack_safe]
    fn transformed(n: u64, switch: &Switch<'_>) -> u64 {
        if n == 0 {
            switch.changed.set(true);
            1
        } else {
            switch.plus(transformed(n - 1, switch))
        }
    }

    fn native(n: u64, switch: &Switch<'_>) -> u64 {
        if n == 0 {
            switch.changed.set(true);
            1
        } else {
            switch.plus(native(n - 1, switch))
        }
    }

    fn run(f: fn(u64, &Switch<'_>) -> u64) -> u64 {
        let changed = Cell::new(false);
        let switch = Switch {
            changed: &changed,
            before: Value(10),
            after: Value(100),
        };
        f(1, &switch)
    }

    #[test]
    fn diverges_from_native() {
        // The receiver's `Deref` runs after the recursion, which has flipped which `Value` it
        // yields.
        assert_eq!(run(native), 11);
        assert_eq!(run(transformed), 101);
    }
}

mod finding_09_argument_deref_coercion {
    use super::stack_safe;
    use std::cell::Cell;
    use std::ops::Deref;

    struct Value(u64);

    struct Switch<'a> {
        changed: &'a Cell<bool>,
        before: Value,
        after: Value,
    }

    impl Deref for Switch<'_> {
        type Target = Value;

        fn deref(&self) -> &Self::Target {
            if self.changed.get() {
                &self.after
            } else {
                &self.before
            }
        }
    }

    fn consume(value: &Value, rhs: u64) -> u64 {
        value.0 + rhs
    }

    #[stack_safe]
    fn transformed(n: u64, switch: &Switch<'_>) -> u64 {
        if n == 0 {
            switch.changed.set(true);
            1
        } else {
            consume(switch, transformed(n - 1, switch))
        }
    }

    fn native(n: u64, switch: &Switch<'_>) -> u64 {
        if n == 0 {
            switch.changed.set(true);
            1
        } else {
            consume(switch, native(n - 1, switch))
        }
    }

    fn run(f: fn(u64, &Switch<'_>) -> u64) -> u64 {
        let changed = Cell::new(false);
        let switch = Switch {
            changed: &changed,
            before: Value(10),
            after: Value(100),
        };
        f(1, &switch)
    }

    #[test]
    fn diverges_from_native() {
        // The first argument is hoisted untyped to keep the evaluation order, so its `&Switch`
        // to `&Value` coercion happens in the reconstructed call rather than before the recursion.
        assert_eq!(run(native), 11);
        assert_eq!(run(transformed), 101);
    }
}

mod finding_10_for_track_caller {
    use super::stack_safe;
    use std::cell::Cell;
    use std::panic::Location;

    struct Probe<'a> {
        into_seen: &'a Cell<bool>,
        next_seen: &'a Cell<bool>,
        expected: u32,
    }

    struct ProbeIter<'a> {
        seen: &'a Cell<bool>,
        expected: u32,
        yielded: bool,
    }

    impl Iterator for ProbeIter<'_> {
        type Item = ();

        #[track_caller]
        fn next(&mut self) -> Option<Self::Item> {
            self.seen.set(Location::caller().line() == self.expected);
            if self.yielded {
                None
            } else {
                self.yielded = true;
                Some(())
            }
        }
    }

    impl<'a> IntoIterator for Probe<'a> {
        type Item = ();
        type IntoIter = ProbeIter<'a>;

        #[track_caller]
        fn into_iter(self) -> Self::IntoIter {
            self.into_seen
                .set(Location::caller().line() == self.expected);
            ProbeIter {
                seen: self.next_seen,
                expected: self.expected,
                yielded: false,
            }
        }
    }

    #[stack_safe]
    fn transformed(n: u64, into: &Cell<bool>, next: &Cell<bool>) -> (bool, bool) {
        if n == 0 {
            return (into.get(), next.get());
        }
        let expected = line!() + 1;
        for () in (Probe {
            into_seen: into,
            next_seen: next,
            expected,
        }) {
            let _ = transformed(n - 1, into, next);
            break;
        }
        (into.get(), next.get())
    }

    fn native(n: u64, into: &Cell<bool>, next: &Cell<bool>) -> (bool, bool) {
        if n == 0 {
            return (into.get(), next.get());
        }
        let expected = line!() + 1;
        for () in (Probe {
            into_seen: into,
            next_seen: next,
            expected,
        }) {
            let _ = native(n - 1, into, next);
            break;
        }
        (into.get(), next.get())
    }

    fn run(f: fn(u64, &Cell<bool>, &Cell<bool>) -> (bool, bool)) -> (bool, bool) {
        f(1, &Cell::new(false), &Cell::new(false))
    }

    #[test]
    fn diverges_from_native() {
        // Both `into_iter` and `next` see the attribute's line rather than the loop's, as
        // anything in a macro expansion does.
        assert_eq!(run(native), (true, true));
        assert_eq!(run(transformed), (false, false));
    }
}

mod finding_11_question_mark_track_caller {
    use super::stack_safe;
    use std::panic::Location;

    #[derive(Debug)]
    struct Source(u32);

    #[derive(Debug, PartialEq)]
    struct Converted(bool);

    impl From<Source> for Converted {
        #[track_caller]
        fn from(source: Source) -> Self {
            Converted(Location::caller().line() == source.0)
        }
    }

    #[stack_safe]
    fn transformed(n: u64) -> Result<(), Converted> {
        if n == 0 {
            let expected = line!() + 1;
            Err(Source(expected))?;
            return Ok(());
        }
        transformed(n - 1)?;
        Ok(())
    }

    fn native(n: u64) -> Result<(), Converted> {
        if n == 0 {
            let expected = line!() + 1;
            Err(Source(expected))?;
            return Ok(());
        }
        native(n - 1)?;
        Ok(())
    }

    #[test]
    fn diverges_from_native() {
        // `?` reaches `From::from` through the shim, so the conversion reports a line inside
        // `yaspar-macros-defs` instead of the `?`.
        assert_eq!(native(0), Err(Converted(true)));
        assert_eq!(transformed(0), Err(Converted(false)));
    }
}

mod finding_12_derived_place_order {
    use super::stack_safe;
    use std::cell::Cell;

    struct Node {
        value: u64,
        left: Vec<Node>,
        right: Vec<Node>,
    }

    impl Node {
        fn select(&mut self, changed: &Cell<bool>) -> &mut Vec<Node> {
            if changed.get() {
                &mut self.right
            } else {
                &mut self.left
            }
        }
    }

    fn index(changed: &Cell<bool>) -> usize {
        changed.set(true);
        0
    }

    #[stack_safe(use_nonlinear_mut)]
    fn transformed(node: &mut Node, changed: &Cell<bool>) -> u64 {
        if node.left.is_empty() {
            node.value
        } else {
            transformed(&mut node.select(changed)[index(changed)], changed)
        }
    }

    fn native(node: &mut Node, changed: &Cell<bool>) -> u64 {
        if node.left.is_empty() {
            node.value
        } else {
            native(&mut node.select(changed)[index(changed)], changed)
        }
    }

    fn tree() -> Node {
        Node {
            value: 0,
            left: vec![Node {
                value: 10,
                left: vec![],
                right: vec![],
            }],
            right: vec![Node {
                value: 100,
                left: vec![],
                right: vec![],
            }],
        }
    }

    #[test]
    fn agrees_with_native() {
        assert_eq!(
            transformed(&mut tree(), &Cell::new(false)),
            native(&mut tree(), &Cell::new(false))
        );
    }
}

mod finding_13_data_in_frame_relocation {
    use super::stack_safe;
    use std::cell::Cell;

    struct Addressed {
        original: Cell<*const Addressed>,
        value: u64,
    }

    impl Addressed {
        fn new(value: u64) -> Self {
            Self {
                original: Cell::new(std::ptr::null()),
                value,
            }
        }

        fn remember(&self) {
            self.original.set(std::ptr::from_ref(self));
        }

        fn stayed_put(&self) -> bool {
            self.original.get() == std::ptr::from_ref(self)
        }
    }

    #[stack_safe(data_in_frame)]
    fn transformed(n: u64, current: &Addressed) -> (bool, u64) {
        if n == 0 {
            return (current.stayed_put(), current.value);
        }
        let next = Addressed::new(current.value + 1);
        next.remember();
        transformed(n - 1, &next)
    }

    fn native(n: u64, current: &Addressed) -> (bool, u64) {
        if n == 0 {
            return (current.stayed_put(), current.value);
        }
        let next = Addressed::new(current.value + 1);
        next.remember();
        native(n - 1, &next)
    }

    #[test]
    fn diverges_from_native() {
        // The lent local is moved into the store, so its address is not where a native
        // caller's frame would have left it. The answer itself is unchanged.
        assert_eq!(native(3, &Addressed::new(0)), (true, 3));
        assert_eq!(transformed(3, &Addressed::new(0)), (false, 3));
    }
}

mod finding_14_nonlinear_mut_binding_address {
    use super::stack_safe;

    struct Link {
        value: u64,
        child: Option<Box<Link>>,
    }

    #[stack_safe(use_nonlinear_mut)]
    fn transformed(node: &mut Link) -> (bool, u64) {
        let binding = std::ptr::addr_of!(node);
        if node.child.is_none() {
            return (true, node.value);
        }
        let (below, total) = transformed(&mut **node.child.as_mut().unwrap());
        (
            below && binding == std::ptr::addr_of!(node),
            total + node.value,
        )
    }

    fn native(node: &mut Link) -> (bool, u64) {
        let binding = std::ptr::addr_of!(node);
        if node.child.is_none() {
            return (true, node.value);
        }
        let (below, total) = native(&mut **node.child.as_mut().unwrap());
        (
            below && binding == std::ptr::addr_of!(node),
            total + node.value,
        )
    }

    fn chain() -> Link {
        Link {
            value: 3,
            child: Some(Box::new(Link {
                value: 2,
                child: Some(Box::new(Link {
                    value: 1,
                    child: Some(Box::new(Link {
                        value: 0,
                        child: None,
                    })),
                })),
            })),
        }
    }

    #[test]
    fn diverges_from_native() {
        // The `&mut` parameter is re-derived in the arm that resumes, so the *binding's*
        // address differs. The answer itself is unchanged.
        assert_eq!(native(&mut chain()), (true, 6));
        assert_eq!(transformed(&mut chain()), (false, 6));
    }
}

mod finding_15_lent_place_shadow_capture {
    use super::stack_safe;
    use std::ops::Index;

    #[derive(Clone, Copy)]
    struct Bag {
        key: usize,
        values: [u64; 2],
    }

    impl Index<Bag> for Bag {
        type Output = u64;

        fn index(&self, index: Bag) -> &Self::Output {
            &self.values[index.key]
        }
    }

    impl Index<&Bag> for Bag {
        type Output = u64;

        fn index(&self, index: &Bag) -> &Self::Output {
            &self.values[index.key]
        }
    }

    #[stack_safe(data_in_frame)]
    fn transformed(n: u64, value: &u64) -> u64 {
        if n == 0 {
            return *value;
        }
        let bag: Bag = Bag {
            key: 0,
            values: [10, 20],
        };
        transformed(
            n - 1,
            &bag[{
                let bag = Bag {
                    key: 1,
                    values: [0, 0],
                };
                bag
            }],
        )
    }

    fn native(n: u64, value: &u64) -> u64 {
        if n == 0 {
            return *value;
        }
        let bag: Bag = Bag {
            key: 0,
            values: [10, 20],
        };
        native(
            n - 1,
            &bag[{
                let bag = Bag {
                    key: 1,
                    values: [0, 0],
                };
                bag
            }],
        )
    }

    #[test]
    fn agrees_with_native() {
        assert_eq!(transformed(1, &0), native(1, &0));
    }
}

mod finding_20_method_track_caller {
    use super::stack_safe;
    use std::panic::Location;

    struct Native;

    impl Native {
        #[track_caller]
        fn run<'a, T, const N: usize>(
            &'a self,
            value: &'a [T; N],
            recurse: bool,
            expected: u32,
        ) -> bool {
            let _ = value;
            if recurse {
                self.run(value, false, expected)
            } else {
                Location::caller().line() == expected
            }
        }
    }

    struct Transformed;

    impl Transformed {
        #[stack_safe]
        #[track_caller]
        fn run<'a, T, const N: usize>(
            &'a self,
            value: &'a [T; N],
            recurse: bool,
            expected: u32,
        ) -> bool {
            let _ = value;
            if recurse {
                self.run(value, false, expected)
            } else {
                Location::caller().line() == expected
            }
        }
    }

    #[test]
    fn agrees_with_native() {
        let native_expected = line!() + 1;
        let native = Native.run(&[0], true, native_expected);
        let transformed_expected = line!() + 1;
        let transformed = Transformed.run(&[0], true, transformed_expected);
        assert_eq!(transformed, native);
    }
}

mod finding_21_module_reexport_resolution {
    use super::stack_safe;

    mod native_scope {
        mod other {
            pub fn f(_: u8) -> u8 {
                1
            }
        }
        use other::*;

        mod m {
            pub fn f(n: u8) -> u8 {
                if n == 0 { 2 } else { f(n - 1) }
            }
        }

        pub fn run() -> u8 {
            f(0)
        }
    }

    mod transformed_scope {
        use super::stack_safe;

        mod other {
            pub fn f(_: u8) -> u8 {
                1
            }
        }
        use other::*;

        #[stack_safe]
        mod m {
            pub fn f(n: u8) -> u8 {
                if n == 0 { 2 } else { f(n - 1) }
            }
        }

        pub fn run() -> u8 {
            f(0)
        }
    }

    #[test]
    fn diverges_from_native() {
        // The generated `use m::f` outranks the glob, so the unqualified call changes target.
        assert_eq!(native_scope::run(), 1);
        assert_eq!(transformed_scope::run(), 2);
    }
}

// ---------------------------------------------------------------------------
// Boundaries of the rejections in `tests/ui/`.
//
// Each rejection there had a broader form that would have refused the code below. These are the
// cases that kept the checks narrow, so they are worth a test of their own: a check that grows will
// fail to compile here rather than in somebody's crate.
// ---------------------------------------------------------------------------

/// Shadowing a binding one is finished with, inside a loop that recurses. The outer `item` is never
/// read after the shadow, so the two are never told apart and one payload slot is right.
mod shadowing_a_finished_binding {
    use super::stack_safe;

    #[stack_safe]
    fn transformed(items: &[u64], n: u64) -> u64 {
        if n == 0 {
            return 0;
        }
        let mut total = 0;
        for item in items {
            let item = item + 1;
            total += item + transformed(items, n - 1);
        }
        total
    }

    fn native(items: &[u64], n: u64) -> u64 {
        if n == 0 {
            return 0;
        }
        let mut total = 0;
        for item in items {
            let item = item + 1;
            total += item + native(items, n - 1);
        }
        total
    }

    #[test]
    fn agrees_with_native() {
        assert_eq!(transformed(&[1, 2], 3), native(&[1, 2], 3));
    }
}

/// Shadowing where the outer binding is only read *before* the block. Nothing has to tell the two
/// apart after the recursion, so one payload slot is right and the check must not fire.
mod shadowing_read_only_before {
    use super::stack_safe;

    #[stack_safe]
    fn transformed(n: u64) -> u64 {
        if n == 0 {
            return 0;
        }
        let x = 10 + n;
        let before = x * 2;
        {
            let x = 100 + n;
            let _ = transformed(n - 1);
            let _ = x;
        }
        before
    }

    fn native(n: u64) -> u64 {
        if n == 0 {
            return 0;
        }
        let x = 10 + n;
        let before = x * 2;
        {
            let x = 100 + n;
            let _ = native(n - 1);
            let _ = x;
        }
        before
    }

    #[test]
    fn agrees_with_native() {
        assert_eq!(transformed(3), native(3));
    }
}

/// Shadowing in the *same* block as the call. There is no inner scope to come back out of, so the
/// later mentions read the inner binding under both programs.
mod shadowing_in_one_block {
    use super::stack_safe;

    #[stack_safe]
    fn transformed(n: u64) -> u64 {
        if n == 0 {
            return 0;
        }
        let x = 10 + n;
        let x = x * 2;
        transformed(n - 1) + x
    }

    fn native(n: u64) -> u64 {
        if n == 0 {
            return 0;
        }
        let x = 10 + n;
        let x = x * 2;
        native(n - 1) + x
    }

    #[test]
    fn agrees_with_native() {
        assert_eq!(transformed(3), native(3));
    }
}

/// A turbofish that restates the enclosing generics. It names the same instantiation, so it is not
/// the call to another one that the macro refuses.
mod turbofish_restating_its_own_generics {
    use super::stack_safe;

    #[stack_safe]
    fn transformed<T: Copy + Into<u64>, const N: u64>(n: u64, t: T) -> u64 {
        if n == 0 {
            N + t.into()
        } else {
            transformed::<T, N>(n - 1, t) + 1
        }
    }

    fn native<T: Copy + Into<u64>, const N: u64>(n: u64, t: T) -> u64 {
        if n == 0 {
            N + t.into()
        } else {
            native::<T, N>(n - 1, t) + 1
        }
    }

    #[test]
    fn agrees_with_native() {
        assert_eq!(transformed::<u8, 5>(3, 7), native::<u8, 5>(3, 7));
    }
}

/// Two nested functions of one name in *different* bodies. Nothing is ambiguous — each is in scope
/// only inside the body that declares it — so neither the duplicate-member check nor the
/// sibling-ambiguity one has anything to say.
mod same_name_in_different_bodies {
    use super::stack_safe;

    #[stack_safe]
    fn transformed_one(n: u64) -> u64 {
        fn go(n: u64) -> u64 {
            if n == 0 { 1 } else { go(n - 1) + 1 }
        }
        go(n)
    }

    #[stack_safe]
    fn transformed_two(n: u64) -> u64 {
        fn go(n: u64) -> u64 {
            if n == 0 { 100 } else { go(n - 1) + 10 }
        }
        go(n)
    }

    fn native_one(n: u64) -> u64 {
        fn go(n: u64) -> u64 {
            if n == 0 { 1 } else { go(n - 1) + 1 }
        }
        go(n)
    }

    fn native_two(n: u64) -> u64 {
        fn go(n: u64) -> u64 {
            if n == 0 { 100 } else { go(n - 1) + 10 }
        }
        go(n)
    }

    #[test]
    fn agrees_with_native() {
        assert_eq!(transformed_one(3), native_one(3));
        assert_eq!(transformed_two(3), native_two(3));
    }
}

/// An item at the top of the *body* rather than of a block. Those are moved out to enclose every
/// arm, so the code after a call still sees them.
mod body_level_item_across_a_call {
    use super::stack_safe;

    #[stack_safe]
    fn transformed(n: u64) -> u64 {
        const STEP: u64 = 7;
        fn helper(v: u64) -> u64 {
            v * 2
        }
        if n == 0 {
            return 0;
        }
        let below = transformed(n - 1);
        helper(below) + STEP
    }

    fn native(n: u64) -> u64 {
        const STEP: u64 = 7;
        fn helper(v: u64) -> u64 {
            v * 2
        }
        if n == 0 {
            return 0;
        }
        let below = native(n - 1);
        helper(below) + STEP
    }

    #[test]
    fn agrees_with_native() {
        assert_eq!(transformed(3), native(3));
    }
}

// ---------------------------------------------------------------------------
// Around the fixes: adjacent shapes the same code paths now handle.
// ---------------------------------------------------------------------------

/// A callable expression that *itself* recurses. Sequencing the callee before the arguments — which
/// is what fixed the evaluation order — also means a recursive call inside it is lowered rather than
/// left to run on the native stack.
mod callee_expression_recurses {
    use super::stack_safe;

    fn make(seed: u64) -> fn(u64) -> u64 {
        if seed.is_multiple_of(2) {
            double
        } else {
            triple
        }
    }
    fn double(v: u64) -> u64 {
        v * 2
    }
    fn triple(v: u64) -> u64 {
        v * 3
    }

    #[stack_safe]
    fn transformed(n: u64) -> u64 {
        if n == 0 {
            return 1;
        }
        make(transformed(n - 1))(n)
    }

    fn native(n: u64) -> u64 {
        if n == 0 {
            return 1;
        }
        make(native(n - 1))(n)
    }

    #[test]
    fn agrees_with_native() {
        for n in 0..6 {
            assert_eq!(transformed(n), native(n), "n = {n}");
        }
    }
}

/// A raw identifier as a payload: it is a parameter, live across the call, and read afterwards, so
/// the frame has to carry it under the name Rust resolves rather than the spelling.
mod raw_identifier_payload {
    use super::stack_safe;

    #[stack_safe]
    fn transformed(r#fn: u64, n: u64) -> u64 {
        if n == 0 {
            return 0;
        }
        let below = transformed(r#fn + 1, n - 1);
        below + r#fn
    }

    fn native(r#fn: u64, n: u64) -> u64 {
        if n == 0 {
            return 0;
        }
        let below = native(r#fn + 1, n - 1);
        below + r#fn
    }

    #[test]
    fn agrees_with_native() {
        assert_eq!(transformed(10, 3), native(10, 3));
    }
}

/// A call in the *base* of an indexed place, beside a call in the index. Rust runs the base first,
/// which is the order the hoist has to keep; the log records which ran when, so a reversal shows up
/// as a different number rather than a different answer.
mod calls_on_both_sides_of_an_index {
    use std::cell::Cell;

    use super::stack_safe;

    struct Node {
        value: u64,
        kids: Vec<Node>,
    }

    impl Node {
        fn pick(&mut self, log: &Cell<u64>) -> &mut Vec<Node> {
            log.set(log.get() * 10 + 1);
            &mut self.kids
        }
    }

    fn idx(log: &Cell<u64>) -> usize {
        log.set(log.get() * 10 + 2);
        0
    }

    #[stack_safe(use_nonlinear_mut)]
    fn transformed(node: &mut Node, log: &Cell<u64>) -> u64 {
        if node.kids.is_empty() {
            return node.value;
        }
        transformed(&mut node.pick(log)[idx(log)], log)
    }

    fn native(node: &mut Node, log: &Cell<u64>) -> u64 {
        if node.kids.is_empty() {
            return node.value;
        }
        native(&mut node.pick(log)[idx(log)], log)
    }

    fn chain() -> Node {
        Node {
            value: 1,
            kids: vec![Node {
                value: 2,
                kids: vec![Node {
                    value: 3,
                    kids: Vec::new(),
                }],
            }],
        }
    }

    fn run(f: fn(&mut Node, &Cell<u64>) -> u64) -> (u64, u64) {
        let log = Cell::new(0);
        let out = f(&mut chain(), &log);
        (out, log.get())
    }

    #[test]
    fn agrees_with_native() {
        assert_eq!(run(transformed), run(native));
    }
}

/// A `#[track_caller]` free function that is itself rewritten. The attribute is carried onto every
/// generated frame, so the location reported is the external call site — which is outside the
/// expansion, and therefore a real line.
mod track_caller_free_function {
    use std::panic::Location;

    use super::stack_safe;

    #[stack_safe]
    #[track_caller]
    fn transformed(n: u64) -> u32 {
        if n == 0 {
            Location::caller().line()
        } else {
            transformed(n - 1)
        }
    }

    #[track_caller]
    fn native(n: u64) -> u32 {
        if n == 0 {
            Location::caller().line()
        } else {
            native(n - 1)
        }
    }

    #[test]
    fn agrees_with_native() {
        // Both calls are on this line, so both have to report it.
        assert_eq!((transformed(3), line!()), (native(3), line!()));
    }
}

/// Three things in one call with an order between them: the callee, a recursive argument, and an
/// argument with a side effect. Each appends to the log, so the log is the order.
mod callee_and_arguments_in_order {
    use std::cell::Cell;

    use super::stack_safe;

    fn make(log: &Cell<u64>) -> fn(u64, u64) -> u64 {
        log.set(log.get() * 10 + 1);
        sum
    }
    fn sum(a: u64, b: u64) -> u64 {
        a + b
    }
    fn side(log: &Cell<u64>) -> u64 {
        log.set(log.get() * 10 + 3);
        7
    }

    #[stack_safe]
    fn transformed(n: u64, log: &Cell<u64>) -> u64 {
        if n == 0 {
            return 0;
        }
        make(log)(transformed(n - 1, log), side(log))
    }

    fn native(n: u64, log: &Cell<u64>) -> u64 {
        if n == 0 {
            return 0;
        }
        make(log)(native(n - 1, log), side(log))
    }

    fn run(f: fn(u64, &Cell<u64>) -> u64) -> (u64, u64) {
        let log = Cell::new(0);
        let out = f(2, &log);
        (out, log.get())
    }

    #[test]
    fn agrees_with_native() {
        assert_eq!(run(transformed), run(native));
    }
}

/// A struct literal with a recursive call in one field and a side effect in another. Fields run in
/// the order they are written, and the recursion splits the literal in half.
mod struct_literal_field_order {
    use std::cell::Cell;

    use super::stack_safe;

    struct Pair {
        left: u64,
        right: u64,
    }

    fn side(log: &Cell<u64>, tag: u64) -> u64 {
        log.set(log.get() * 10 + tag);
        tag
    }

    #[stack_safe]
    fn transformed(n: u64, log: &Cell<u64>) -> u64 {
        if n == 0 {
            return 0;
        }
        let p = Pair {
            left: side(log, 1),
            right: transformed(n - 1, log) + side(log, 2),
        };
        p.left + p.right
    }

    fn native(n: u64, log: &Cell<u64>) -> u64 {
        if n == 0 {
            return 0;
        }
        let p = Pair {
            left: side(log, 1),
            right: native(n - 1, log) + side(log, 2),
        };
        p.left + p.right
    }

    fn run(f: fn(u64, &Cell<u64>) -> u64) -> (u64, u64) {
        let log = Cell::new(0);
        let out = f(2, &log);
        (out, log.get())
    }

    #[test]
    fn agrees_with_native() {
        assert_eq!(run(transformed), run(native));
    }
}

/// The *receiver* recurses and the argument has a side effect. The receiver runs first, so the
/// recursion happens before the argument — the reverse of the shapes that bite.
mod recursive_receiver_then_argument {
    use std::cell::Cell;

    use super::stack_safe;

    struct Value(u64);

    impl Value {
        fn plus(&self, rhs: u64) -> u64 {
            self.0 + rhs
        }
    }

    fn side(log: &Cell<u64>) -> u64 {
        log.set(log.get() * 10 + 9);
        1
    }

    #[stack_safe]
    fn transformed(n: u64, log: &Cell<u64>) -> u64 {
        if n == 0 {
            return 0;
        }
        Value(transformed(n - 1, log)).plus(side(log))
    }

    fn native(n: u64, log: &Cell<u64>) -> u64 {
        if n == 0 {
            return 0;
        }
        Value(native(n - 1, log)).plus(side(log))
    }

    fn run(f: fn(u64, &Cell<u64>) -> u64) -> (u64, u64) {
        let log = Cell::new(0);
        let out = f(3, &log);
        (out, log.get())
    }

    #[test]
    fn agrees_with_native() {
        assert_eq!(run(transformed), run(native));
    }
}
