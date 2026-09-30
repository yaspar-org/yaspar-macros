// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! What `#[stack_safe]` costs as the *resume dispatch* widens. Run as:
//!
//! ```text
//! cargo run --release --example perf_dispatch_width
//! ```
//!
//! `perf_contrast` measures one recursive call site, where the transform is free or
//! better. This measures the axis that `perf_contrast` holds fixed: how many recursive
//! call sites the cycle has, which is how many variants the frame enum has, and so how
//! wide the switch on the frame tag is.
//!
//! Everything else is held constant — one recursive function, one hot node kind (`N0`,
//! the only one the benchmarked tree is built from), the same per-node work, and a
//! return type that owns a heap allocation and is too big for a register, as a real
//! interpreter's is. The extra arms are never executed; they exist only to add call
//! sites. So any difference between the rows is the width of the dispatch and nothing
//! else.
//!
//! What it shows, on an M-series Mac, beside what it showed when the expansion handed a
//! closure to `drive` and answered with `Step` — the shape `ladder4_expansion` still is:
//!
//! ```text
//! call sites   depth 1024   was
//!          1        0.65x   0.18x
//!          3        0.77x   1.86x
//!          5        0.80x   1.74x
//!          9        0.66x   1.46x
//! ```
//!
//! The machine now beats native recursion at every width: native pays a large stack frame
//! per level where the machine pays a `Vec` push. The old shape's cliff between one call
//! site and three — where a resume became a switch on a tag read out of the `Vec`, against
//! native's static fall-through edge — is gone, and the rows are within noise of each other.
//! One call site *regressed*: it used to collapse into something extraordinary and now merely
//! does well, which was traded deliberately for the rows that a real recursion sits in.
//!
//! This is a guard, not a target, and the absolute figures move with the machine and the
//! toolchain. What a regression looks like is a row drifting back above 1x, or the ladder
//! below losing its ordering. Only figures from one run compare: the same unmodified code has
//! measured a 2x spread across runs, so a change is only real if it is real inside one table.

use std::hint::black_box;
use std::sync::Arc;
use std::time::Instant;
use yaspar_macros::stack_safe;

/// `N0` is the only variant the benchmarked tree uses. The rest are reached by no input
/// and are here to give the cycle more call sites.
pub enum E {
    Lit(Arc<String>),
    N0(Box<E>),
    N1(Box<E>, Box<E>),
    N2(Box<E>, Box<E>),
    N3(Box<E>, Box<E>),
    N4(Box<E>, Box<E>),
}

/// Owns an allocation and does not fit in a register, like an interpreter's value.
pub struct V(Arc<String>, [u64; 8]);
pub type R = Result<V, Box<str>>;

fn native(e: &E) -> R {
    match e {
        E::Lit(s) => Ok(V(Arc::clone(s), [0; 8])),
        E::N0(a) => {
            let mut v = native(a)?;
            v.1[0] ^= v.0.len() as u64;
            Ok(v)
        }
        E::N1(a, b) | E::N2(a, b) | E::N3(a, b) | E::N4(a, b) => {
            let x = native(a)?;
            let y = native(b)?;
            Ok(V(x.0, [y.1[0]; 8]))
        }
    }
}

// One call site: `N0` recurses, and the arms that would add more are folded into a
// single non-recursive answer.
#[stack_safe]
fn sites1(e: &E) -> R {
    match e {
        E::Lit(s) => Ok(V(Arc::clone(s), [0; 8])),
        E::N0(a) => {
            let mut v = sites1(a)?;
            v.1[0] ^= v.0.len() as u64;
            Ok(v)
        }
        E::N1(..) | E::N2(..) | E::N3(..) | E::N4(..) => {
            Err("not reached".to_string().into_boxed_str())
        }
    }
}

// Three call sites.
#[stack_safe]
fn sites3(e: &E) -> R {
    match e {
        E::Lit(s) => Ok(V(Arc::clone(s), [0; 8])),
        E::N0(a) => {
            let mut v = sites3(a)?;
            v.1[0] ^= v.0.len() as u64;
            Ok(v)
        }
        E::N1(a, b) => {
            let x = sites3(a)?;
            let y = sites3(b)?;
            Ok(V(x.0, [y.1[0]; 8]))
        }
        E::N2(..) | E::N3(..) | E::N4(..) => Err("not reached".to_string().into_boxed_str()),
    }
}

/// One frame variant per recursive call site in [`sites3`], named after the point in that
/// function each one resumes at.
enum Hand3Frame<'a> {
    /// in `N0`, after the recursive call returned: apply the xor and answer
    N0Resume,
    /// in `N1`, after the first call returned: `b` is still to evaluate
    N1AfterFirst(&'a E),
    /// in `N1`, after the second call returned: the first child's value, carried across
    N1AfterSecond(V),
}

/// [`sites3`] transformed by hand, for the same signature and the same `?` behaviour: what
/// the macro's three-call-site output is being compared against.
///
/// The difference from the macro's encoding is where a continuation lives. Here the value
/// being returned stays in a local and the code that followed each recursive call is reached
/// by *falling into* the unwind loop. The macro packs that value into `Step::Done`, then into
/// `In::Resume`, then re-enters one body and switches on the frame tag to find the same code.
///
/// Both park frames on the heap, so this is not measuring the cost of leaving the stack.
///
/// A variant of this that keeps the frame stack in a thread-local and `clear()`s it, rather
/// than allocating a `Vec` per call, measured 0.25x native at depth 4 against this one's
/// 0.84x — so most of what is left here is the one allocation per descent. Pooling needs the
/// frame type to be lifetime-free, hence a raw pointer instead of `&'a E`, which is why it is
/// described rather than written here.
fn hand3(e: &E) -> R {
    let mut frames: Vec<Hand3Frame<'_>> = Vec::new();
    let mut cur: &E = e;
    'descend: loop {
        let mut val: V = loop {
            match cur {
                E::Lit(s) => break V(Arc::clone(s), [0; 8]),
                E::N0(a) => {
                    frames.push(Hand3Frame::N0Resume);
                    cur = a;
                }
                E::N1(a, b) => {
                    frames.push(Hand3Frame::N1AfterFirst(b));
                    cur = a;
                }
                // `?` on an `Err`: the source returns at once, dropping the parked frames.
                E::N2(..) | E::N3(..) | E::N4(..) => {
                    return Err("not reached".to_string().into_boxed_str());
                }
            }
        };
        loop {
            match frames.pop() {
                None => return Ok(val),
                Some(Hand3Frame::N0Resume) => val.1[0] ^= val.0.len() as u64,
                Some(Hand3Frame::N1AfterFirst(b)) => {
                    frames.push(Hand3Frame::N1AfterSecond(val));
                    cur = b;
                    continue 'descend;
                }
                Some(Hand3Frame::N1AfterSecond(x)) => val = V(x.0, [val.1[0]; 8]),
            }
        }
    }
}

// Five call sites.
#[stack_safe]
fn sites5(e: &E) -> R {
    match e {
        E::Lit(s) => Ok(V(Arc::clone(s), [0; 8])),
        E::N0(a) => {
            let mut v = sites5(a)?;
            v.1[0] ^= v.0.len() as u64;
            Ok(v)
        }
        E::N1(a, b) => {
            let x = sites5(a)?;
            let y = sites5(b)?;
            Ok(V(x.0, [y.1[0]; 8]))
        }
        E::N2(a, b) => {
            let x = sites5(a)?;
            let y = sites5(b)?;
            Ok(V(x.0, [y.1[0]; 8]))
        }
        E::N3(..) | E::N4(..) => Err("not reached".to_string().into_boxed_str()),
    }
}

// Nine call sites.
#[stack_safe]
fn sites9(e: &E) -> R {
    match e {
        E::Lit(s) => Ok(V(Arc::clone(s), [0; 8])),
        E::N0(a) => {
            let mut v = sites9(a)?;
            v.1[0] ^= v.0.len() as u64;
            Ok(v)
        }
        E::N1(a, b) => {
            let x = sites9(a)?;
            let y = sites9(b)?;
            Ok(V(x.0, [y.1[0]; 8]))
        }
        E::N2(a, b) => {
            let x = sites9(a)?;
            let y = sites9(b)?;
            Ok(V(x.0, [y.1[0]; 8]))
        }
        E::N3(a, b) => {
            let x = sites9(a)?;
            let y = sites9(b)?;
            Ok(V(x.0, [y.1[0]; 8]))
        }
        E::N4(a, b) => {
            let x = sites9(a)?;
            let y = sites9(b)?;
            Ok(V(x.0, [y.1[0]; 8]))
        }
    }
}

/// `hand3`, but with the generic enums the transform is obliged to use -- its payload types are
/// never written down -- and with payloads held directly rather than in one-tuples.
///
/// This is the open question in one function. `hand3` is fast and concrete; what the macro emits
/// is one loop and generic. If *this* is fast, the two-loop shape survives genericity and is
/// worth emitting. If it is slow, genericity is the wall and restructuring cannot pay.
enum GEntry<A0> {
    E0(A0),
}
enum GFrame<F0, F1, F2> {
    R0(F0),
    R1(F1),
    R2(F2),
}

fn hand3_generic(root: &E) -> R {
    let mut frames: Vec<GFrame<(), &E, V>> = Vec::new();
    let mut cur: GEntry<&E> = GEntry::E0(root);
    'descend: loop {
        let mut val: R = loop {
            match cur {
                GEntry::E0(x) => match x {
                    E::Lit(s) => break Ok(V(Arc::clone(s), [0; 8])),
                    E::N0(a) => {
                        frames.push(GFrame::R0(()));
                        cur = GEntry::E0(a);
                    }
                    E::N1(a, b) => {
                        frames.push(GFrame::R1(b));
                        cur = GEntry::E0(a);
                    }
                    E::N2(..) | E::N3(..) | E::N4(..) => {
                        return Err("not reached".to_string().into_boxed_str());
                    }
                },
            }
        };
        loop {
            match frames.pop() {
                None => return val,
                Some(f) => {
                    let mut v = match val {
                        Ok(v) => v,
                        Err(e) => return Err(e),
                    };
                    match f {
                        GFrame::R0(()) => {
                            v.1[0] ^= v.0.len() as u64;
                            val = Ok(v);
                        }
                        GFrame::R1(b) => {
                            frames.push(GFrame::R2(v));
                            cur = GEntry::E0(b);
                            continue 'descend;
                        }
                        GFrame::R2(x) => val = Ok(V(x.0, [v.1[0]; 8])),
                    }
                }
            }
        }
    }
}

// ---- the ladder: hand3_generic -> the macro's expansion, one change per rung ----
// Every rung keeps generic enums and heap frames. Read the deltas, not the absolutes.

type GF<'a> = GFrame<(), &'a E, V>;

/// L1: one loop instead of two. "Which phase am I in" moves from the program counter into a
/// data tag. Everything else is `hand3_generic`.
enum L1In<'a> {
    Enter(GEntry<&'a E>),
    Resume(GF<'a>, R),
}

fn ladder1_one_loop(root: &E) -> R {
    let mut frames: Vec<GF<'_>> = Vec::new();
    let mut st = L1In::Enter(GEntry::E0(root));
    loop {
        st = match st {
            L1In::Enter(GEntry::E0(x)) => match x {
                E::Lit(s) => {
                    let v = V(Arc::clone(s), [0; 8]);
                    match frames.pop() {
                        None => return Ok(v),
                        Some(f) => L1In::Resume(f, Ok(v)),
                    }
                }
                E::N0(a) => {
                    frames.push(GFrame::R0(()));
                    L1In::Enter(GEntry::E0(a))
                }
                E::N1(a, b) => {
                    frames.push(GFrame::R1(b));
                    L1In::Enter(GEntry::E0(a))
                }
                E::N2(..) | E::N3(..) | E::N4(..) => {
                    return Err("not reached".to_string().into_boxed_str());
                }
            },
            L1In::Resume(f, r) => {
                let mut v = match r {
                    Ok(v) => v,
                    Err(e) => return Err(e),
                };
                match f {
                    GFrame::R0(()) => {
                        v.1[0] ^= v.0.len() as u64;
                        match frames.pop() {
                            None => return Ok(v),
                            Some(f2) => L1In::Resume(f2, Ok(v)),
                        }
                    }
                    GFrame::R1(b) => {
                        frames.push(GFrame::R2(v));
                        L1In::Enter(GEntry::E0(b))
                    }
                    GFrame::R2(x) => {
                        let nv = V(x.0, [v.1[0]; 8]);
                        match frames.pop() {
                            None => return Ok(nv),
                            Some(f2) => L1In::Resume(f2, Ok(nv)),
                        }
                    }
                }
            }
        };
    }
}

/// L2: L1 plus the driver protocol -- the body answers with a `Step` and a second match does the
/// push and pop, instead of the body doing them itself.
enum L2Step<'a> {
    Done(R),
    Call(GEntry<&'a E>, GF<'a>),
    #[allow(dead_code)]
    Tail(GEntry<&'a E>),
}

fn ladder2_step_protocol(root: &E) -> R {
    let mut frames: Vec<GF<'_>> = Vec::new();
    let mut input = L1In::Enter(GEntry::E0(root));
    loop {
        let step = match input {
            L1In::Enter(GEntry::E0(x)) => match x {
                E::Lit(s) => L2Step::Done(Ok(V(Arc::clone(s), [0; 8]))),
                E::N0(a) => L2Step::Call(GEntry::E0(a), GFrame::R0(())),
                E::N1(a, b) => L2Step::Call(GEntry::E0(a), GFrame::R1(b)),
                E::N2(..) | E::N3(..) | E::N4(..) => {
                    L2Step::Done(Err("not reached".to_string().into_boxed_str()))
                }
            },
            L1In::Resume(f, r) => {
                let mut v = match r {
                    Ok(v) => v,
                    Err(e) => return Err(e),
                };
                match f {
                    GFrame::R0(()) => {
                        v.1[0] ^= v.0.len() as u64;
                        L2Step::Done(Ok(v))
                    }
                    GFrame::R1(b) => L2Step::Call(GEntry::E0(b), GFrame::R2(v)),
                    GFrame::R2(x) => L2Step::Done(Ok(V(x.0, [v.1[0]; 8]))),
                }
            }
        };
        match step {
            L2Step::Tail(a) => input = L1In::Enter(a),
            L2Step::Call(a, f) => {
                frames.push(f);
                input = L1In::Enter(a);
            }
            L2Step::Done(r) => match frames.pop() {
                None => return r,
                Some(f) => input = L1In::Resume(f, r),
            },
        }
    }
}

/// L3: L2 with every payload wrapped in a one-tuple, as the expansion writes them.
enum TEntry<A0> {
    E0(A0),
}
enum TFrame<F0, F1, F2> {
    R0(F0),
    R1(F1),
    R2(F2),
}
type TF<'a> = TFrame<(), (&'a E,), (V,)>;
enum L3In<'a> {
    Enter(TEntry<(&'a E,)>),
    Resume(TF<'a>, R),
}
enum L3Step<'a> {
    Done(R),
    Call(TEntry<(&'a E,)>, TF<'a>),
    #[allow(dead_code)]
    Tail(TEntry<(&'a E,)>),
}

fn ladder3_tuple_payloads(root: &E) -> R {
    let mut frames: Vec<TF<'_>> = Vec::new();
    let mut input = L3In::Enter(TEntry::E0((root,)));
    loop {
        let step = match input {
            L3In::Enter(TEntry::E0((x,))) => match x {
                E::Lit(s) => L3Step::Done(Ok(V(Arc::clone(s), [0; 8]))),
                E::N0(a) => L3Step::Call(TEntry::E0((a,)), TFrame::R0(())),
                E::N1(a, b) => L3Step::Call(TEntry::E0((a,)), TFrame::R1((b,))),
                E::N2(..) | E::N3(..) | E::N4(..) => {
                    L3Step::Done(Err("not reached".to_string().into_boxed_str()))
                }
            },
            L3In::Resume(f, r) => {
                let mut v = match r {
                    Ok(v) => v,
                    Err(e) => return Err(e),
                };
                match f {
                    TFrame::R0(()) => {
                        v.1[0] ^= v.0.len() as u64;
                        L3Step::Done(Ok(v))
                    }
                    TFrame::R1((b,)) => L3Step::Call(TEntry::E0((b,)), TFrame::R2((v,))),
                    TFrame::R2((x,)) => L3Step::Done(Ok(V(x.0, [v.1[0]; 8]))),
                }
            }
        };
        match step {
            L3Step::Tail(a) => input = L3In::Enter(a),
            L3Step::Call(a, f) => {
                frames.push(f);
                input = L3In::Enter(a);
            }
            L3Step::Done(r) => match frames.pop() {
                None => return r,
                Some(f) => input = L3In::Resume(f, r),
            },
        }
    }
}

/// L3c: the arms live in a closure, as they must if their payload types are to be inferred from
/// a signature, but the closure is called from a *single* site inside the loop in this function
/// rather than from the four sites inside `drive`. If this is as fast as L3, an expansion can
/// have both: inference from the closure's signature, and the loop state in registers.
fn ladder3c_one_call_site(root: &E) -> R {
    fn body<'a>(input: L3In<'a>) -> L3Step<'a> {
        match input {
            L3In::Enter(TEntry::E0((x,))) => match x {
                E::Lit(s) => L3Step::Done(Ok(V(Arc::clone(s), [0; 8]))),
                E::N0(a) => L3Step::Call(TEntry::E0((a,)), TFrame::R0(())),
                E::N1(a, b) => L3Step::Call(TEntry::E0((a,)), TFrame::R1((b,))),
                E::N2(..) | E::N3(..) | E::N4(..) => {
                    L3Step::Done(Err("not reached".to_string().into_boxed_str()))
                }
            },
            L3In::Resume(f, r) => {
                let mut v = match r {
                    Ok(v) => v,
                    Err(e) => return L3Step::Done(Err(e)),
                };
                match f {
                    TFrame::R0(()) => {
                        v.1[0] ^= v.0.len() as u64;
                        L3Step::Done(Ok(v))
                    }
                    TFrame::R1((b,)) => L3Step::Call(TEntry::E0((b,)), TFrame::R2((v,))),
                    TFrame::R2((x,)) => L3Step::Done(Ok(V(x.0, [v.1[0]; 8]))),
                }
            }
        }
    }
    let mut frames: Vec<TF<'_>> = Vec::new();
    let mut input = L3In::Enter(TEntry::E0((root,)));
    loop {
        match body(input) {
            L3Step::Tail(a) => input = L3In::Enter(a),
            L3Step::Call(a, f) => {
                frames.push(f);
                input = L3In::Enter(a);
            }
            L3Step::Done(r) => match frames.pop() {
                None => return r,
                Some(f) => input = L3In::Resume(f, r),
            },
        }
    }
}

/// L3b: L3 with the transition written *above* the arms and the loop seeded with a `Tail`.
/// An expansion has to be written this way if the arms are to type-check -- payload types that
/// only the arms construct are otherwise still inference variables when the arms are checked.
/// So this rung asks whether that ordering costs anything.
fn ladder3b_transition_first(root: &E) -> R {
    let mut frames: Vec<TF<'_>> = Vec::new();
    let mut input;
    let mut step = L3Step::Tail(TEntry::E0((root,)));
    loop {
        match step {
            L3Step::Tail(a) => input = L3In::Enter(a),
            L3Step::Call(a, f) => {
                frames.push(f);
                input = L3In::Enter(a);
            }
            L3Step::Done(r) => match frames.pop() {
                None => return r,
                Some(f) => input = L3In::Resume(f, r),
            },
        }
        step = match input {
            L3In::Enter(TEntry::E0((x,))) => match x {
                E::Lit(s) => L3Step::Done(Ok(V(Arc::clone(s), [0; 8]))),
                E::N0(a) => L3Step::Call(TEntry::E0((a,)), TFrame::R0(())),
                E::N1(a, b) => L3Step::Call(TEntry::E0((a,)), TFrame::R1((b,))),
                E::N2(..) | E::N3(..) | E::N4(..) => {
                    L3Step::Done(Err("not reached".to_string().into_boxed_str()))
                }
            },
            L3In::Resume(f, r) => {
                let mut v = match r {
                    Ok(v) => v,
                    Err(e) => return Err(e),
                };
                match f {
                    TFrame::R0(()) => {
                        v.1[0] ^= v.0.len() as u64;
                        L3Step::Done(Ok(v))
                    }
                    TFrame::R1((b,)) => L3Step::Call(TEntry::E0((b,)), TFrame::R2((v,))),
                    TFrame::R2((x,)) => L3Step::Done(Ok(V(x.0, [v.1[0]; 8]))),
                }
            }
        };
    }
}

/// L4a: L3 plus the closure handed to `drive`, but the residual still a plain `match`.
fn ladder4a_closure(root: &E) -> R {
    use yaspar_macros_defs::{In, Step, drive};
    drive(
        &mut (),
        TEntry::E0((root,)),
        |_ctx, input: In<_, TF<'_>, R>| match input {
            In::Enter(TEntry::E0((x,))) => match x {
                E::Lit(s) => Step::Done(Ok(V(Arc::clone(s), [0; 8]))),
                E::N0(a) => Step::Call(TEntry::E0((&**a,)), TFrame::R0(())),
                E::N1(a, b) => Step::Call(TEntry::E0((&**a,)), TFrame::R1((&**b,))),
                E::N2(..) | E::N3(..) | E::N4(..) => {
                    Step::Done(Err("not reached".to_string().into_boxed_str()))
                }
            },
            In::Resume(f, r) => {
                let mut v = match r {
                    Ok(v) => v,
                    Err(e) => return Step::Done(Err(e)),
                };
                match f {
                    TFrame::R0(()) => {
                        v.1[0] ^= v.0.len() as u64;
                        Step::Done(Ok(v))
                    }
                    TFrame::R1((b,)) => Step::Call(TEntry::E0((b,)), TFrame::R2((v,))),
                    TFrame::R2((x,)) => Step::Done(Ok(V(x.0, [v.1[0]; 8]))),
                }
            }
        },
    )
}

/// L4b: L3 plus `Try`/`FromResidual`, but the loop still written in the function.
fn ladder4b_try(root: &E) -> R {
    use yaspar_macros_defs::{FromResidual, Try};
    let mut frames: Vec<TF<'_>> = Vec::new();
    let mut input = L3In::Enter(TEntry::E0((root,)));
    loop {
        let step = match input {
            L3In::Enter(TEntry::E0((x,))) => match x {
                E::Lit(s) => L3Step::Done(Ok(V(Arc::clone(s), [0; 8]))),
                E::N0(a) => L3Step::Call(TEntry::E0((a,)), TFrame::R0(())),
                E::N1(a, b) => L3Step::Call(TEntry::E0((a,)), TFrame::R1((b,))),
                E::N2(..) | E::N3(..) | E::N4(..) => {
                    L3Step::Done(Err("not reached".to_string().into_boxed_str()))
                }
            },
            L3In::Resume(f, r) => match f {
                TFrame::R0(()) => match Try::branch(r) {
                    Ok(mut v) => {
                        v.1[0] ^= v.0.len() as u64;
                        L3Step::Done(Ok(v))
                    }
                    Err(res) => L3Step::Done(FromResidual::from_residual(res)),
                },
                TFrame::R1((b,)) => match Try::branch(r) {
                    Ok(v) => L3Step::Call(TEntry::E0((b,)), TFrame::R2((v,))),
                    Err(res) => L3Step::Done(FromResidual::from_residual(res)),
                },
                TFrame::R2((x,)) => match Try::branch(r) {
                    Ok(v) => L3Step::Done(Ok(V(x.0, [v.1[0]; 8]))),
                    Err(res) => L3Step::Done(FromResidual::from_residual(res)),
                },
            },
        };
        match step {
            L3Step::Tail(a) => input = L3In::Enter(a),
            L3Step::Call(a, f) => {
                frames.push(f);
                input = L3In::Enter(a);
            }
            L3Step::Done(r) => match frames.pop() {
                None => return r,
                Some(f) => input = L3In::Resume(f, r),
            },
        }
    }
}

/// L4: L3 with the body handed to `drive` as a closure, and the residual routed through
/// `Try`/`FromResidual`. This is what the expansion *was*, written out: the rung the current
/// one is measured against. `ladder1_one_loop` above is what it is now, near enough.
fn ladder4_expansion(root: &E) -> R {
    use yaspar_macros_defs::{FromResidual, In, Step, Try, drive};
    drive(
        &mut (),
        TEntry::E0((root,)),
        |_ctx, input: In<_, TF<'_>, R>| match input {
            In::Enter(TEntry::E0((x,))) => match x {
                E::Lit(s) => Step::Done(Ok(V(Arc::clone(s), [0; 8]))),
                E::N0(a) => Step::Call(TEntry::E0((&**a,)), TFrame::R0(())),
                E::N1(a, b) => Step::Call(TEntry::E0((&**a,)), TFrame::R1((&**b,))),
                E::N2(..) | E::N3(..) | E::N4(..) => {
                    Step::Done(Err("not reached".to_string().into_boxed_str()))
                }
            },
            In::Resume(f, r) => match f {
                TFrame::R0(()) => match Try::branch(r) {
                    Ok(mut v) => {
                        v.1[0] ^= v.0.len() as u64;
                        Step::Done(Ok(v))
                    }
                    Err(res) => Step::Done(FromResidual::from_residual(res)),
                },
                TFrame::R1((b,)) => match Try::branch(r) {
                    Ok(v) => Step::Call(TEntry::E0((b,)), TFrame::R2((v,))),
                    Err(res) => Step::Done(FromResidual::from_residual(res)),
                },
                TFrame::R2((x,)) => match Try::branch(r) {
                    Ok(v) => Step::Done(Ok(V(x.0, [v.1[0]; 8]))),
                    Err(res) => Step::Done(FromResidual::from_residual(res)),
                },
            },
        },
    )
}

fn chain(depth: usize) -> E {
    let mut n = E::Lit(Arc::new("x".repeat(8)));
    for _ in 0..depth {
        n = E::N0(Box::new(n));
    }
    n
}

fn time(mut f: impl FnMut(), iters: u64) -> f64 {
    for _ in 0..iters / 10 {
        f();
    }
    let t = Instant::now();
    for _ in 0..iters {
        f();
    }
    t.elapsed().as_nanos() as f64 / iters as f64
}

fn main() {
    // Shallow enough that a tree of this depth is what an interpreter actually sees, and
    // deep enough to show where the two curves cross.
    for depth in [4usize, 1024] {
        let e = chain(depth);
        let iters = if depth > 512 { 50_000 } else { 500_000 };
        let base = time(
            || {
                black_box(native(black_box(&e))).ok();
            },
            iters,
        );
        println!("depth {depth}, native {base:.0} ns");
        for (sites, ns) in [
            (
                1,
                time(
                    || {
                        black_box(sites1(black_box(&e))).ok();
                    },
                    iters,
                ),
            ),
            (
                3,
                time(
                    || {
                        black_box(sites3(black_box(&e))).ok();
                    },
                    iters,
                ),
            ),
            (
                5,
                time(
                    || {
                        black_box(sites5(black_box(&e))).ok();
                    },
                    iters,
                ),
            ),
            (
                9,
                time(
                    || {
                        black_box(sites9(black_box(&e))).ok();
                    },
                    iters,
                ),
            ),
        ] {
            println!(
                "  {sites:>2} call sites: {ns:8.0} ns  {:5.2}x native",
                ns / base
            );
        }
        let hand = time(
            || {
                black_box(hand3(black_box(&e))).ok();
            },
            iters,
        );
        println!(
            "   3 by hand:    {hand:8.0} ns  {:5.2}x native",
            hand / base
        );
        let handg = time(
            || {
                black_box(hand3_generic(black_box(&e))).ok();
            },
            iters,
        );
        println!(
            "   L0 two loops, generic:    {handg:8.0} ns  {:5.2}x native",
            handg / base
        );
        for (name, ns) in [
            (
                "L1 + one loop (phase in tag)",
                time(
                    || {
                        black_box(ladder1_one_loop(black_box(&e))).ok();
                    },
                    iters,
                ),
            ),
            (
                "L2 + Step/driver protocol   ",
                time(
                    || {
                        black_box(ladder2_step_protocol(black_box(&e))).ok();
                    },
                    iters,
                ),
            ),
            (
                "L3 + tuple payloads         ",
                time(
                    || {
                        black_box(ladder3_tuple_payloads(black_box(&e))).ok();
                    },
                    iters,
                ),
            ),
            (
                "L3c closure, 1 call site    ",
                time(
                    || {
                        black_box(ladder3c_one_call_site(black_box(&e))).ok();
                    },
                    iters,
                ),
            ),
            (
                "L3b transition above arms   ",
                time(
                    || {
                        black_box(ladder3b_transition_first(black_box(&e))).ok();
                    },
                    iters,
                ),
            ),
            (
                "L4a + closure/drive only    ",
                time(
                    || {
                        black_box(ladder4a_closure(black_box(&e))).ok();
                    },
                    iters,
                ),
            ),
            (
                "L4b + Try only              ",
                time(
                    || {
                        black_box(ladder4b_try(black_box(&e))).ok();
                    },
                    iters,
                ),
            ),
            (
                "L4 + both (= old expansion)  ",
                time(
                    || {
                        black_box(ladder4_expansion(black_box(&e))).ok();
                    },
                    iters,
                ),
            ),
        ] {
            println!("   {name} {ns:8.0} ns  {:5.2}x native", ns / base);
        }
    }
}
