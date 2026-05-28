//! Single-writer / multi-reader "latest value wins" channel.
//!
//! This is the transport for *sampled* signals — high-frequency, lossy-
//! tolerant data where the consumer only ever wants the most recent
//! value and missing intermediate values is harmless (audio meter
//! levels, the periodic stats roll-up). It is deliberately **not** a
//! substitute for the projection event bus, which carries *discrete*
//! facts (user joined/left, mute toggled, chat arrived) that must not be
//! dropped, must stay ordered, and fan out to several subscribers.
//!
//! Rule of thumb: if you find yourself wanting to skip a repaint for an
//! event because it fires too often, that event probably wants to be a
//! [`Snapshot`] instead.
//!
//! ## Why not just `ArcSwap<T>` with `store(Arc::new(v))`?
//!
//! That allocates an `Arc` on every store. For the meter that store
//! happens inside the real-time cpal capture callback ~50×/s, and
//! allocating in an audio callback is a latency hazard. [`SnapshotWriter`]
//! avoids it: after swapping a new value in, it reclaims the previous
//! `Arc` as a spare and writes the *next* value into that same
//! allocation. In steady state (readers grab-copy-drop quickly) the
//! writer ping-pongs a single buffer and never touches the allocator.
//!
//! ### Soundness of the reclaim
//!
//! Readers use [`ArcSwap::load_full`], which hands back a real `Arc`
//! with an honest strong count. After `swap`, the old `Arc` is no longer
//! reachable through the cell, so no *new* reader can take a reference to
//! it — its strong count is monotonically non-increasing. If it reads as
//! `1`, the writer is the sole owner and `Arc::get_mut` into it is sound;
//! if a reader is still mid-read (count > 1) the writer conservatively
//! allocates a fresh buffer instead. Either way there is no data race and
//! no `unsafe`.

use std::sync::Arc;

use arc_swap::ArcSwap;

/// Reader handle. Cheap to clone (an `Arc` bump); hand one to every
/// consumer that wants to sample the value. Reading never blocks the
/// writer and never allocates.
#[derive(Clone)]
pub struct Snapshot<T> {
    cell: Arc<ArcSwap<T>>,
}

/// Writer handle. The producer stores the latest value here.
///
/// `Clone` is provided so the same logical channel can be re-armed
/// across producer restarts (e.g. the audio capture stream is torn down
/// and rebuilt on device change, each incarnation needing its own
/// writer). The contract is **single-producer-at-a-time**: storing from
/// two clones concurrently is memory-safe (readers always observe a
/// valid published value) but defeats buffer reclaiming, so don't.
#[derive(Clone)]
pub struct SnapshotWriter<T> {
    cell: Arc<ArcSwap<T>>,
    /// The previously-published `Arc`, reclaimed once no reader holds it,
    /// reused as the next store's backing allocation. `None` forces a
    /// fresh allocation (first store, or a store that raced a reader).
    spare: Option<Arc<T>>,
}

impl<T: Copy> Snapshot<T> {
    /// Create a channel seeded with `initial`, returning the writer and
    /// a reader. Clone the reader for additional consumers.
    pub fn new(initial: T) -> (SnapshotWriter<T>, Snapshot<T>) {
        let cell = Arc::new(ArcSwap::from_pointee(initial));
        (
            SnapshotWriter {
                cell: cell.clone(),
                spare: None,
            },
            Snapshot { cell },
        )
    }

    /// Read the most recently published value. Lock-free; copies the
    /// value out and releases its `Arc` immediately so the writer can
    /// reclaim the buffer.
    pub fn load(&self) -> T {
        *self.cell.load_full()
    }
}

impl<T: Copy> SnapshotWriter<T> {
    /// Publish `value` as the new latest. Reuses the reclaimed spare
    /// buffer when available, otherwise allocates once.
    pub fn store(&mut self, value: T) {
        let new = match self.spare.take() {
            // Spare is only ever stashed when it was uniquely owned, and
            // nothing can have grabbed it since (it isn't published), so
            // `get_mut` succeeds in the steady state. The `None` arm is
            // defensive — a fresh allocation is always correct.
            Some(mut arc) => match Arc::get_mut(&mut arc) {
                Some(slot) => {
                    *slot = value;
                    arc
                }
                None => Arc::new(value),
            },
            None => Arc::new(value),
        };

        let old = self.cell.swap(new);
        // Reclaim only when we're the sole owner — see module docs for
        // why `strong_count == 1` is sound here.
        if Arc::strong_count(&old) == 1 {
            self.spare = Some(old);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_returns_latest_store() {
        let (mut w, r) = Snapshot::new(0u32);
        assert_eq!(r.load(), 0);
        w.store(7);
        assert_eq!(r.load(), 7);
        w.store(42);
        assert_eq!(r.load(), 42);
    }

    #[test]
    fn clones_observe_same_channel() {
        let (mut w, r1) = Snapshot::new(0u32);
        let r2 = r1.clone();
        w.store(99);
        assert_eq!(r1.load(), 99);
        assert_eq!(r2.load(), 99);
    }

    #[test]
    fn steady_state_reclaims_buffer() {
        // With no reader holding a value between stores, the writer
        // should reclaim and reuse a single allocation: after the first
        // store the spare is primed, and subsequent stores reuse it.
        let (mut w, r) = Snapshot::new(0u32);

        w.store(1);
        let _ = r.load(); // grab + drop, leaving the published Arc unique
        w.store(2); // should reclaim the Arc swapped out by this store
        assert!(w.spare.is_some(), "writer should hold a reclaimed spare");
        let spare_ptr = Arc::as_ptr(w.spare.as_ref().unwrap());

        w.store(3);
        let _ = r.load();
        w.store(4);
        // The reclaimed buffer should be the same allocation ping-ponging.
        assert_eq!(
            Arc::as_ptr(w.spare.as_ref().unwrap()),
            spare_ptr,
            "writer should reuse the same backing allocation"
        );
        assert_eq!(r.load(), 4);
    }

    #[test]
    fn outstanding_reader_blocks_reclaim_without_unsoundness() {
        let (mut w, r) = Snapshot::new(0u32);
        w.store(1);
        // Hold a full Arc across a store — simulates a reader mid-read.
        let held = w.cell.load_full();
        assert_eq!(*held, 1);
        w.store(2);
        // The store's swapped-out Arc was still referenced by `held`, so
        // it must not have been reclaimed.
        assert!(w.spare.is_none(), "must not reclaim a buffer a reader still holds");
        drop(held);
        // New reads see the latest value regardless.
        assert_eq!(r.load(), 2);
    }
}
