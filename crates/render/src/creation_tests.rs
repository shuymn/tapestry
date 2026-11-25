//! A skin array's creation off the frame on WARP (design.md "Screen
//! Transitions and Loading", the frame-budgeted uploads; "Memory Budget",
//! the `Residing` kind): with the fake residency, a play staging's and a
//! result pages upload's array is created not resident and its residency
//! asked for, nothing of it used until its fence completes; the ticket is
//! answered once past its deadline, and an array whose residency is in
//! flight stays charged until its fence completes, also across a healthy
//! rebuild.

use std::sync::Arc;
use std::time::{Duration, Instant};

use bms_model::{ChartBuilder, Deadline, TimeUs, VirtualClock};
use skin_model::ScreenKind;

use super::{PagesBegun, PagesStep, PreparedToken, RenderError, Renderer};
use crate::decl::pages_texels;
use crate::residency::fake::Fake;
use crate::residency::{
    Budget, CREATE_BOUND, REMOVED, Refusal, ResidencyError, ResidencyEvent, ResidencyHolder,
    TimeoutPhase, timeout,
};
use crate::test_common::{GAUGES, attempt, context, input, offscreen, read_back, rgb_at, view};
use crate::test_decl::{Look, solid_bundle};
use crate::{EarlyRoom, SHARED_TEXELS, SkinBundle, StagingEnd};

const W: u32 = 1_920;
const H: u32 = 1_080;
const RED: [u8; 4] = [255, 0, 0, 255];
const GREEN: [u8; 4] = [0, 255, 0, 255];
const BLUE: [u8; 4] = [0, 0, 255, 255];
/// Past a ticket's deadline.
const PAST: TimeUs = TimeUs(CREATE_BOUND.0 + 1);

/// A renderer on the fake's path, its deadlines read on a virtual clock.
fn fixture() -> (Renderer, Fake, Arc<VirtualClock>) {
    let mut r = offscreen(W, H);
    let f = Fake::default();
    r.use_residency(f.residency());
    let clock = Arc::new(VirtualClock::new(TimeUs(0)));
    r.set_creation_clock(Arc::clone(&clock));
    (r, f, clock)
}

fn tiny(r: &Renderer, rgba: [u8; 4]) -> Arc<SkinBundle> {
    solid_bundle(r, &Look::left(rgba), [16, 16])
}

/// Asks the stepped staging of `b` with an attempt for `request`.
fn stage(
    r: &mut Renderer,
    b: &Arc<SkinBundle>,
    request: u64,
) -> Result<Option<PreparedToken>, RenderError> {
    let chart = ChartBuilder::new(120_000).build().unwrap();
    let ctx = context(&GAUGES, false);
    r.stage_stepped(b, &attempt(r, &chart, &ctx), request)
}

/// One boundary with time for a step, its completion waited for.
fn boundary(r: &mut Renderer) -> crate::Uploaded {
    let up = r.upload_boundary_within(Duration::from_secs(10), &mut || false, None);
    if r.upload_state() == crate::UploadState::InFlight {
        assert!(r.wait_step(Duration::from_secs(10)).unwrap());
    }
    up
}

/// Asks again and steps until the staging of `request` answers.
fn finish(r: &mut Renderer, b: &Arc<SkinBundle>, request: u64) -> PreparedToken {
    for _ in 0..10_000 {
        if let Some(t) = stage(r, b, request).unwrap() {
            return t;
        }
        boundary(r);
    }
    panic!("the staging never answered");
}

/// Boundaries until the pages upload in hand ends.
fn pages_end(r: &mut Renderer) -> PagesStep {
    for _ in 0..10_000 {
        if let Some(end) = boundary(r).pages {
            return end;
        }
    }
    panic!("the pages upload never ended");
}

/// The left half's colour at its top after a play frame.
fn colour(r: &mut Renderer) -> [u8; 3] {
    let v = view(0, 0);
    let timing = r.frame(&input(&v, TimeUs(0), TimeUs(0), &[])).unwrap();
    assert!(timing.presented());
    rgb_at(&read_back(r), W, (480, 60))
}

fn is_timeout(e: &RenderError, phase: TimeoutPhase) -> bool {
    matches!(
        e,
        RenderError::Atlas(crate::UploadError::Residency(ResidencyError::Timeout { phase: p }))
            if *p == phase
    ) && e.to_string() == timeout(phase).to_string()
}

/// S-R1: a stepped play staging creates its array on the seam: one native
/// creation of the checked shape, its residency asked with no flag, the
/// boundary's heavy operation, and no `wgpu` creation; the staging then
/// goes on, on the imported array.
#[test]
fn a_staging_requests_with_the_seam() {
    let (mut r, f, _clock) = fixture();
    let b = tiny(&r, RED);
    let heavy = r.heavy_ops().count;
    assert_eq!(stage(&mut r, &b, 1).unwrap(), None);
    assert_eq!(
        r.heavy_ops().count,
        heavy + 1,
        "the request is the heavy operation"
    );
    let creates = f.creates();
    assert_eq!(creates.len(), 1);
    assert_eq!(creates[0].side, b.side());
    assert_eq!(f.enqueued(), vec![1]);
    assert!(f.state().calls.iter().all(|c| match c {
        crate::residency::fake::Call::Enqueue { flags, .. } => flags.0 == 0,
        _ => true,
    }));
    assert_eq!(r.sync_creations(), 0);
    f.complete(1);
    let token = finish(&mut r, &b, 1);
    assert_eq!(f.imports(), 1);
    assert_eq!(r.sync_creations(), 0, "no wgpu creation");
    assert_eq!(
        r.heavy_ops().count,
        heavy + 1,
        "the import is no heavy operation"
    );
    r.commit(token).unwrap();
}

/// S-R1: the result pages upload creates its array on the seam, charged
/// from its begin.
#[test]
fn a_pages_upload_requests_with_the_seam() {
    let (mut r, f, _clock) = fixture();
    let b = tiny(&r, RED);
    let p = b.atlas_pages();
    assert_eq!(r.begin_pages(p, 1).unwrap(), PagesBegun::Begun);
    assert_eq!(r.pages_in_hand(), Some(1));
    assert_eq!(f.creates().len(), 1);
    assert_eq!(f.enqueued(), vec![1]);
    assert_eq!(r.admission().charge(ScreenKind::Result), pages_texels(p));
    assert_eq!(r.sync_creations(), 0);
    f.complete(1);
    let end = pages_end(&mut r);
    assert!(matches!(end, PagesStep::Written(1)), "{end:?}");
    assert_eq!(f.imports(), 1);
    assert_eq!(r.publish_pages(), Some(1));
}

/// S-R1, WARP: on the device's own path, the request returns before its
/// residency completes and the staging answers once it did; where the
/// device lacks the path, the `Sync` path creates the array.
#[test]
fn a_real_request_returns_before_residency() {
    let mut r = offscreen(W, H);
    r.use_device_path();
    let (path, why) = r.array_creation();
    eprintln!("array creation: {path} ({why})");
    let b = tiny(&r, RED);
    let asked = Instant::now();
    let first = stage(&mut r, &b, 1).unwrap();
    let took = asked.elapsed();
    if path == "sync" {
        assert_eq!(r.sync_creations(), 1, "{why}");
        assert!(r.take_residency_line().is_none());
    } else {
        assert_eq!(first, None);
        assert_eq!(r.sync_creations(), 0);
        let line = r.take_residency_line().expect("the request's line");
        assert_eq!(line.event, ResidencyEvent::Request);
        eprintln!("request returned in {took:?}: {line}");
    }
    let token = finish(&mut r, &b, 1);
    r.commit(token).unwrap();
    assert_eq!(colour(&mut r), [255, 0, 0]);
}

/// S-R2: while a creation's fence is held, frames are presented and each
/// boundary polls without waiting.
#[test]
fn frames_go_on_while_a_creation_is_pending() {
    let (mut r, _f, _clock) = fixture();
    let a = tiny(&r, RED);
    let chart = ChartBuilder::new(120_000).build().unwrap();
    let ctx = context(&GAUGES, false);
    let token = r.stage(&a, &attempt(&r, &chart, &ctx), 1).unwrap();
    r.commit(token).unwrap();
    let b = tiny(&r, BLUE);
    assert_eq!(stage(&mut r, &b, 2).unwrap(), None);
    for _ in 0..5 {
        let at = Instant::now();
        let _ = r.upload_boundary_within(Duration::from_millis(4), &mut || false, None);
        assert!(at.elapsed() < Duration::from_secs(1), "the poll waited");
        assert_eq!(colour(&mut r), [255, 0, 0]);
        assert!(r.creating());
        assert_eq!(stage(&mut r, &b, 2).unwrap(), None);
    }
}

/// S-R3: once its fence completes, the imported array takes its pages
/// and draws as the `Sync` path's does; the pages upload's is written.
#[test]
fn an_imported_array_takes_its_pages_and_draws() {
    let mut sync = offscreen(W, H);
    let s = tiny(&sync, GREEN);
    let token = finish(&mut sync, &s, 1);
    sync.commit(token).unwrap();
    let expected = colour(&mut sync);
    assert_eq!(sync.sync_creations(), 1);

    let (mut r, f, _clock) = fixture();
    let b = tiny(&r, GREEN);
    assert_eq!(stage(&mut r, &b, 1).unwrap(), None);
    f.complete(1);
    let token = finish(&mut r, &b, 1);
    r.commit(token).unwrap();
    assert_eq!(colour(&mut r), expected);
    assert_eq!(f.imports(), 1);

    let p = tiny(&r, BLUE);
    assert_eq!(
        r.begin_pages(p.atlas_pages(), 1).unwrap(),
        PagesBegun::Begun
    );
    f.complete(2);
    let end = pages_end(&mut r);
    assert!(matches!(end, PagesStep::Written(1)), "{end:?}");
    assert_eq!(f.imports(), 2);
}

/// S-R4: until the import, no step writes either array.
#[test]
fn no_use_before_residency() {
    let (mut r, _f, _clock) = fixture();
    let b = tiny(&r, RED);
    assert_eq!(stage(&mut r, &b, 1).unwrap(), None);
    let p = tiny(&r, BLUE);
    assert_eq!(
        r.begin_pages(p.atlas_pages(), 2).unwrap(),
        PagesBegun::Begun
    );
    let submits = r.step_submits.get();
    for _ in 0..5 {
        let up = boundary(&mut r);
        assert!(up.staged.is_none());
        assert!(up.pages.is_none());
        assert_eq!(stage(&mut r, &b, 1).unwrap(), None);
    }
    assert_eq!(
        r.step_submits.get(),
        submits,
        "a step wrote before the import"
    );
    assert!(r.staged().is_none(), "nothing staged on the array");
}

/// S-R5: a fence never signalled times the staging out once, from its
/// ticket: reported at the boundary, answered once when asked again; its
/// array kept charged as a `Residing` entry, other charges unchanged.
#[test]
fn a_never_signalled_fence_times_out() {
    let (mut r, f, clock) = fixture();
    let b = tiny(&r, RED);
    let committed = r.admission().committed;
    assert_eq!(stage(&mut r, &b, 1).unwrap(), None);
    let (_, charge) = r.admission().candidate.unwrap();
    clock.advance(TimeUs(CREATE_BOUND.0 - 1));
    assert!(boundary(&mut r).staged.is_none(), "inside its deadline");
    clock.advance(TimeUs(2));
    assert_eq!(boundary(&mut r).staged, Some(StagingEnd::Refused(1)));
    let e = stage(&mut r, &b, 1).unwrap_err();
    assert!(is_timeout(&e, TimeoutPhase::Residency), "{e:?}");
    assert!(e.is_refusal());
    let again = stage(&mut r, &b, 1).unwrap_err();
    assert!(matches!(again, RenderError::Superseded { .. }), "{again:?}");
    let a = r.admission();
    assert_eq!(a.filled, charge, "kept charged");
    assert_eq!(a.candidate, None);
    assert_eq!(a.committed, committed);
    assert_eq!(f.imports(), 0);
    let line = r.take_residency_line().unwrap();
    assert_eq!(line.event, ResidencyEvent::Request);
    let line = r.take_residency_line().unwrap();
    assert!(
        matches!(line.event, ResidencyEvent::TimedOut { .. }),
        "{line:?}"
    );
}

/// S-R5: the deadline runs from the ticket's request, as its owner set
/// it, not from the creation: a ticket that waited for its boundary has
/// that much less left.
#[test]
fn the_deadline_runs_from_the_ticket() {
    let (mut r, _f, clock) = fixture();
    let b = tiny(&r, RED);
    r.set_ticket_deadline(Deadline::after(TimeUs(0), CREATE_BOUND));
    clock.advance(TimeUs(CREATE_BOUND.0 - 100_000));
    assert_eq!(stage(&mut r, &b, 1).unwrap(), None);
    clock.advance(TimeUs(100_001));
    let e = stage(&mut r, &b, 1).unwrap_err();
    assert!(is_timeout(&e, TimeoutPhase::Residency), "{e:?}");
}

/// S-R5: a pages ticket timed out is answered once; its fence completing
/// later answers nothing more, and its entry is then destroyed at a
/// boundary.
#[test]
fn a_late_completion_gives_no_second_answer() {
    let (mut r, f, clock) = fixture();
    let p = tiny(&r, BLUE);
    assert_eq!(
        r.begin_pages(p.atlas_pages(), 7).unwrap(),
        PagesBegun::Begun
    );
    clock.advance(PAST);
    let end = boundary(&mut r).pages;
    assert!(
        matches!(&end, Some(PagesStep::Refused(7, e)) if is_timeout(e, TimeoutPhase::Residency)),
        "{end:?}"
    );
    assert_eq!(r.pages_in_hand(), None);
    f.complete(1);
    for _ in 0..3 {
        assert!(boundary(&mut r).pages.is_none(), "answered again");
    }
    assert_eq!(f.imports(), 0, "a timed-out ticket is never imported");
    assert!(r.filled() > 0);
    let _ = r.destroy_due(true);
    assert_eq!(r.filled(), 0, "destroyed once its fence completed");
}

/// S-R5a: with every entry a `Residing` one whose fence never completes,
/// each new ticket is answered once, and nothing is destroyed before its
/// fence completes; completed late, each entry goes at its own boundary
/// and its charge with it.
#[test]
fn saturation_answers_each_ticket_once_and_destroys_nothing_unfenced() {
    let (mut r, f, clock) = fixture();
    let p = tiny(&r, BLUE);
    let mut ticket = 0;
    let refused = loop {
        ticket += 1;
        assert!(ticket < 100, "never saturated");
        match r.begin_pages(p.atlas_pages(), ticket) {
            Ok(begun) => {
                assert_eq!(begun, PagesBegun::Begun);
                clock.advance(PAST);
                let end = boundary(&mut r).pages;
                assert!(
                    matches!(&end, Some(PagesStep::Refused(t, e)) if *t == ticket && is_timeout(e, TimeoutPhase::Residency)),
                    "{end:?}"
                );
                assert!(boundary(&mut r).pages.is_none(), "answered twice");
            }
            Err(e) => break e,
        }
    };
    assert!(refused.is_refusal(), "{refused:?}");
    let entries = r.retire.residing().unwrap().0;
    assert!(entries > 1, "{entries} entries");
    let charged = r.admission().filled;
    for _ in 0..entries + 2 {
        let _ = r.destroy_due(true);
        let _ = boundary(&mut r);
    }
    assert_eq!(
        r.retire.residing().unwrap().0,
        entries,
        "destroyed unfenced"
    );
    assert_eq!(r.admission().filled, charged, "a charge released unfenced");
    f.complete(u64::try_from(entries).unwrap() + 1);
    let filled = r.retire.filled();
    for left in (0..entries).rev() {
        let _ = r.destroy_due(true);
        let gone = u32::try_from(entries - left).unwrap();
        assert_eq!(r.retire.filled(), filled - gone, "one a boundary");
    }
    assert_eq!(r.admission().filled, 0);
}

/// S-R5b: a renderer dropped on a healthy device leaves its pending
/// resource with the holder, charged; the next renderer counts it,
/// releases nothing before its fence completes, then releases it at a
/// boundary with its charge.
#[test]
fn a_healthy_rebuild_keeps_pending_native_work() {
    let holder = ResidencyHolder::default();
    let (mut r, f, clock) = fixture();
    r.set_residency_holder(holder.clone());
    let p = tiny(&r, BLUE);
    assert_eq!(
        r.begin_pages(p.atlas_pages(), 1).unwrap(),
        PagesBegun::Begun
    );
    clock.advance(PAST);
    assert!(matches!(
        boundary(&mut r).pages,
        Some(PagesStep::Refused(1, _))
    ));
    let charge = r.admission().filled;
    assert!(charge > 0);
    drop(r);
    assert_eq!(holder.pending(), 1, "dropped with the renderer");
    let mut next = offscreen(W, H);
    next.set_residency_holder(holder.clone());
    assert_eq!(next.admission().held, charge);
    assert!(!next.due_heavy());
    let _ = next.destroy_due(true);
    assert_eq!(holder.pending(), 1, "released before its fence completed");
    f.complete(1);
    assert!(next.due_heavy());
    let heavy = next.heavy_ops().count;
    let _ = next.destroy_due(true);
    assert_eq!(holder.pending(), 0);
    assert_eq!(next.admission().held, 0);
    assert_eq!(
        next.heavy_ops().count,
        heavy + 1,
        "the release is a heavy operation"
    );
}

/// S-R6: with no fence value left, the request takes the `Sync` path
/// before any native call.
#[test]
fn an_exhausted_fence_counter_takes_sync() {
    let (mut r, f, _clock) = fixture();
    r.set_fence_next(REMOVED);
    let b = tiny(&r, RED);
    let token = finish(&mut r, &b, 1);
    assert!(f.state().calls.is_empty(), "a native call was made");
    assert_eq!(r.sync_creations(), 1);
    r.commit(token).unwrap();
}

/// S-R6: a fence reading `UINT64_MAX` on a device reporting its removal
/// answers the loss, latches it and never imports.
#[test]
fn a_removed_fence_is_answered_lost_and_never_imported() {
    let (mut r, f, _clock) = fixture();
    let b = tiny(&r, RED);
    assert_eq!(stage(&mut r, &b, 1).unwrap(), None);
    {
        let mut s = f.state();
        s.completed = REMOVED;
        s.removed = true;
    }
    let e = stage(&mut r, &b, 1).unwrap_err();
    assert!(matches!(e, RenderError::DeviceLost), "{e:?}");
    assert_eq!(f.imports(), 0);
    assert!(r.device_lost());
}

/// S-R7: the request's and the residency's lines carry DXGI's budget and
/// usage; over the budget the residency still completes and is imported.
#[test]
fn budget_and_usage_are_logged_at_request_and_ready() {
    let (mut r, f, _clock) = fixture();
    let budget = Budget {
        budget: 64 << 20,
        usage: 128 << 20,
    };
    f.state().budget = Some(budget);
    let b = tiny(&r, RED);
    assert_eq!(stage(&mut r, &b, 1).unwrap(), None);
    f.complete(1);
    let token = finish(&mut r, &b, 1);
    r.commit(token).unwrap();
    let request = r.take_residency_line().unwrap();
    let ready = r.take_residency_line().unwrap();
    assert_eq!(request.event, ResidencyEvent::Request);
    assert_eq!(ready.event, ResidencyEvent::Ready);
    assert_eq!(request.budget, Some(budget));
    assert_eq!(ready.budget, Some(budget));
    assert!(
        request.to_string().contains("budget 64 MiB, usage 128 MiB"),
        "{request}"
    );
    assert!(r.take_residency_line().is_none());
}

/// S-R9: a staging withdrawn before its import keeps its array charged
/// as a `Residing` entry, which no boundary destroys before its fence
/// completes.
#[test]
fn cancel_before_completion_keeps_it_charged() {
    let (mut r, f, _clock) = fixture();
    let b = tiny(&r, RED);
    assert_eq!(stage(&mut r, &b, 1).unwrap(), None);
    let (_, charge) = r.admission().candidate.unwrap();
    r.discard();
    assert!(!r.creating());
    let a = r.admission();
    assert_eq!(a.candidate, None);
    assert_eq!(a.filled, charge);
    for _ in 0..3 {
        let _ = r.destroy_due(true);
    }
    assert_eq!(r.admission().filled, charge, "destroyed before its fence");
    assert_eq!(f.imports(), 0);
}

/// S-R9: once its fence completes, the withdrawn array is destroyed at a
/// boundary, never imported.
#[test]
fn completion_after_cancel_destroys_it_at_a_boundary() {
    let (mut r, f, _clock) = fixture();
    let b = tiny(&r, RED);
    assert_eq!(stage(&mut r, &b, 1).unwrap(), None);
    r.discard();
    f.complete(1);
    assert!(r.due_heavy());
    let _ = r.destroy_due(true);
    assert_eq!(r.admission().filled, 0);
    assert_eq!(r.filled(), 0);
    assert_eq!(f.imports(), 0);
}

/// S-R9: a newer staging that fits only once the withdrawn array is gone
/// waits for it, and fits once its fence completed and it was destroyed.
#[test]
fn admission_waits_while_a_cancelled_owner_remains() {
    let (mut r, f, _clock) = fixture();
    let b = tiny(&r, RED);
    assert_eq!(stage(&mut r, &b, 1).unwrap(), None);
    let (_, charge) = r.admission().candidate.unwrap();
    r.discard();
    let next = tiny(&r, GREEN);
    r.state_candidate_charge(&next, SHARED_TEXELS - charge / 2);
    assert_eq!(r.early_room(&next), EarlyRoom::Wait);
    let _ = r.destroy_due(true);
    assert_eq!(
        r.early_room(&next),
        EarlyRoom::Wait,
        "freed before its fence"
    );
    f.complete(1);
    let _ = r.destroy_due(true);
    assert_eq!(r.early_room(&next), EarlyRoom::Fits);
}

/// S-R9: a newer request withdraws the older creation, which is kept as
/// a `Residing` entry and answers superseded.
#[test]
fn supersession_withdraws_the_older_creation() {
    let (mut r, f, _clock) = fixture();
    let a = tiny(&r, RED);
    let b = tiny(&r, BLUE);
    assert_eq!(stage(&mut r, &a, 1).unwrap(), None);
    assert_eq!(stage(&mut r, &b, 2).unwrap(), None);
    assert_eq!(f.creates().len(), 2);
    assert_eq!(r.retire.residing().map(|(n, _)| n), Some(1));
    let e = stage(&mut r, &a, 1).unwrap_err();
    assert!(matches!(e, RenderError::Superseded { .. }), "{e:?}");
    f.complete(2);
    let token = finish(&mut r, &b, 2);
    r.commit(token).unwrap();
    assert_eq!(f.imports(), 1, "only the newer one imported");
}

/// S-R9: READY's cutoff waits for a `Residing` entry until its ticket's
/// deadline, then proceeds with it kept, charged.
#[test]
fn a_cutoff_with_a_residing_entry_waits_then_proceeds() {
    let (mut r, _f, clock) = fixture();
    let b = tiny(&r, RED);
    assert_eq!(stage(&mut r, &b, 1).unwrap(), None);
    let (_, charge) = r.admission().candidate.unwrap();
    r.discard();
    assert_eq!(r.filled(), 1, "the cutoff waits for it");
    clock.advance(PAST);
    assert_eq!(r.filled(), 0, "past its deadline the cutoff proceeds");
    assert_eq!(r.admission().filled, charge, "kept charged");
}

/// S-R10: a request the runtime refuses as unsupported takes the `Sync`
/// path.
#[test]
fn an_unsupported_request_falls_back_to_sync() {
    let (mut r, f, _clock) = fixture();
    f.state().refuse_create = Some(Refusal::Unsupported);
    let b = tiny(&r, RED);
    let token = finish(&mut r, &b, 1);
    assert_eq!(r.sync_creations(), 1);
    assert_eq!(f.imports(), 0);
    r.commit(token).unwrap();
    assert_eq!(colour(&mut r), [255, 0, 0]);
    let (mut r, f, _clock) = fixture();
    f.state().refuse_enqueue = Some(Refusal::Unsupported);
    let b = tiny(&r, RED);
    let token = finish(&mut r, &b, 1);
    assert_eq!(r.sync_creations(), 1);
    r.commit(token).unwrap();
}

/// The bytes of `texture`'s `layer` at `level`, `side` texels square,
/// read back.
fn layer_bytes(
    r: &Renderer,
    texture: &wgpu::Texture,
    layer: u32,
    level: u32,
    side: u32,
) -> Vec<u8> {
    let row = side * 4;
    let padded =
        row.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let device = r.gpu.device();
    let out = device.create_buffer(&wgpu::BufferDescriptor {
        label: None,
        size: u64::from(padded) * u64::from(side),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: level,
            origin: wgpu::Origin3d {
                x: 0,
                y: 0,
                z: layer,
            },
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &out,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded),
                rows_per_image: Some(side),
            },
        },
        wgpu::Extent3d {
            width: side,
            height: side,
            depth_or_array_layers: 1,
        },
    );
    r.gpu.queue().submit([encoder.finish()]);
    let slice = out.slice(..);
    slice.map_async(wgpu::MapMode::Read, |m| m.unwrap());
    device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
    let data = slice.get_mapped_range().unwrap();
    let mut bytes = Vec::new();
    for y in 0..side as usize {
        let start = y * padded as usize;
        bytes.extend_from_slice(&data[start..start + row as usize]);
    }
    bytes
}

/// S-R11, WARP: an array created on the device's path after a texture of
/// its size was filled with a pattern and released reads zero in every
/// texel of every layer and level no step wrote (evidence: the platform
/// may zero anyway).
#[test]
fn unwritten_texels_read_zero_after_dirty_memory() {
    let mut r = offscreen(W, H);
    r.use_device_path();
    let (path, why) = r.array_creation();
    if path == "sync" {
        eprintln!("the device lacks the path ({why}): nothing to check");
        return;
    }
    let key = crate::residency::NativeCreate {
        side: 512,
        layers: 2,
        levels: 3,
    };
    {
        let dirty = r
            .gpu
            .device()
            .create_texture(&crate::skin_atlas::descriptor(key.side, key.layers, 1));
        let pattern = vec![0xAB_u8; (key.side * key.side * 4) as usize];
        for layer in 0..key.layers {
            r.gpu.queue().write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &dirty,
                    mip_level: 0,
                    origin: wgpu::Origin3d {
                        x: 0,
                        y: 0,
                        z: layer,
                    },
                    aspect: wgpu::TextureAspect::All,
                },
                &pattern,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(key.side * 4),
                    rows_per_image: Some(key.side),
                },
                wgpu::Extent3d {
                    width: key.side,
                    height: key.side,
                    depth_or_array_layers: 1,
                },
            );
        }
        r.gpu.queue().submit([]);
        assert!(
            layer_bytes(&r, &dirty, 0, 0, key.side)
                .iter()
                .all(|&b| b == 0xAB)
        );
        drop(dirty);
        r.gpu
            .device()
            .poll(wgpu::PollType::wait_indefinitely())
            .unwrap();
    }
    let Ok(super::creation::Started::Async(t)) = r.start_creation(key, 1, 0, None) else {
        panic!("the device's path took no ticket");
    };
    let asked = Instant::now();
    while r.seen(&t, r.creation_now()) == super::creation::Seen::Pending {
        assert!(asked.elapsed() < Duration::from_secs(10), "never resident");
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(r.seen(&t, r.creation_now()), super::creation::Seen::Ready);
    r.import(t);
    let (_, texture) = r.import_texture.take().unwrap();
    for layer in 0..key.layers {
        for level in 0..key.levels {
            let side = key.side >> level;
            let bytes = layer_bytes(&r, &texture, layer, level, side);
            assert!(
                bytes.iter().all(|&b| b == 0),
                "layer {layer} level {level} reads a texel no step wrote"
            );
        }
    }
}
