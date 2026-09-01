// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! The fixed half of what `#[stack_safe]` expands to.
//!
//! An expansion has two halves. One is particular to the function being rewritten: the
//! entry enum has a variant per entry point and the frame enum a variant per call site,
//! both carrying payloads whose types only that function's body implies. The other half
//! is the same for every function, and lives here rather than being emitted again into
//! each one:
//!
//! - [`Step`] and [`In`], the protocol between the rewritten body and its driver;
//! - [`drive`], the loop that keeps the recursion in a `Vec` instead of on the stack;
//! - [`Pin`], the store for values a call site lends its callee, under
//!   `#[stack_safe(data_in_frame)]`;
//! - [`Try`] and [`FromResidual`], a stable stand-in for the unstable traits of the
//!   same names, so that `?` works on a `Result` and on an `Option` alike.
//!
//! Nothing here is meant to be named by hand. It is `pub` because the expansions refer
//! to it by path, and the items are documented because a reader of an expansion should
//! be able to find out what they do.

#![no_std]

extern crate alloc;

use alloc::vec::Vec;

/// What the driver hands the body on each step.
pub enum In<A, F, R> {
    /// Run the body from an entry point.
    Enter(A),
    /// Continue a parked frame, with the result the callee produced.
    Resume(F, R),
}

/// What the body hands back.
pub enum Step<A, F, R> {
    /// This computation is finished; hand the value to the frame below.
    Done(R),
    /// Park `1` and enter `0`. The frame is a plain value in a `Vec`: one variant per
    /// call site, carrying the locals live across it, with the types left to inference.
    Call(A, F),
    /// Re-enter the body *without* parking a frame: the result belongs to whichever
    /// frame is already on top. This is what makes a loop iteration cost no stack.
    Tail(A),
}

/// Run a rewritten body to completion, keeping its frames on the heap.
///
/// `c` is the context the driver owns and lends out for the duration of each step: the
/// `&mut` parameters and any receiver, which cannot travel in a payload because two live
/// frames would then hold the same `&mut`. Lending it per step is what lets the body use
/// it at every level of the recursion without anything capturing it.
pub fn drive<C, A, F, R>(
    c: &mut C,
    init: A,
    mut body: impl FnMut(&mut C, In<A, F, R>) -> Step<A, F, R>,
) -> R {
    let mut frames: Vec<F> = Vec::new();
    let mut step = body(c, In::Enter(init));
    loop {
        match step {
            Step::Tail(args) => step = body(c, In::Enter(args)),
            Step::Call(args, frame) => {
                frames.push(frame);
                step = body(c, In::Enter(args));
            }
            Step::Done(r) => match frames.pop() {
                None => return r,
                Some(frame) => step = body(c, In::Resume(frame, r)),
            },
        }
    }
}

/// Storage for values a call site builds and lends to its callee.
///
/// Element addresses have to be stable. A pointer to one is handed to the callee and
/// stays live for that callee's whole subtree, during which further values are pushed,
/// so the chunks are pre-sized and never regrown: the outer `Vec` may move the chunk
/// *structs*, but never a chunk's buffer, and so never a value. That costs one
/// allocation per [`Pin::CHUNK`] values rather than one per value.
pub struct Pin<D> {
    chunks: Vec<Vec<D>>,
    len: usize,
}

impl<D> Pin<D> {
    /// Values per chunk, i.e. how many pushes one allocation serves.
    pub const CHUNK: usize = 64;

    pub fn new() -> Self {
        Self {
            chunks: Vec::new(),
            len: 0,
        }
    }

    /// How much is live now, so a frame can record what to drop when it resumes.
    pub fn mark(&self) -> usize {
        self.len
    }

    /// Take ownership of `d` and hand back its address, which will not move until
    /// [`Pin::truncate`] drops it.
    pub fn push(&mut self, d: D) -> *const D {
        if self.chunks.last().is_none_or(|c| c.len() == c.capacity()) {
            self.chunks.push(Vec::with_capacity(Self::CHUNK));
        }
        let chunk = self.chunks.last_mut().expect("just pushed one");
        chunk.push(d);
        self.len += 1;
        &chunk[chunk.len() - 1] as *const D
    }

    /// Drop everything pushed since `mark`.
    ///
    /// A chunk that lies entirely above the mark is dropped whole, so unwinding a deep
    /// recursion costs one step per chunk rather than one per value. Only the chunk the
    /// mark falls inside is trimmed, and that too in one `Vec::truncate` rather than a
    /// pop per element. The trimmed chunk keeps its capacity, so the next push reuses it
    /// and the addresses of the values still live do not move.
    pub fn truncate(&mut self, mark: usize) {
        while self.len > mark {
            let chunk_len = self.chunks.last().map_or(0, Vec::len);
            if self.len >= mark + chunk_len {
                // Nothing in this chunk is still live.
                self.chunks.pop();
                self.len -= chunk_len;
            } else {
                let chunk = self.chunks.last_mut().expect("len > mark, so one is live");
                chunk.truncate(chunk_len + mark - self.len);
                self.len = mark;
            }
        }
    }
}

impl<D> Default for Pin<D> {
    fn default() -> Self {
        Self::new()
    }
}

/// The residual of a `Result`: the error, still to be widened by `From`.
pub struct ResultErr<E>(pub E);

/// The residual of an `Option`, which carries nothing.
pub struct OptionNone;

/// `core::ops::Try::branch`, on stable.
///
/// `?` has to be desugared by hand, because it returns early and every early exit has to
/// become `Step::Done` instead. The obvious desugaring hardcodes `Ok` / `Err` /
/// `From::from`, which is wrong for an `Option`; the real one goes through `Try` and
/// `FromResidual`, which are unstable. This pair stands in for them, with one impl per
/// carrier. A type with a hand-written `Try` impl is therefore still unsupported: the
/// error is a missing-impl one naming this trait by path.
pub trait Try {
    type Output;
    type Residual;
    fn branch(self) -> Result<Self::Output, Self::Residual>;
}

impl<T, E> Try for Result<T, E> {
    type Output = T;
    type Residual = ResultErr<E>;

    #[inline]
    fn branch(self) -> Result<T, ResultErr<E>> {
        match self {
            Ok(v) => Ok(v),
            Err(e) => Err(ResultErr(e)),
        }
    }
}

impl<T> Try for Option<T> {
    type Output = T;
    type Residual = OptionNone;

    #[inline]
    fn branch(self) -> Result<T, OptionNone> {
        match self {
            Some(v) => Ok(v),
            None => Err(OptionNone),
        }
    }
}

/// `core::ops::FromResidual::from_residual`, on stable.
///
/// `Self` is the *function's* return type, which the driver's annotated `let` pins, so
/// inference has both ends.
pub trait FromResidual<R> {
    fn from_residual(r: R) -> Self;
}

impl<T, E, F> FromResidual<ResultErr<E>> for Result<T, F>
where
    F: From<E>,
{
    #[inline]
    fn from_residual(r: ResultErr<E>) -> Self {
        Err(From::from(r.0))
    }
}

impl<T> FromResidual<OptionNone> for Option<T> {
    #[inline]
    fn from_residual(_: OptionNone) -> Self {
        None
    }
}
