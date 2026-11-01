//! Offscreen frame pacing (design.md "Headless runs"): nothing paces an
//! offscreen target's frames, so the renderer keeps at most one submitted
//! frame whose GPU work is not known to be done, as a swap chain's frame
//! latency bounds a window's. The bookkeeping is apart from wgpu, so tests
//! drive completion and timeouts directly. It runs with every offscreen
//! frame, play frames included.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

/// The offscreen frame left pending on the GPU, of submission index `I`.
#[derive(Debug)]
pub(crate) struct Pacer<I> {
    pending: Option<I>,
}

impl<I> Default for Pacer<I> {
    fn default() -> Self {
        Self { pending: None }
    }
}

impl<I> Pacer<I> {
    /// Whether the next frame may be submitted: with a frame pending, only
    /// once `wait` (a bounded wait for its GPU work) says it is done; then
    /// nothing is pending. A frame not done within the wait stays pending
    /// and the next frame is skipped, so however slow the GPU, frames
    /// never queue up behind it.
    pub(crate) fn admit(&mut self, wait: impl FnOnce(&I) -> bool) -> bool {
        match &self.pending {
            None => true,
            Some(index) if wait(index) => {
                self.pending = None;
                true
            }
            Some(_) => false,
        }
    }

    /// A frame of `index` was submitted after [`admit`](Self::admit) let
    /// it.
    pub(crate) fn submitted(&mut self, index: I) {
        self.pending = Some(index);
    }
}

#[cfg(test)]
mod tests {
    use super::Pacer;

    /// A GPU that takes `slow` admissions' waits to finish each frame:
    /// over many frames, frames submitted never run ahead of frames done
    /// by more than one, and a frame is skipped, never queued, while one
    /// is pending.
    fn run(slow: u32) {
        let mut pacer = Pacer::default();
        let (mut submitted, mut done, mut skipped) = (0_u32, 0_u32, 0_u32);
        // Waits the pending frame has had, and the frame it is.
        let mut waits = 0;
        for _ in 0..1_000 {
            let admitted = pacer.admit(|&index: &u32| {
                waits += 1;
                // Each wait times out until the GPU is done with it.
                let finished = waits >= slow;
                if finished {
                    assert_eq!(index, submitted, "waits for the last frame");
                    done = index;
                }
                finished
            });
            if admitted {
                submitted += 1;
                waits = 0;
                pacer.submitted(submitted);
            } else {
                skipped += 1;
            }
            assert!(submitted - done <= 1, "{submitted} submitted, {done} done");
        }
        assert!(submitted > 1);
        if slow > 1 {
            assert!(skipped > 0);
        } else {
            assert_eq!(skipped, 0);
        }
    }

    #[test]
    fn a_frame_waits_for_the_one_before() {
        run(1);
    }

    #[test]
    fn a_slow_gpu_skips_frames_instead_of_queueing_them() {
        run(2);
        run(7);
    }

    /// A pending frame stays pending across timed-out waits, and the next
    /// frame goes once it is done.
    #[test]
    fn a_timed_out_frame_stays_pending() {
        let mut pacer = Pacer::default();
        assert!(pacer.admit(|_: &u8| unreachable!("nothing pending")));
        pacer.submitted(1);
        assert!(!pacer.admit(|&i| {
            assert_eq!(i, 1);
            false
        }));
        assert!(!pacer.admit(|&i| {
            assert_eq!(i, 1);
            false
        }));
        assert!(pacer.admit(|&i| i == 1));
        assert!(pacer.admit(|_| unreachable!("nothing pending")));
    }
}
