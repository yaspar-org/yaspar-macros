// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! `#[cfg]` on something a recursive call is cut out of.
//!
//! The transform splits such a construct across the driver's arms, so the gate has to travel to
//! every piece. `cfg(all())` is always on and `cfg(any())` always off, which lets these tests pin
//! both answers without a feature flag: the disabled arms call functions that do not exist, so a
//! gate that failed to travel would not compile.

// `all()` and `any()` are the point: they are a predicate that is always true and one that is
// always false, so both answers can be pinned in one build without declaring a feature.
#![allow(clippy::non_minimal_cfg)]

use yaspar_macros::stack_safe;

#[stack_safe]
mod gated_stmt {
    /// The shape this came from: a gated `if` whose taken branch recurses and returns, with
    /// ungated code after it.
    pub fn walk(n: u64, acc: &mut Vec<u64>) -> u64 {
        if n == 0 {
            return 0;
        }
        #[cfg(all())]
        if n.is_multiple_of(2) {
            acc.push(n);
            return step(n, acc) + 1;
        }
        #[cfg(any())]
        if n % 3 == 0 {
            return no_such_function(step(n, acc));
        }
        step(n, acc)
    }

    pub fn step(n: u64, acc: &mut Vec<u64>) -> u64 {
        walk(n - 1, acc)
    }
}

#[test]
fn gated_statement_keeps_the_live_branch() {
    let mut acc = Vec::new();
    // 4 and 2 take the gated branch and add one each; 3 and 1 do not.
    assert_eq!(gated_stmt::walk(4, &mut acc), 2);
    assert_eq!(acc, vec![4, 2]);
}

#[test]
fn gated_statement_is_flat() {
    let deep = std::thread::Builder::new()
        .stack_size(64 * 1024)
        .spawn(|| {
            let mut acc = Vec::new();
            gated_stmt::walk(100_000, &mut acc)
        })
        .expect("spawn")
        .join();
    assert_eq!(deep.ok(), Some(50_000));
}

#[stack_safe]
mod gated_arm {
    pub enum Op {
        Down,
        Twice,
    }

    /// A gated *arm* whose body recurses. The arm itself keeps its attribute, but the code after
    /// the call inside it becomes another arm of the driver, which needs the gate too.
    pub fn arm_walk(op: &Op, n: u64) -> u64 {
        if n == 0 {
            return 0;
        }
        match op {
            #[cfg(all())]
            Op::Down => arm_step(op, n) + 1,
            #[cfg(any())]
            Op::Down => no_such_function(arm_step(op, n)),
            Op::Twice => arm_step(op, n) + 2,
        }
    }

    pub fn arm_step(op: &Op, n: u64) -> u64 {
        arm_walk(op, n - 1)
    }
}

#[test]
fn gated_arm_keeps_the_live_arm() {
    assert_eq!(gated_arm::arm_walk(&gated_arm::Op::Down, 3), 3);
    assert_eq!(gated_arm::arm_walk(&gated_arm::Op::Twice, 3), 6);
}

#[test]
fn gated_arm_is_flat() {
    let deep = std::thread::Builder::new()
        .stack_size(64 * 1024)
        .spawn(|| gated_arm::arm_walk(&gated_arm::Op::Down, 100_000))
        .expect("spawn")
        .join();
    assert_eq!(deep.ok(), Some(100_000));
}

#[stack_safe]
mod gated_field {
    pub struct Count {
        pub below: u64,
        #[cfg(all())]
        pub kept: u64,
        #[cfg(any())]
        pub gone: u64,
    }

    /// A gated *field* whose value recurses. The literal is rebuilt from names and values, so the
    /// field's attribute cannot ride along on it either.
    pub fn field_walk(n: u64) -> Count {
        if n == 0 {
            return Count {
                below: 0,
                #[cfg(all())]
                kept: 0,
                #[cfg(any())]
                gone: 0,
            };
        }
        // Only the gated field recurses, and only once: two calls per level would be exponential.
        Count {
            below: n,
            #[cfg(all())]
            kept: field_step(n).kept + 2,
            #[cfg(any())]
            gone: no_such_function(field_step(n)),
        }
    }

    pub fn field_step(n: u64) -> Count {
        field_walk(n - 1)
    }
}

#[test]
fn gated_field_keeps_the_live_field() {
    let c = gated_field::field_walk(3);
    assert_eq!((c.below, c.kept), (3, 6));
}

#[test]
fn gated_field_is_flat() {
    let deep = std::thread::Builder::new()
        .stack_size(64 * 1024)
        .spawn(|| gated_field::field_walk(100_000).kept)
        .expect("spawn")
        .join();
    assert_eq!(deep.ok(), Some(200_000));
}
