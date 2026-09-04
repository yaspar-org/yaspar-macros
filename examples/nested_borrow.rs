// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use yaspar_macros::stack_safe;

#[derive(Clone, Copy)]
pub struct H<'tm>(&'tm u64);
pub trait Marker {}
impl Marker for () {}
pub struct Env<'tm, Ctx> {
    pub ctx: Ctx,
    pub seen: Vec<u64>,
    pub m: std::marker::PhantomData<&'tm ()>,
}

#[stack_safe(data_in_frame)]
mod m {
    use super::{Env, H, Marker};

    /// Shape of translate_quantifier_body_from_cvc5, now with the members' own generics and an
    /// element type carrying their lifetime.
    pub fn quant<'tm, Ctx>(groups: Vec<Vec<H<'tm>>>, env: &mut Env<'tm, Ctx>) -> Result<u64, u64>
    where
        Ctx: Marker,
    {
        let mut attrs: Vec<u64> = Vec::with_capacity(groups.len());
        for group in &groups {
            let mut trigger: Vec<u64> = Vec::with_capacity(group.len());
            for t in group {
                trigger.push(step(*t, env)?);
            }
            attrs.push(trigger.len() as u64);
        }
        Ok(attrs.len() as u64)
    }

    pub fn step<'tm, Ctx>(h: H<'tm>, env: &mut Env<'tm, Ctx>) -> Result<u64, u64>
    where
        Ctx: Marker,
    {
        let n = *h.0;
        if n == 0 {
            return Ok(0);
        }
        env.seen.push(n);
        quant(vec![vec![h]], env)
    }
}

fn main() {
    let zero = 0u64;
    let mut env: Env<'_, ()> = Env {
        ctx: (),
        seen: vec![],
        m: std::marker::PhantomData,
    };
    println!("{:?}", m::step(H(&zero), &mut env));
}
