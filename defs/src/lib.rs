// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Runtime support for `#[stack_safe]` expansions.
//!
//! Expansions import [`Frames`], [`push`], [`Pin`], [`Try`], [`FromResidual`], [`range_peek`]
//! and [`range_at`]. [`In`], [`Step`] and [`drive`] are the older loop-as-a-function encoding,
//! kept as a benchmark baseline. Only [`Try`] and [`FromResidual`] are meant to be used by hand,
//! to make `?` work on a custom type.

#![no_std]

extern crate alloc;

use alloc::vec::Vec;
use core::ops::Range;

/// Input to one step of [`drive`].
pub enum In<A, F, R> {
    /// Run the body from an entry point.
    Enter(A),
    /// Continue a parked frame, with the result the callee produced.
    Resume(F, R),
}

/// Like [`In`], but `Resume` leaves the frame on the stack for the resume arm to pop, so the
/// state stays small enough to live in registers. Currently unused.
pub enum InSplit<A, R> {
    /// Run the body from an entry point.
    Enter(A),
    /// A callee's result, for the frame on top of the stack.
    Resume(R),
}

/// The heap stack of parked frames. An alias so expansions work in `no_std` crates.
///
/// Starts unallocated; the first [`push`] reserves [`FIRST_FRAMES`].
pub type Frames<F> = Vec<F>;

/// How many frames the first push onto an empty [`Frames`] makes room for.
pub const FIRST_FRAMES: usize = 64;

/// Park `frame` on `frames`. Growth happens out of line in [`push_grow`].
#[inline(always)]
pub fn push<F>(frames: &mut Frames<F>, frame: F) {
    if frames.len() == frames.capacity() {
        push_grow(frames, frame);
    } else {
        frames.push(frame);
    }
}

/// Slow path of [`push`]: reserve [`FIRST_FRAMES`] if empty, else double, then push.
#[cold]
#[inline(never)]
pub fn push_grow<F>(frames: &mut Frames<F>, frame: F) {
    if frames.capacity() == 0 {
        // Cheaper than `reserve`'s general growth path.
        *frames = Frames::with_capacity(FIRST_FRAMES);
    } else {
        frames.reserve(frames.capacity());
    }
    frames.push(frame);
}

/// What the body hands back, in the [`drive`] protocol.
pub enum Step<A, F, R> {
    /// This computation is finished; hand the value to the frame below.
    Done(R),
    /// Park frame `1` and enter `0`.
    Call(A, F),
    /// Re-enter the body without parking a frame (used for loop iterations).
    Tail(A),
}

/// Run `body` to completion with its frames on the heap, lending it `c` (the `&mut`
/// parameters and receiver) on each step.
///
/// Expansions no longer call this (they inline the loop, about 2x faster); it is kept as the
/// baseline for `examples/perf_dispatch_width.rs`.
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

/// The next value of `r`, without advancing it.
///
/// A lowered `for` over a range advances at the end of each iteration, so frames can recover
/// the current index with [`range_at`] instead of storing it.
#[inline]
pub fn range_peek<T: Clone>(r: &Range<T>) -> Option<T>
where
    Range<T>: Iterator<Item = T>,
{
    Iterator::next(&mut r.clone())
}

/// The value [`range_peek`] last returned for `r` (its `start`).
#[inline]
pub fn range_at<T: Clone>(r: &Range<T>) -> T {
    r.start.clone()
}

/// Address-stable storage for values a call site lends its callee (`data_in_frame`).
///
/// Values live in fixed-capacity chunks that are never regrown, so a value never moves
/// until it is dropped.
pub struct Pin<D> {
    chunks: Vec<Vec<D>>,
    len: usize,
}

impl<D> Pin<D> {
    /// Values per chunk.
    pub const CHUNK: usize = 64;

    pub fn new() -> Self {
        Self {
            chunks: Vec::new(),
            len: 0,
        }
    }

    /// The current length, to pass to [`Pin::truncate`] later.
    pub fn mark(&self) -> usize {
        self.len
    }

    /// Store `d` and return its address, valid across later pushes until it is dropped.
    pub fn push(&mut self, d: D) -> *const D {
        if self.chunks.last().is_none_or(|c| c.len() == c.capacity()) {
            self.chunks.push(Vec::with_capacity(Self::CHUNK));
        }
        let chunk = self.chunks.last_mut().expect("just pushed one");
        chunk.push(d);
        self.len += 1;
        // SAFETY: the pointer's provenance is the chunk's heap buffer, not the borrow of
        // `self`, so later pushes (which write other elements) do not invalidate it. The
        // buffer never moves: chunks are never regrown and `truncate` keeps capacity.
        // Verified under Miri with stacked and tree borrows; keep those tests in sync.
        &chunk[chunk.len() - 1] as *const D
    }

    /// Push `d` and return the address of the part of it selected by `project`.
    pub fn push_projected<E: ?Sized>(&mut self, d: D, project: impl FnOnce(&D) -> &E) -> *const E {
        let at = self.push(d);
        // SAFETY: `at` was just pushed and `Pin` never moves its values, so it is live. The
        // reference does not escape this call; only the address does.
        core::ptr::from_ref(project(unsafe { &*at }))
    }

    /// Remove and return the value at index `at`, dropping everything pushed after it.
    pub fn take_at(&mut self, at: usize) -> Option<D> {
        if at >= self.len {
            return None;
        }
        self.truncate(at + 1);
        self.take_last()
    }

    /// Remove and return the last value pushed.
    pub fn take_last(&mut self) -> Option<D> {
        // Drop empty chunks on top.
        while self.chunks.last().is_some_and(Vec::is_empty) {
            self.chunks.pop();
        }
        let chunk = self.chunks.last_mut()?;
        let d = chunk.pop()?;
        self.len -= 1;
        Some(d)
    }

    /// Drop everything pushed since `mark`, a whole chunk at a time where possible.
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

/// The residual of a `ControlFlow`: the value it broke with.
pub struct ControlFlowBreak<B>(pub B);

/// Stable stand-in for `core::ops::Try`, used to desugar `?` in rewritten bodies.
///
/// Implemented for `Result`, `Option` and `ControlFlow`. Implement it and [`FromResidual`]
/// to use `?` on your own type inside `#[stack_safe]`:
///
/// ```
/// use yaspar_macros_defs::{FromResidual, Try};
///
/// enum Maybe<T> { Just(T), Nothing }
/// struct NothingLeft;
///
/// impl<T> Try for Maybe<T> {
///     type Output = T;
///     type Residual = NothingLeft;
///     fn branch(self) -> Result<T, NothingLeft> {
///         match self {
///             Maybe::Just(v) => Ok(v),
///             Maybe::Nothing => Err(NothingLeft),
///         }
///     }
/// }
///
/// impl<T> FromResidual<NothingLeft> for Maybe<T> {
///     fn from_residual(_: NothingLeft) -> Self { Maybe::Nothing }
/// }
/// ```
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

impl<B, C> Try for core::ops::ControlFlow<B, C> {
    type Output = C;
    type Residual = ControlFlowBreak<B>;

    #[inline]
    fn branch(self) -> Result<C, ControlFlowBreak<B>> {
        match self {
            core::ops::ControlFlow::Continue(c) => Ok(c),
            core::ops::ControlFlow::Break(b) => Err(ControlFlowBreak(b)),
        }
    }
}

/// Stable stand-in for `core::ops::FromResidual`.
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

impl<B, C> FromResidual<ControlFlowBreak<B>> for core::ops::ControlFlow<B, C> {
    #[inline]
    fn from_residual(r: ControlFlowBreak<B>) -> Self {
        core::ops::ControlFlow::Break(r.0)
    }
}

#[cfg(test)]
mod frames_tests {
    use super::{FIRST_FRAMES, Frames, push};

    /// Starts unallocated, first push reserves `FIRST_FRAMES`, then doubles.
    #[test]
    fn first_push_reserves_a_block_then_doubles() {
        let mut frames: Frames<u64> = Frames::new();
        assert_eq!(frames.capacity(), 0);
        push(&mut frames, 0);
        assert_eq!(frames.capacity(), FIRST_FRAMES);
        for i in 1..FIRST_FRAMES as u64 {
            push(&mut frames, i);
        }
        assert_eq!(frames.capacity(), FIRST_FRAMES);
        push(&mut frames, FIRST_FRAMES as u64);
        assert_eq!(frames.capacity(), 2 * FIRST_FRAMES);
        let expect: alloc::vec::Vec<u64> = (0..=FIRST_FRAMES as u64).collect();
        assert_eq!(frames, expect);
    }

    /// A frame with no bytes never needs room, and is still counted.
    #[test]
    fn zero_sized_frames() {
        let mut frames: Frames<()> = Frames::new();
        for _ in 0..1000 {
            push(&mut frames, ());
        }
        assert_eq!(frames.len(), 1000);
    }
}

#[cfg(test)]
mod pin_tests {
    use super::Pin;

    /// A parked value comes back out, and what stays behind is still dropped once.
    #[test]
    fn take_last_returns_the_parked_value() {
        let mut pin: Pin<alloc::string::String> = Pin::new();
        let mark = pin.mark();
        let p = pin.push(alloc::string::String::from("parked"));
        assert_eq!(unsafe { &*p }, "parked");
        assert_eq!(pin.take_last().as_deref(), Some("parked"));
        assert_eq!(pin.mark(), mark, "taking it back leaves nothing behind");
        assert_eq!(pin.take_last(), None, "and nothing else to take");
    }

    /// The projection points *inside* the pushed value, and stays valid as more is pushed.
    #[test]
    fn push_projected_points_inside_the_value() {
        struct Held {
            head: u64,
            tail: u64,
        }

        let mut pin: Pin<Held> = Pin::new();
        let head = pin.push_projected(Held { head: 1, tail: 2 }, |h| &h.head);
        let tail = pin.push_projected(Held { head: 3, tail: 4 }, |h| &h.tail);
        assert_eq!((unsafe { *head }, unsafe { *tail }), (1, 4));
        for i in 0..(Pin::<Held>::CHUNK as u64 * 2) {
            pin.push(Held { head: i, tail: i });
        }
        assert_eq!(
            (unsafe { *head }, unsafe { *tail }),
            (1, 4),
            "later pushes do not move either"
        );
        pin.truncate(0);
    }

    /// Taking a value out from under a later push drops what was above it.
    #[test]
    fn take_at_drops_what_sits_above() {
        let mut pin: Pin<alloc::string::String> = Pin::new();
        let mark = pin.mark();
        pin.push(alloc::string::String::from("parked"));
        pin.push(alloc::string::String::from("lent later"));
        assert_eq!(pin.take_at(mark).as_deref(), Some("parked"));
        assert_eq!(pin.mark(), mark, "and the store is back where it started");
        assert_eq!(pin.take_at(mark), None, "nothing at that index any more");
    }

    /// `take_at` indexes across chunk boundaries.
    #[test]
    fn take_at_crosses_chunks() {
        let mut pin: Pin<u64> = Pin::new();
        let mark = pin.mark();
        for i in 0..(Pin::<u64>::CHUNK as u64 * 3) {
            pin.push(i);
        }
        assert_eq!(pin.take_at(mark + Pin::<u64>::CHUNK), Some(64));
        assert_eq!(pin.mark(), mark + Pin::<u64>::CHUNK);
        pin.truncate(mark);
    }

    /// `take_last` does not move the remaining values.
    #[test]
    fn take_last_leaves_other_addresses_alone() {
        let mut pin: Pin<u64> = Pin::new();
        let first = pin.push(1);
        let second = pin.push(2);
        assert_eq!(pin.take_last(), Some(2));
        assert_eq!(unsafe { *first }, 1);
        let third = pin.push(3);
        assert_eq!(unsafe { *first }, 1);
        assert_eq!(
            third, second,
            "the slot is reused, as `truncate` would leave it"
        );
        pin.truncate(0);
    }
}
