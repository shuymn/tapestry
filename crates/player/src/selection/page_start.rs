//! A practice Start of the page's chart (design.md "Load Pipeline"): the
//! page's 1× bank joined, or decoded again, and its gain known before any
//! bank of the Start is installed or decoded. At 1× the Start installs that
//! bank; at another speed the barrier releases it before the speed's own
//! decode.

use std::sync::Arc;

use game::coord::{JobKind, LoadFailure, OwnedJob, Promotion, Waited};

use super::{ChartLoadError, Progress, cancelled, decode_on_permits};
use crate::background::Claim;
use crate::page_jobs::{PageChart, Taken};
use crate::play::PlayerRig;

/// What a step of the Start came to.
pub(super) enum Got<T> {
    /// Done, with its value.
    Ready(T),
    /// The quit key or a close: the load is cancelled.
    Cancelled,
    /// A job's failure the run ends with.
    Failed(LoadFailure),
}

impl PlayerRig<'_> {
    /// A page job of `kind` joined by a Start: promoted (the Start is
    /// responsible for it from here) and waited for while the adjustments
    /// are served, then settled: what its settle found
    /// ([`crate::page_jobs::settled`]). A quit gives the job back, demoted,
    /// for the page to keep.
    fn join_page<T: Clone>(
        &mut self,
        job: OwnedJob<T>,
        kind: JobKind,
    ) -> Result<Option<Taken<T>>, OwnedJob<T>> {
        if let Promotion::Live = job.promote_unless_ended() {
            #[cfg(test)]
            if let Some(sel) = &self.selection {
                sel.page.probe(crate::page_jobs::PageAt::Promoted(kind));
            }
            let (adjust, input, controls) = (&mut self.adjust, &self.input, self.controls);
            let waited = job.wait(crate::play::MARKER_PARK, || {
                crate::play::serve_adjustments(adjust.as_mut(), input, controls.tuning());
                cancelled(controls)
            });
            if let Waited::Stopped(_) = waited {
                return Err(job);
            }
        }
        // Published or ending: what its settle finds is its outcome.
        let looks = super::looks!(self);
        Ok(self.selection.as_ref().and_then(|sel| {
            sel.page
                .settle_one(kind, Some(job), &mut || looks.terminal())
        }))
    }

    /// The page's 1× bank for a Start: the one kept, the page's job joined
    /// (promoted, so it is the Start's responsibility from here), or, when
    /// the page has none to give, decoded now as a load's.
    pub(super) fn page_bank(
        &mut self,
        page: &PageChart,
        progress: &Arc<Progress>,
    ) -> Result<Got<Arc<audio::SoundBank>>, ChartLoadError> {
        let sel = self.selection.as_mut().ok_or(ChartLoadError::NoSelection)?;
        if let Some(e) = sel.page.take_refusal() {
            return Err(ChartLoadError::Sounds(e));
        }
        if let Some(bank) = sel.page.kept() {
            return Ok(Got::Ready(Arc::clone(bank)));
        }
        if let Some(job) = sel.page.take_bank() {
            let shown = Arc::clone(progress);
            sel.page.show(Some(Box::new(move |done, files| {
                shown.decoded(done, files)
            })));
            let joined = self.join_page(job, JobKind::Bank);
            let sel = self.selection.as_mut().ok_or(ChartLoadError::NoSelection)?;
            sel.page.show(None);
            match joined {
                // A quit: the job goes on for the page, demoted, and the
                // next Start takes it.
                Err(job) => {
                    sel.page.put_bank(job);
                    return Ok(Got::Cancelled);
                }
                Ok(Some(Taken::Done(Ok(bank)))) => {
                    progress.completed();
                    sel.page.keep_bank(&mut sel.shelf, page, Arc::clone(&bank));
                    return Ok(Got::Ready(bank));
                }
                Ok(Some(Taken::Done(Err(audio::BankError::Cancelled)) | Taken::Redo) | None) => {}
                Ok(Some(Taken::Done(Err(e)))) => return Err(ChartLoadError::Sounds(e)),
                Ok(Some(Taken::Fatal(f))) => return Ok(Got::Failed(f)),
            }
        }
        // Nothing to join: the Start decodes it, as a load does, after the
        // release barrier (a bank of another speed may be installed).
        if let Some(failure) = self
            .release_banks(None)
            .map_err(ChartLoadError::Audio)?
            .failure()
        {
            return Ok(Got::Failed(failure));
        }
        if cancelled(self.controls) {
            return Ok(Got::Cancelled);
        }
        let got = self.decode_bank((&page.path, &page.chart), audio::Speed::NORMAL, progress)?;
        if let Got::Ready(bank) = &got {
            let sel = self.selection.as_mut().ok_or(ChartLoadError::NoSelection)?;
            sel.page.keep_bank(&mut sel.shelf, page, Arc::clone(bank));
        }
        Ok(got)
    }

    /// The one way a practice Start accepts the gain of its bank at another
    /// speed: the bank's key against the 1× key the gain is of (the page's
    /// 1× bank, looked up or measured), whatever the Start knew as it
    /// began. Sounds replaced since: `false`, the 1× graph forgotten so it
    /// runs again, once (`retried`); replaced again, the Start plays at the
    /// headroom.
    pub(super) fn speed_gain(
        &mut self,
        chart: &bms_model::Chart,
        bank: &audio::SoundBank,
        retried: &mut bool,
    ) -> Result<bool, ChartLoadError> {
        let sel = self.selection.as_mut().ok_or(ChartLoadError::NoSelection)?;
        if sel.page.bank_key() == crate::loudness::key(chart, bank) {
            return Ok(true);
        }
        if std::mem::replace(retried, true) {
            println!("load: the sounds changed again; this Start plays at the headroom");
            return Ok(true);
        }
        println!("load: the sounds changed since their gain was measured");
        sel.page.forget();
        Ok(false)
    }

    /// The gain of the page's chart before the Start's bank: known, or the
    /// page's measurement joined, or the background's waited for, or
    /// measured now on `bank`, the 1× bank.
    pub(super) fn page_gain(
        &mut self,
        page: &PageChart,
        bank: &Arc<audio::SoundBank>,
        source: game::ChartSource,
    ) -> Result<Got<()>, ChartLoadError> {
        let sel = self.selection.as_mut().ok_or(ChartLoadError::NoSelection)?;
        let Some(key) = sel.page.key().filter(|_| !sel.page.gain_known()) else {
            return Ok(Got::Ready(()));
        };
        if let Some(job) = sel.page.take_measure() {
            let joined = self.join_page(job, JobKind::Measure);
            let sel = self.selection.as_mut().ok_or(ChartLoadError::NoSelection)?;
            match joined {
                Err(job) => {
                    sel.page.put_measure(job);
                    return Ok(Got::Cancelled);
                }
                // A failed measurement plays this Start at the headroom; the
                // next Start measures again.
                Ok(Some(Taken::Done(Ok(m)))) => {
                    sel.page.keep_measured(&mut sel.shelf, m);
                    return Ok(Got::Ready(()));
                }
                Ok(Some(Taken::Done(Err(crate::loudness::Cancelled)) | Taken::Redo) | None) => {}
                Ok(Some(Taken::Fatal(f))) => return Ok(Got::Failed(f)),
            }
        }
        // The background may have it: taken, waited for, or left to the
        // Start's own measurement.
        loop {
            let sel = self.selection.as_mut().ok_or(ChartLoadError::NoSelection)?;
            match sel.page.claim(&mut sel.shelf, &page.path, &key) {
                Claim::Reused(_) => return Ok(Got::Ready(())),
                Claim::Taken | Claim::Free => break,
                Claim::Running | Claim::Settling => {
                    let controls = self.controls;
                    let render = &self.render;
                    let stage = sel.loading.clone();
                    let waited = sel.shelf.wait_background(
                        &page.path,
                        &page.name,
                        || cancelled(controls),
                        || super::show_stage(render, stage.as_ref(), super::Stage::Loudness),
                    );
                    if waited == game::Loading::Cancelled {
                        return Ok(Got::Cancelled);
                    }
                    // The background's measurement, kept by the wait.
                    if sel
                        .shelf
                        .look_up_page(&page.chart, bank, &page.name)
                        .is_none()
                    {
                        sel.page.known();
                        return Ok(Got::Ready(()));
                    }
                }
            }
        }
        self.measure_scoped(&page.chart, bank, key, &page.name, source)
            .map(|got| match got {
                Got::Ready(m) => {
                    if let Some(sel) = self.selection.as_mut() {
                        sel.page.keep_measured(&mut sel.shelf, m);
                    }
                    Got::Ready(())
                }
                Got::Cancelled => Got::Cancelled,
                Got::Failed(f) => Got::Failed(f),
            })
    }
}

impl PlayerRig<'_> {
    /// The sounds of `chart` (the chart at `path`) decoded at `speed` on
    /// the Bank job's thread while this one serves the adjustments, so one
    /// made during a long decode is published before loading ends
    /// (design.md "Play Adjustments"). The quit key or a close stops the
    /// decode at the next file or packet, and no bank is made.
    pub(super) fn decode_bank(
        &mut self,
        (path, chart): (&std::path::Path, &bms_model::Chart),
        speed: audio::Speed,
        progress: &Arc<Progress>,
    ) -> Result<Got<Arc<audio::SoundBank>>, ChartLoadError> {
        let controls = self.controls;
        // A script's `quit-decoding` presses the quit key once the first
        // file is decoded (dev-tools).
        #[cfg(feature = "dev-tools")]
        let quit_decoding = self
            .selection
            .as_mut()
            .and_then(|s| s.script.as_mut())
            .is_some_and(|s| std::mem::take(&mut s.quit_decoding));
        let sel = self.selection.as_ref().ok_or(ChartLoadError::NoSelection)?;
        let (coord, spawner, rate) = (sel.coord.clone(), sel.spawner, sel.rate);
        #[cfg(test)]
        let decodes = sel.decodes.clone();
        let (adjust, input) = (&mut self.adjust, &self.input);
        let job = (JobKind::Bank, path.to_owned(), speed);
        // The decode is accounted from before its job exists to its bank's
        // registration; declared before the result, so a result dropped on
        // a cancel or a refusal goes first.
        #[cfg(feature = "dev-tools")]
        let token = sel.witness.decode();
        let decoded = std::thread::scope(|s| {
            let decode = |ctx: &game::coord::Ctx<'_>| {
                decode_on_permits(
                    ctx,
                    &coord,
                    (chart, rate, speed),
                    &|done, files| {
                        #[cfg(feature = "dev-tools")]
                        crate::dev::hold_decoding();
                        progress.decoded(done, files);
                        #[cfg(feature = "dev-tools")]
                        if quit_decoding && done == 1 {
                            controls.command(game::Command::Quit);
                        }
                    },
                    &|| ctx.cancelled() || cancelled(controls),
                    #[cfg(test)]
                    super::DecodeProbe {
                        load: decodes.as_deref().map(|d| (d, controls)),
                        at_file: decodes
                            .as_deref()
                            .and_then(|d| d.at_file.as_ref())
                            .map(|h| &*h.0 as &(dyn Fn(usize) + Sync)),
                    },
                )
            };
            match coord.run_scoped(s, spawner, job, decode) {
                Err(failure) => game::coord::Joined::Failed(failure),
                Ok(job) => job.wait(crate::play::MARKER_PARK, || {
                    #[cfg(test)]
                    super::probe(
                        decodes.as_deref(),
                        super::Probe::Wait(JobKind::Bank),
                        controls,
                        &|| false,
                    );
                    #[cfg(feature = "dev-tools")]
                    crate::dev::press_due(controls, crate::dev::LoadPhase::Decoding);
                    crate::play::serve_adjustments(adjust.as_mut(), input, controls.tuning());
                    cancelled(controls)
                }),
            }
        });
        #[cfg(test)]
        super::probe(decodes.as_deref(), super::Probe::Joined, controls, &|| {
            false
        });
        #[cfg(feature = "dev-tools")]
        crate::dev::print_adjustments("decoding", self.adjust.as_ref());
        use game::coord::Joined;
        Ok(match decoded {
            Joined::Done(Ok(bank)) => {
                progress.completed();
                let bank = Arc::new(bank);
                #[cfg(feature = "dev-tools")]
                token.register(&bank);
                Got::Ready(bank)
            }
            Joined::Done(Err(audio::BankError::Cancelled))
            | Joined::Cancelled(Some(Err(audio::BankError::Cancelled))) => {
                println!("load: cancelled while loading the sounds");
                Got::Cancelled
            }
            Joined::Cancelled(_) => Got::Cancelled,
            Joined::Done(Err(e)) => return Err(ChartLoadError::Sounds(e)),
            Joined::Failed(failure) => Got::Failed(failure),
        })
    }
}

impl PlayerRig<'_> {
    /// The loudness of `chart` (`name` in the log) under `key`, measured on
    /// its 1× `bank` on the Measure job's thread while this one serves the
    /// adjustments. The quit key leaves the measurement to the background
    /// (unless the page claimed the chart: the page measures it again); a
    /// close leaves nothing.
    pub(super) fn measure_scoped(
        &mut self,
        chart: &bms_model::Chart,
        bank: &Arc<audio::SoundBank>,
        key: library::LoudnessKey,
        name: &str,
        source: game::ChartSource,
    ) -> Result<Got<Option<audio::Measurement>>, ChartLoadError> {
        let controls = self.controls;
        let sel = self.selection.as_mut().ok_or(ChartLoadError::NoSelection)?;
        #[cfg(feature = "dev-tools")]
        let close = sel
            .script
            .as_mut()
            .is_some_and(|s| std::mem::take(&mut s.close_measuring));
        #[cfg(feature = "dev-tools")]
        let quit = sel
            .script
            .as_mut()
            .is_some_and(|s| std::mem::take(&mut s.quit_measuring));
        let (coord, spawner, rate) = (sel.coord.clone(), sel.spawner, sel.rate);
        #[cfg(test)]
        let decodes = sel.decodes.clone();
        #[cfg(test)]
        super::probe(
            decodes.as_deref(),
            super::Probe::BeforeMeasure,
            controls,
            &|| false,
        );
        // A bank whose rate the meter refuses (tests).
        #[cfg(test)]
        let bank = &if decodes
            .as_deref()
            .is_some_and(|d| d.refuse_meter.load(std::sync::atomic::Ordering::SeqCst))
        {
            Arc::new(audio::SoundBank::from_sounds(1, Vec::new()))
        } else {
            Arc::clone(bank)
        };
        let job = (
            JobKind::Measure,
            source.path().to_owned(),
            bms_model::Speed::NORMAL,
        );
        let (adjust, input) = (&mut self.adjust, &self.input);
        let measured = std::thread::scope(|s| {
            let measure = |ctx: &game::coord::Ctx<'_>| {
                // A script's `close-measuring` or `quit-measuring` acts
                // once the measurement holds its permit (dev-tools).
                #[cfg(feature = "dev-tools")]
                if close {
                    controls.close();
                }
                #[cfg(feature = "dev-tools")]
                if quit {
                    controls.command(game::Command::Quit);
                }
                let mut host = super::JobHost {
                    controls,
                    #[cfg(test)]
                    coord: &coord,
                    ctx,
                    #[cfg(test)]
                    decodes: decodes.as_deref(),
                };
                crate::loudness::measure(chart, bank, Some(&key), name, &mut host)
            };
            match coord.run_scoped(s, spawner, job, measure) {
                Err(failure) => game::coord::Joined::Failed(failure),
                Ok(job) => job.wait(crate::play::MARKER_PARK, || {
                    #[cfg(test)]
                    super::probe(
                        decodes.as_deref(),
                        super::Probe::Wait(JobKind::Measure),
                        controls,
                        &|| false,
                    );
                    crate::play::serve_adjustments(adjust.as_mut(), input, controls.tuning());
                    cancelled(controls)
                }),
            }
        });
        use game::coord::Joined;
        Ok(match measured {
            Joined::Done(Ok(m)) => Got::Ready(m),
            Joined::Done(Err(crate::loudness::Cancelled)) | Joined::Cancelled(_) => {
                // The quit key leaves the chart's measurement to the
                // background; a close ends the run.
                if !self.controls.quitting()
                    && let Some(sel) = &self.selection
                {
                    sel.shelf.leave_to_background(crate::background::Job {
                        source,
                        rate,
                        name: name.to_owned(),
                    });
                }
                Got::Cancelled
            }
            Joined::Failed(failure) => Got::Failed(failure),
        })
    }
}
