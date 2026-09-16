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
//!   same names, so that `?` works on a `Result`, an `Option` and a `ControlFlow` alike.
//!
//! Nothing here is meant to be named by hand, except [`Try`] and [`FromResidual`]: those are
//! how a carrier of your own joins `?`. It is all `pub` because the expansions refer to it by
//! path, and documented because a reader of an expansion should be able to find out what it does.

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
    ///
    /// The returned pointer stays valid across *later* `push`es, which is the whole
    /// point of the type: a caller lends a value to its callee's entire subtree, and
    /// that subtree pushes more values of its own before reading this one back.
    pub fn push(&mut self, d: D) -> *const D {
        if self.chunks.last().is_none_or(|c| c.len() == c.capacity()) {
            self.chunks.push(Vec::with_capacity(Self::CHUNK));
        }
        let chunk = self.chunks.last_mut().expect("just pushed one");
        chunk.push(d);
        self.len += 1;
        // SAFETY: a later `push` re-takes `&mut self` and writes the same chunk, which
        // would invalidate this pointer under either aliasing model *if* its provenance
        // came from that borrow of `self`. It does not: the address belongs to the heap
        // allocation the inner `Vec` owns, and the borrow of `self` only reads that
        // buffer pointer out. A later push therefore writes a different element of the
        // same allocation and leaves this one alone. The buffer never moves either, as
        // chunks are pre-sized and never regrown and `truncate` keeps their capacity;
        // the outer `Vec` may move the chunk *headers* when its spine grows, which does
        // not move the buffers they own. Checked, not just argued: this interleaving and
        // the macro-expanded tests report no UB under both `-Zmiri-stacked-borrows` and
        // `-Zmiri-tree-borrows` with `-Zmiri-strict-provenance` (see the README), so
        // keep those tests in step with any change to the chunking above.
        &chunk[chunk.len() - 1] as *const D
    }

    /// Push `d` and hand back the address of a place inside it, which `project` reaches.
    ///
    /// One store serves values of several shapes by holding an enum, so a caller that wants a
    /// pointer to what is *inside* a variant would have to dereference the pushed pointer itself.
    /// It happens here instead, where the safety argument for that dereference is one line: the
    /// value was just pushed and [`Pin`] never moves what it holds.
    pub fn push_projected<E: ?Sized>(&mut self, d: D, project: impl FnOnce(&D) -> &E) -> *const E {
        let at = self.push(d);
        // SAFETY: `at` is the value pushed on the line above, and nothing has run since; `Pin`
        // never moves a value it holds, so the address is live. The reference `project` receives
        // does not outlive this call — only the address it returns does, and that address is the
        // pushed value's, which lives until `truncate` or `take_last` reaches it.
        core::ptr::from_ref(project(unsafe { &*at }))
    }

    /// Take the value at `at` back out, dropping everything pushed after it.
    ///
    /// One store holds every shape a descent parks, so a frame that parked a value and then lent
    /// another one to the same call cannot ask for "the last": the lend sits on top of it. It knows
    /// where its own value went, though — the store's length before the call, plus its position
    /// among that call's pushes — and what is above it belongs to the call that has just returned,
    /// so dropping it here is what [`Pin::truncate`] would have done a moment later.
    pub fn take_at(&mut self, at: usize) -> Option<D> {
        if at >= self.len {
            return None;
        }
        self.truncate(at + 1);
        self.take_last()
    }

    /// Take the value pushed last back out, without dropping it.
    ///
    /// A frame that parked a value to lend a place inside it owns that value again once the callee
    /// has returned, so the resume arm takes it back rather than letting [`Pin::truncate`] drop it.
    /// The slot's chunk keeps its capacity, exactly as `truncate` leaves it.
    pub fn take_last(&mut self) -> Option<D> {
        // Taking the last value of a chunk leaves it empty, and `push` only ever appends to the
        // last one, so an empty chunk on top is dropped rather than searched past.
        while self.chunks.last().is_some_and(Vec::is_empty) {
            self.chunks.pop();
        }
        let chunk = self.chunks.last_mut()?;
        let d = chunk.pop()?;
        self.len -= 1;
        Some(d)
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

/// The residual of a `ControlFlow`: the value it broke with.
pub struct ControlFlowBreak<B>(pub B);

/// `core::ops::Try::branch`, on stable.
///
/// `?` has to be desugared by hand, because it returns early and every early exit has to
/// become `Step::Done` instead. The obvious desugaring hardcodes `Ok` / `Err` /
/// `From::from`, which is wrong for an `Option`; the real one goes through `Try` and
/// `FromResidual`, which are unstable. This pair stands in for them, with one impl per
/// carrier: `Result`, `Option` and `ControlFlow`, as in `core`.
///
/// # A carrier of your own
///
/// Neither trait is sealed, so implementing both for your own carrier makes `?` work on it
/// inside a `#[stack_safe]` body:
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
///
/// An existing `core::ops::Try` impl cannot be reused: a blanket impl over it would need that
/// unstable trait. Without the pair above, the error is a missing-impl one naming this trait.
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

impl<B, C> FromResidual<ControlFlowBreak<B>> for core::ops::ControlFlow<B, C> {
    #[inline]
    fn from_residual(r: ControlFlowBreak<B>) -> Self {
        core::ops::ControlFlow::Break(r.0)
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

    /// It reaches past a chunk boundary, so the index is the store's own and not a chunk's.
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

    /// Taking one back does not move the values still parked, which is what the pointers rely on.
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
