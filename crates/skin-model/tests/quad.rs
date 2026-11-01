//! `QuadBatch`: draw order across blends, runs split on blend and clip,
//! drops past either capacity, and a fixed allocation.

use skin_model::{Blend, ClipId, Quad, QuadBatch, Run, RunKey};

const ALPHA: RunKey = RunKey {
    blend: Blend::Alpha,
    clip: None,
};
const ADD: RunKey = RunKey {
    blend: Blend::Add,
    clip: None,
};

fn clipped(id: u16) -> RunKey {
    RunKey {
        blend: Blend::Alpha,
        clip: Some(ClipId(id)),
    }
}

/// A quad told apart by its left edge.
fn quad(x: f32) -> Quad {
    Quad {
        rect: [x, 0.0, 1.0, 1.0],
        ..Quad::default()
    }
}

fn run(key: RunKey, start: u32, end: u32) -> Run {
    Run { key, start, end }
}

/// Runs cover the quads contiguously from 0, and neighbours differ.
fn assert_runs_tile(batch: &QuadBatch) {
    let mut next = 0;
    for r in batch.runs() {
        assert_eq!(r.start, next);
        assert!(r.end > r.start);
        next = r.end;
    }
    assert_eq!(usize::try_from(next).unwrap(), batch.quads().len());
    for pair in batch.runs().windows(2) {
        assert_ne!(pair[0].key, pair[1].key);
    }
}

#[test]
fn an_alpha_quad_after_an_additive_one_keeps_its_order() {
    let mut batch = QuadBatch::with_capacity(8, 8);
    assert!(batch.push(ALPHA, quad(0.0)));
    assert!(batch.push(ADD, quad(1.0)));
    assert!(batch.push(ALPHA, quad(2.0)));
    assert_eq!(
        batch.runs(),
        [run(ALPHA, 0, 1), run(ADD, 1, 2), run(ALPHA, 2, 3)]
    );
    let xs: Vec<f32> = batch.quads().iter().map(|q| q.rect[0]).collect();
    assert_eq!(xs, [0.0, 1.0, 2.0]);
    assert_runs_tile(&batch);
}

#[test]
fn a_clip_change_splits_a_run() {
    let mut batch = QuadBatch::with_capacity(8, 8);
    batch.push(clipped(1), quad(0.0));
    batch.push(clipped(2), quad(1.0));
    batch.push(ALPHA, quad(2.0));
    assert_eq!(
        batch.runs(),
        [
            run(clipped(1), 0, 1),
            run(clipped(2), 1, 2),
            run(ALPHA, 2, 3)
        ]
    );
    assert_runs_tile(&batch);
}

#[test]
fn equal_keys_merge_into_one_run() {
    let mut batch = QuadBatch::with_capacity(8, 8);
    for i in 0..3u8 {
        batch.push(clipped(4), quad(f32::from(i)));
    }
    batch.push(ADD, quad(3.0));
    batch.push(ADD, quad(4.0));
    assert_eq!(batch.runs(), [run(clipped(4), 0, 3), run(ADD, 3, 5)]);
    assert_runs_tile(&batch);
}

#[test]
fn a_full_quad_list_drops_and_counts() {
    let mut batch = QuadBatch::with_capacity(2, 8);
    assert!(batch.push(ALPHA, quad(0.0)));
    assert!(!batch.is_full());
    assert!(batch.push(ADD, quad(1.0)));
    assert!(batch.is_full());
    assert!(!batch.push(ADD, quad(2.0)));
    assert!(!batch.push(ALPHA, quad(3.0)));
    assert_eq!(batch.dropped(), 2);
    assert_eq!(batch.quads().len(), 2);
    assert_runs_tile(&batch);
}

#[test]
fn a_full_run_list_drops_only_quads_that_need_a_new_run() {
    let mut batch = QuadBatch::with_capacity(8, 1);
    assert!(batch.push(ALPHA, quad(0.0)));
    assert!(!batch.push(ADD, quad(1.0)));
    assert!(batch.push(ALPHA, quad(2.0)));
    assert_eq!(batch.dropped(), 1);
    assert_eq!(batch.runs(), [run(ALPHA, 0, 2)]);
    assert_runs_tile(&batch);
}

#[test]
fn clear_resets_everything_but_capacity() {
    let mut batch = QuadBatch::with_capacity(1, 1);
    let (quads, runs) = (batch.quads().as_ptr(), batch.runs().as_ptr());
    batch.push(ALPHA, quad(0.0));
    batch.push(ALPHA, quad(1.0));
    assert_eq!(batch.dropped(), 1);
    batch.clear();
    assert!(batch.quads().is_empty());
    assert!(batch.runs().is_empty());
    assert_eq!(batch.dropped(), 0);
    assert!(batch.push(ADD, quad(2.0)));
    assert_eq!(batch.runs(), [run(ADD, 0, 1)]);
    assert_eq!(batch.quads().as_ptr(), quads);
    assert_eq!(batch.runs().as_ptr(), runs);
}

#[test]
fn filling_to_capacity_does_not_reallocate() {
    let mut batch = QuadBatch::with_capacity(64, 64);
    let keys = [ALPHA, ADD, clipped(0), clipped(1)];
    // The buffers allocated by `with_capacity`, kept across every frame.
    let quads = batch.quads().as_ptr();
    let runs = batch.runs().as_ptr();
    for frame in 0..3 {
        batch.push(ALPHA, quad(0.0));
        for i in 0..100u16 {
            batch.push(keys[usize::from(i % 4)], quad(f32::from(i)));
        }
        assert_eq!(batch.quads().len(), 64, "frame {frame}");
        assert_eq!(batch.dropped(), 101 - 64);
        assert_eq!(batch.quads().as_ptr(), quads, "frame {frame}");
        assert_eq!(batch.runs().as_ptr(), runs, "frame {frame}");
        assert_runs_tile(&batch);
        batch.clear();
        assert_eq!(batch.quads().as_ptr(), quads, "frame {frame}");
        assert_eq!(batch.runs().as_ptr(), runs, "frame {frame}");
    }
}

/// A skin that fills its own quads and runs leaves the reserve whole: the
/// owner opens it and pushes after the skin, in a run of its own, into
/// the buffers allocated up front, and a clear closes it again.
#[test]
fn the_reserve_follows_a_full_skin() {
    let mut batch = QuadBatch::with_reserve(2, 2, 3, 1);
    assert_eq!(batch.capacity(), (2, 2));
    assert_eq!(batch.total_capacity(), (5, 3));
    let (quads, runs) = (batch.quads().as_ptr(), batch.runs().as_ptr());
    for frame in 0..2 {
        assert!(batch.push(ALPHA, quad(0.0)));
        assert!(batch.push(ADD, quad(1.0)));
        assert!(batch.is_full());
        assert!(!batch.push(ALPHA, quad(2.0)));
        assert_eq!(batch.dropped(), 1);
        batch.open_reserve();
        assert!(!batch.is_full());
        for x in 3..7u8 {
            batch.push(ALPHA, quad(f32::from(x)));
        }
        let xs: Vec<f32> = batch.quads().iter().map(|q| q.rect[0]).collect();
        assert_eq!(xs, [0.0, 1.0, 3.0, 4.0, 5.0], "frame {frame}");
        assert_eq!(
            batch.runs(),
            [run(ALPHA, 0, 1), run(ADD, 1, 2), run(ALPHA, 2, 5)]
        );
        assert_eq!(batch.dropped(), 2);
        assert_runs_tile(&batch);
        assert_eq!(batch.quads().as_ptr(), quads, "frame {frame}");
        assert_eq!(batch.runs().as_ptr(), runs, "frame {frame}");
        batch.clear();
        // Closed again: the skin has its own capacities only.
        assert!(batch.push(ALPHA, quad(0.0)));
        assert!(batch.push(ADD, quad(1.0)));
        assert!(!batch.push(ADD, quad(2.0)));
        batch.clear();
    }
}

/// A clear closes the run reserve on its own: with quads to spare, a blend
/// change past the skin's own runs is still refused.
#[test]
fn a_clear_closes_the_run_reserve() {
    let mut batch = QuadBatch::with_reserve(4, 1, 0, 1);
    batch.open_reserve();
    assert!(batch.push(ALPHA, quad(0.0)));
    assert!(batch.push(ADD, quad(1.0)));
    batch.clear();
    assert!(batch.push(ALPHA, quad(0.0)));
    assert!(!batch.push(ADD, quad(1.0)));
    assert!(batch.push(ALPHA, quad(2.0)));
    assert_eq!(batch.runs(), [run(ALPHA, 0, 2)]);
    assert_eq!(batch.dropped(), 1);
}
