// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Liveness for payload points (lowered-loop entries and resume points): which locals each one
//! carries. The two kinds mention each other's markers, so they are solved together.

use proc_macro2::{Delimiter, Group, Ident, TokenStream, TokenTree};
use std::collections::{HashMap, HashSet};

use super::names::{frame_marker, state_marker};
use super::walk::{Derived, PayloadPoint, ResumePoint};

/// For each payload point, the values it carries: its `forced` ones plus the in-scope bindings
/// its code mentions (syntactically, so moved-away locals are not threaded).
///
/// If `a`'s code contains `b`'s marker, `a` must also carry what `b` carries, so sets grow to a
/// fixed point. Bindings in `PayloadPoint::derived` are recomputed instead of carried; needing
/// one means needing its source.
pub(super) fn solve_payloads(loops: &[PayloadPoint], resumes: &[ResumePoint]) -> Solved {
    // One index space while solving: loops first, then resumes.
    let points: Vec<&PayloadPoint> = loops
        .iter()
        .chain(resumes.iter().map(|r| &r.point))
        .collect();
    let markers: Vec<String> = (0..loops.len())
        .map(|n| state_marker(n).to_string())
        .chain((0..resumes.len()).map(|r| frame_marker(r).to_string()))
        .collect();

    let mut mentioned: Vec<HashSet<String>> = points.iter().map(|p| idents(&p.code)).collect();
    let mut solved: Vec<Vec<Ident>> = vec![Vec::new(); points.len()];
    loop {
        let mut changed = false;
        for n in 0..points.len() {
            let mut needed = mentioned[n].clone();
            for (m, marker) in markers.iter().enumerate() {
                if mentioned[n].contains(marker) {
                    for id in &solved[m] {
                        needed.insert(canonical(id));
                    }
                }
            }
            let recomputed = recomputed(points[n], &needed);
            for d in &recomputed {
                needed.insert(canonical(&d.from));
            }
            let mut next: Vec<Ident> = points[n].forced.clone();
            for id in &points[n].scope {
                if needed.contains(&canonical(id))
                    && !recomputed.iter().any(|d| &d.name == id)
                    && !next.iter().any(|i| i == id)
                {
                    next.push(id.clone());
                }
            }
            if next != solved[n] {
                solved[n] = next;
                changed = true;
            }
        }
        // Propagate grown payloads into the points whose code mentions their markers.
        for set in mentioned.iter_mut() {
            for (m, marker) in markers.iter().enumerate() {
                if set.contains(marker) {
                    for id in &solved[m] {
                        if set.insert(canonical(id)) {
                            changed = true;
                        }
                    }
                }
            }
        }
        if !changed {
            break;
        }
    }

    // At the fixed point, `mentioned` covers every marker's payload.
    let derived: Vec<Vec<Derived>> = points
        .iter()
        .zip(&mentioned)
        .map(|(p, m)| recomputed(p, m).into_iter().cloned().collect())
        .collect();
    let frames = solved.split_off(loops.len());
    Solved {
        states: solved,
        frames,
        derived,
    }
}

/// What [`solve_payloads`] settles on.
pub(super) struct Solved {
    /// Per lowered loop, the values its state carries.
    pub(super) states: Vec<Vec<Ident>>,
    /// Per resume point, the values its frame carries.
    pub(super) frames: Vec<Vec<Ident>>,
    /// Per point (loops first), bindings recomputed rather than carried, in binding order.
    pub(super) derived: Vec<Vec<Derived>>,
}

/// The bindings `point` recomputes rather than carries, of those `needed` names.
fn recomputed<'p>(point: &'p PayloadPoint, needed: &HashSet<String>) -> Vec<&'p Derived> {
    point
        .derived
        .iter()
        .filter(|d| needed.contains(&canonical(&d.name)))
        .collect()
}

/// A name without its `r#` prefix, so `r#type` matches a `{type}` format capture. A missed use
/// would silently resolve to the outermost call's parameter.
fn canonical(id: &Ident) -> String {
    let name = id.to_string();
    name.strip_prefix("r#").unwrap_or(&name).to_owned()
}

fn idents(ts: &TokenStream) -> HashSet<String> {
    fn go(ts: &TokenStream, out: &mut HashSet<String>) {
        for t in ts.clone() {
            match t {
                TokenTree::Ident(i) => {
                    out.insert(canonical(&i));
                }
                TokenTree::Group(g) => go(&g.stream(), out),
                // Implicit format captures (`format!("{n}")`) are uses too.
                TokenTree::Literal(l) => format_captures(&l.to_string(), out),
                _ => {}
            }
        }
    }
    let mut out = HashSet::new();
    go(ts, &mut out);
    out
}

/// Names captured by a format string: `{n}`, `{n:?}`, and `w` in `{:w$}`. Over-approximates on
/// purpose: a false hit can only fail to compile, a miss gives wrong results.
fn format_captures(literal: &str, out: &mut HashSet<String>) {
    let bytes = literal.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'{' {
            i += 1;
            continue;
        }
        // `{{` is an escaped brace, not a placeholder.
        if bytes.get(i + 1) == Some(&b'{') {
            i += 2;
            continue;
        }
        let Some(len) = bytes[i + 1..].iter().position(|&b| b == b'}') else {
            break;
        };
        let body = &literal[i + 1..i + 1 + len];
        let (name, spec) = match body.split_once(':') {
            Some((name, spec)) => (name, spec),
            None => (body, ""),
        };
        if is_ident(name) {
            out.insert(name.to_string());
        }
        // `{:w$}` / `{:.p$}` take the width or precision from a named binding.
        for part in spec.split('$').rev().skip(1) {
            let name: String = part
                .chars()
                .rev()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            let name: String = name.chars().rev().collect();
            if is_ident(&name) {
                out.insert(name);
            }
        }
        i += len + 2;
    }
}

fn is_ident(s: &str) -> bool {
    let mut chars = s.chars();
    chars.next().is_some_and(|c| c.is_alphabetic() || c == '_')
        && chars.all(|c| c.is_alphanumeric() || c == '_')
}

/// Replace each payload marker with its parenthesized tuple.
pub(super) fn substitute(ts: TokenStream, map: &HashMap<String, TokenStream>) -> TokenStream {
    ts.into_iter()
        .map(|t| match t {
            TokenTree::Ident(ref i) => match map.get(&i.to_string()) {
                Some(rep) => TokenTree::Group(Group::new(Delimiter::Parenthesis, rep.clone())),
                None => t,
            },
            TokenTree::Group(g) => {
                TokenTree::Group(Group::new(g.delimiter(), substitute(g.stream(), map)))
            }
            other => other,
        })
        .collect()
}
