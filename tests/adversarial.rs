// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Known semantic mismatches found by differential testing.
//!
//! Every test compares ordinary recursion with the `#[stack_safe]` transform.
//! They are ignored while the corresponding bugs remain open, so the regular
//! suite stays green. Run them with:
//!
//! ```text
//! cargo test --test adversarial -- --ignored
//! ```
//!
//! None of these tests relies on destructor or `Drop` call ordering.

#![allow(dead_code, unused_imports)]

use yaspar_macros::stack_safe;

mod finding_01_local_callable_shadow {
    use super::stack_safe;

    #[stack_safe]
    fn transformed(n: u64) -> u64 {
        if n == 0 {
            return 1;
        }
        let below = transformed(n - 1);
        let transformed: fn(u64) -> u64 = |_| 100;
        below + transformed(0)
    }

    fn native(n: u64) -> u64 {
        if n == 0 {
            return 1;
        }
        let below = native(n - 1);
        let native: fn(u64) -> u64 = |_| 100;
        below + native(0)
    }

    #[test]
    #[ignore = "known bug: local callable is mistaken for recursion"]
    fn agrees_with_native() {
        assert_eq!(transformed(2), native(2));
    }
}

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
    #[ignore = "known bug: callable expression runs after recursive arguments"]
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
    #[ignore = "known bug: by-value place receiver is reread after recursion"]
    fn agrees_with_native() {
        assert_eq!(transformed(2, &mut [Value(0)]), native(2, &mut [Value(0)]));
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
    #[ignore = "known bug: raw identifier format capture is not carried"]
    fn agrees_with_native() {
        assert_eq!(transformed(10, 3), native(10, 3));
    }
}

mod finding_05_shadowed_payload_binding {
    use super::stack_safe;

    #[stack_safe]
    fn transformed(n: u64) -> u64 {
        if n == 0 {
            0
        } else {
            let x = 10 + n;
            {
                let x = 100 + n;
                let _ = transformed(n - 1);
                let _ = x;
            }
            x
        }
    }

    fn native(n: u64) -> u64 {
        if n == 0 {
            0
        } else {
            let x = 10 + n;
            {
                let x = 100 + n;
                let _ = native(n - 1);
                let _ = x;
            }
            x
        }
    }

    #[test]
    #[ignore = "known bug: payload liveness confuses shadowed bindings"]
    fn agrees_with_native() {
        assert_eq!(transformed(2), native(2));
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
    #[ignore = "known bug: receiver type is ignored when rewriting group methods"]
    fn agrees_with_native() {
        assert_eq!(TRANSFORMED_TARGET.recurse(0), NATIVE_TARGET.recurse(0));
    }
}

mod finding_07_recursive_turbofish {
    use super::stack_safe;

    #[stack_safe]
    fn transformed<const N: u64>(n: u64) -> u64 {
        if n == 0 { N } else { transformed::<7>(n - 1) }
    }

    fn native<const N: u64>(n: u64) -> u64 {
        if n == 0 { N } else { native::<7>(n - 1) }
    }

    #[test]
    #[ignore = "known bug: recursive turbofish arguments are discarded"]
    fn agrees_with_native() {
        assert_eq!(transformed::<3>(1), native::<3>(1));
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
    #[ignore = "known bug: borrowed receiver autoderef runs after recursion"]
    fn agrees_with_native() {
        assert_eq!(run(transformed), run(native));
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
    #[ignore = "known bug: argument deref coercion is delayed past recursion"]
    fn agrees_with_native() {
        assert_eq!(run(transformed), run(native));
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
    #[ignore = "known bug: lowered for loop changes track_caller locations"]
    fn agrees_with_native() {
        assert_eq!(run(transformed), run(native));
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
    #[ignore = "known bug: question-mark conversion changes track_caller location"]
    fn agrees_with_native() {
        assert_eq!(transformed(0), native(0));
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
    #[ignore = "known bug: derived mutable place reverses base/index evaluation"]
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
    #[ignore = "known bug: data_in_frame relocation is observable"]
    fn agrees_with_native() {
        assert_eq!(
            transformed(3, &Addressed::new(0)),
            native(3, &Addressed::new(0))
        );
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
    #[ignore = "known bug: nonlinear mut recreates the parameter binding"]
    fn agrees_with_native() {
        assert_eq!(transformed(&mut chain()), native(&mut chain()));
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
    #[ignore = "known bug: lent-place substitution ignores lexical shadowing"]
    fn agrees_with_native() {
        assert_eq!(transformed(1, &0), native(1, &0));
    }
}

mod finding_16_raw_identifier_temp_collision {
    use super::stack_safe;

    #[stack_safe]
    fn transformed(n: u64) -> u64 {
        if n == 0 {
            1
        } else {
            let r#__ss_v0 = 100 + n;
            transformed(n - 1) + r#__ss_v0
        }
    }

    fn native(n: u64) -> u64 {
        if n == 0 {
            1
        } else {
            let r#__ss_v0 = 100 + n;
            native(n - 1) + r#__ss_v0
        }
    }

    #[test]
    #[ignore = "known bug: raw identifier collides with a generated temporary"]
    fn agrees_with_native() {
        assert_eq!(transformed(1), native(1));
    }
}

mod finding_17_sibling_nested_function_scope {
    use super::stack_safe;

    #[stack_safe]
    fn transformed(n: u64, left: bool) -> u64 {
        if left {
            fn step(n: u64) -> u64 {
                if n == 0 {
                    100
                } else {
                    transformed(n - 1, false) + 1
                }
            }
            step(n)
        } else {
            fn step(n: u64) -> u64 {
                if n == 0 {
                    200
                } else {
                    transformed(n - 1, true) + 10
                }
            }
            step(n)
        }
    }

    fn native(n: u64, left: bool) -> u64 {
        if left {
            fn step(n: u64) -> u64 {
                if n == 0 {
                    100
                } else {
                    native(n - 1, false) + 1
                }
            }
            step(n)
        } else {
            fn step(n: u64) -> u64 {
                if n == 0 {
                    200
                } else {
                    native(n - 1, true) + 10
                }
            }
            step(n)
        }
    }

    #[test]
    #[ignore = "known bug: sibling branch-local functions are conflated"]
    fn agrees_with_native() {
        assert_eq!(transformed(2, true), native(2, true));
    }
}

mod finding_18_outer_nested_function_identity {
    use super::stack_safe;

    mod native {
        pub fn f(n: u8) -> u8 {
            fn f(_: u8) -> u8 {
                1 + self::f(0)
            }

            if n == 0 { 0 } else { f(0) }
        }
    }

    #[stack_safe]
    mod transformed {
        pub fn f(n: u8) -> u8 {
            fn f(_: u8) -> u8 {
                1 + self::f(0)
            }

            if n == 0 { 0 } else { f(0) }
        }
    }

    #[test]
    #[ignore = "known bug: outer and nested function identities collapse by name"]
    fn agrees_with_native() {
        assert_eq!(transformed::f(1), native::f(1));
    }
}

mod finding_19_block_local_item_scope {
    use super::stack_safe;

    mod native {
        const X: u8 = 2;

        pub fn run() -> u8 {
            fn f(n: bool) -> u8 {
                {
                    const X: u8 = 1;
                    if n { g(false) + X } else { 0 }
                }
            }

            fn g(n: bool) -> u8 {
                if n { f(false) } else { 0 }
            }

            f(true)
        }
    }

    #[stack_safe]
    mod transformed {
        const X: u8 = 2;

        pub fn run() -> u8 {
            fn f(n: bool) -> u8 {
                {
                    const X: u8 = 1;
                    if n { g(false) + X } else { 0 }
                }
            }

            fn g(n: bool) -> u8 {
                if n { f(false) } else { 0 }
            }

            f(true)
        }
    }

    #[test]
    #[ignore = "known bug: block-local item scope is lost after suspension"]
    fn agrees_with_native() {
        assert_eq!(transformed::run(), native::run());
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
    #[ignore = "known bug: transformed method loses track_caller propagation"]
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
    #[ignore = "known bug: generated module re-export changes name resolution"]
    fn agrees_with_native() {
        assert_eq!(transformed_scope::run(), native_scope::run());
    }
}
