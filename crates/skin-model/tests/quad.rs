//! `QuadBatch`: draw order across blends, runs split on blend and clip,
//! split on an inline clip rect, drops past either capacity, and a fixed
//! allocation.

use skin_model::{Blend, ClipId, ClipRect, Quad, QuadBatch, Run, RunKey};

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
    Run {
        key,
        clip_rect: None,
        start,
        end,
    }
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
        assert_ne!(
            (pair[0].key, pair[0].clip_rect),
            (pair[1].key, pair[1].clip_rect)
        );
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

fn rect(y: f32) -> ClipRect {
    ClipRect::new([0.0, y, 10.0, 5.0]).unwrap()
}

#[test]
fn a_clip_rect_is_finite_with_positive_area() {
    assert_eq!(rect(1.0).rect(), [0.0, 1.0, 10.0, 5.0]);
    for bad in [
        [0.0, 0.0, 0.0, 1.0],
        [0.0, 0.0, 1.0, 0.0],
        [0.0, 0.0, -1.0, 1.0],
        [0.0, 0.0, 1.0, -1.0],
        [f32::NAN, 0.0, 1.0, 1.0],
        [0.0, f32::INFINITY, 1.0, 1.0],
        [0.0, 0.0, f32::INFINITY, 1.0],
    ] {
        assert_eq!(ClipRect::new(bad), None, "{bad:?}");
    }
}

/// A run breaks where either its key or its clip rect changes, and merges
/// only where both match; a plain push has no clip rect.
#[test]
fn a_clip_rect_change_splits_a_run() {
    let mut batch = QuadBatch::with_capacity(16, 16);
    let (a, b) = (rect(1.0), rect(2.0));
    assert!(batch.push_clipped(ALPHA, quad(0.0), a));
    assert!(batch.push_clipped(ALPHA, quad(1.0), a));
    assert!(batch.push_clipped(ALPHA, quad(2.0), b));
    assert!(batch.push(ALPHA, quad(3.0)));
    assert!(batch.push_clipped(ADD, quad(4.0), b));
    assert!(batch.push_clipped(clipped(1), quad(5.0), b));
    assert!(batch.push(ALPHA, quad(6.0)));
    let with = |key, clip_rect, start, end| Run {
        key,
        clip_rect,
        start,
        end,
    };
    assert_eq!(
        batch.runs(),
        [
            with(ALPHA, Some(a), 0, 2),
            with(ALPHA, Some(b), 2, 3),
            with(ALPHA, None, 3, 4),
            with(ADD, Some(b), 4, 5),
            with(clipped(1), Some(b), 5, 6),
            with(ALPHA, None, 6, 7),
        ]
    );
    assert_runs_tile(&batch);
}

/// A clipped push that needs a new run past the run cap is dropped like
/// any other.
#[test]
fn a_clipped_push_respects_both_capacities() {
    let mut batch = QuadBatch::with_capacity(2, 1);
    assert!(batch.push(ALPHA, quad(0.0)));
    assert!(!batch.push_clipped(ALPHA, quad(1.0), rect(0.0)));
    assert!(batch.push(ALPHA, quad(2.0)));
    assert!(!batch.push_clipped(ALPHA, quad(3.0), rect(0.0)));
    assert_eq!(batch.dropped(), 2);
    assert_runs_tile(&batch);
}
