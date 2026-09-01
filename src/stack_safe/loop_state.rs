// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Which locals each payload point threads, solved to a fixed point.
//!
//! There are two kinds of point and they refer to each other, so one solver handles
//! both: a lowered loop's entry, and a resume point after a recursive call. A loop
//! body containing a call mentions that call's frame marker; the resume arm that
//! follows the call mentions the loop's state marker for the next iteration.

use proc_macro2::{Delimiter, Group, Ident, TokenStream, TokenTree};
use std::collections::{HashMap, HashSet};

use super::names::{frame_marker, state_marker};
use super::walk::{PayloadPoint, ResumePoint};

/// Compute, for every payload point, the list of values it carries: the forced ones
/// (a `for` loop's iterator, a parked context pointer) plus the in-scope bindings its
/// code actually mentions.
///
/// Threading *all* in-scope bindings would be simpler but wrong in practice — it
/// would try to move a local that the body had already moved elsewhere. So the set is
/// filtered by the identifiers appearing in the generated code. That filtering is
/// what a boxed closure used to get for free from capture inference, and it is the
/// price of keeping the frames in a plain `Vec`.
///
/// One point's code may mention another's marker. Those markers stand for tuples
/// whose contents are not yet known, so the sets are grown to a fixed point: if `a`'s
/// code contains `b`'s marker, everything `b` threads and `a` has in scope must be
/// live in `a` too.
///
/// The main arms are not inputs: they can only *enter* points, never receive their
/// payloads, so they contribute nothing and are substituted with whatever the points
/// settle on.
pub(super) fn solve_payloads(
    loops: &[PayloadPoint],
    resumes: &[ResumePoint],
) -> (Vec<Vec<Ident>>, Vec<Vec<Ident>>) {
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
                        needed.insert(id.to_string());
                    }
                }
            }
            let mut next: Vec<Ident> = points[n].forced.clone();
            for id in &points[n].scope {
                if needed.contains(&id.to_string()) && !next.iter().any(|i| i == id) {
                    next.push(id.clone());
                }
            }
            if next != solved[n] {
                solved[n] = next;
                changed = true;
            }
        }
        // Growing one payload can add identifiers another point must now keep alive,
        // so re-derive until nothing moves.
        for set in mentioned.iter_mut() {
            for (m, marker) in markers.iter().enumerate() {
                if set.contains(marker) {
                    for id in &solved[m] {
                        if set.insert(id.to_string()) {
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

    let resume_solved = solved.split_off(loops.len());
    (solved, resume_solved)
}

fn idents(ts: &TokenStream) -> HashSet<String> {
    fn go(ts: &TokenStream, out: &mut HashSet<String>) {
        for t in ts.clone() {
            match t {
                TokenTree::Ident(i) => {
                    out.insert(i.to_string());
                }
                TokenTree::Group(g) => go(&g.stream(), out),
                // A name can be used from *inside a string literal* too: an implicit
                // format capture, `format!("{n}")`. Those are real uses, and missing
                // one is not merely a lost optimisation — the payload would not carry
                // `n`, and the generated code would silently resolve it to the
                // enclosing function's own parameter, i.e. the outermost call's
                // argument. That is a wrong answer with no diagnostic, so a literal is
                // scanned rather than skipped.
                TokenTree::Literal(l) => format_captures(&l.to_string(), out),
                _ => {}
            }
        }
    }
    let mut out = HashSet::new();
    go(ts, &mut out);
    out
}

/// The names a format string captures implicitly: `{n}` and `{n:?}` name `n`, and a
/// width or precision written `{:w$}` names `w`.
///
/// Over-approximating is the safe direction here. A brace-wrapped word in some
/// unrelated string could thread a local that is not really used, which at worst
/// moves it too early and fails to compile — loud, and far better than the silent
/// wrong answer that under-approximating gives.
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

/// Replace each payload marker — a loop's state or a resume point's frame — with its
/// parenthesised tuple.
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
