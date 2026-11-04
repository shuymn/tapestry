//! Measurements of rasterized play text (design.md "Rendering and
//! Skins"), in release: printed, not compared, by
//! `cargo bench -p render --bench outline_text`, on the adapter
//! `TAPESTRY_BENCH_ADAPTER` names (`high`, the default, the hardware GPU;
//! `low`; `warp`), its name printed.
//!
//! 1. Installation time (median and p95 of 10) and snapshot bytes of each
//!    font: the generated Latin face, `YuGothM.ttc` when the system has
//!    it, and every file named in `TAPESTRY_TEXT_FONTS` (separated by
//!    `;`), read in place.
//! 2. A cold and a warm preparation of an attempt's text, median and p95
//!    of 30 at 1920 × 1080, against the 8 ms gate, each the whole
//!    `Renderer::stage` with its wait for the GPU: six lines at 73, 42,
//!    26, 42, 26 and 31 px (title, artist, genre, subtitle and two
//!    literals, Japanese where the font has it), each attempt cold with
//!    new labels, warm with the same ones.
//! 3. The atlas side with and without the text layer, the layer's bytes
//!    with its mips, the bytes the last cold preparation staged, the
//!    largest line, and the preparation's buffers.
//!
//! The default font is the runtime one, or the generated CJK face without
//! one.

#[path = "../tests/common/mod.rs"]
mod common;
#[path = "../tests/font_support/mod.rs"]
mod font_support;
#[path = "../tests/outline_support/mod.rs"]
mod outline_support;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bms_model::ChartBuilder;
use common::{GAUGES, context};
use outline_support::{Value, text, ttf};
use render::{AdapterChoice, Renderer, Target};
use render::{
    AttemptInput, AttemptText, DefaultFont, LineBounds, LineFit, LineLayout, OutlineText,
    SkinError, TextResidency, text::fonts_dir,
};
use skin_model::{ChartText, Layout, ScreenKind, TextRequest};

/// The producer, timing its layouts and rasterizations.
#[derive(Debug)]
struct Timed {
    inner: OutlineText,
    layout: Mutex<Duration>,
    raster: Mutex<Duration>,
}

impl AttemptText for Timed {
    fn layout(&self, req: &TextRequest, em_px: f32, bounds: LineBounds) -> LineFit {
        let t = Instant::now();
        let out = self.inner.layout(req, em_px, bounds);
        *self.layout.lock().unwrap() += t.elapsed();
        out
    }

    fn raster(&self, layout: &LineLayout, out: &mut [u8]) -> Result<(), SkinError> {
        let t = Instant::now();
        let r = self.inner.raster(layout, out);
        *self.raster.lock().unwrap() += t.elapsed();
        r
    }

    fn resident(&self) -> TextResidency {
        self.inner.resident()
    }
}

/// The adapter `TAPESTRY_BENCH_ADAPTER` names: `warp`, `low` (low power)
/// or `high` (high performance, the default: the hardware GPU where there
/// is one).
fn adapter_choice() -> AdapterChoice {
    match std::env::var("TAPESTRY_BENCH_ADAPTER").as_deref() {
        Ok("warp") => AdapterChoice::Warp,
        Ok("low") => AdapterChoice::LowPower,
        Ok("high") | Err(_) => AdapterChoice::HighPerformance,
        Ok(other) => panic!("TAPESTRY_BENCH_ADAPTER={other}: warp, low or high"),
    }
}

/// The median and the 95th percentile of `samples`.
fn stats(mut samples: Vec<Duration>) -> (Duration, Duration) {
    samples.sort_unstable();
    let at = |q: usize| samples[(samples.len() * q / 100).min(samples.len() - 1)];
    (at(50), at(95))
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1_000.0
}

fn main() {
    let default = DefaultFont::runtime().unwrap_or_else(|_| outline_support::default_font());
    println!("default font: {} bytes", default.bytes());
    let choice = adapter_choice();
    let mut fonts: Vec<(String, Vec<u8>)> = vec![(
        "generated Latin".to_owned(),
        font_support::build(&font_support::latin("Latin")),
    )];
    let yu = fonts_dir().join("YuGothM.ttc");
    if let Ok(bytes) = std::fs::read(&yu) {
        fonts.push(("YuGothM.ttc".to_owned(), bytes));
    }
    if let Ok(list) = std::env::var("TAPESTRY_TEXT_FONTS") {
        for path in list.split(';').filter(|p| !p.is_empty()) {
            match std::fs::read(path) {
                Ok(bytes) => fonts.push((path.to_owned(), bytes)),
                Err(e) => println!("{path}: {e}"),
            }
        }
    }
    let sizes = [73, 42, 26, 42, 26, 31];
    let values = [
        Value::Ref(10),
        Value::Ref(16),
        Value::Ref(13),
        Value::Ref(11),
        Value::Literal("SCORE".to_owned()),
        Value::Literal("ハイスコア".to_owned()),
    ];
    let texts: Vec<_> = sizes
        .iter()
        .zip(values)
        .enumerate()
        .map(|(i, (&size, v))| {
            let y = 40 + i32::try_from(i).unwrap() * 120;
            text("f", size, v, [100, y, 1_200, size])
        })
        .collect();
    // The skin's font record names a placeholder; each font's bytes are
    // handed to the producer as read.
    let placeholder = font_support::build(&font_support::latin("Placeholder"));
    let dir = outline_support::folder(&[("ttf.ttf", placeholder)], &[ttf()], &texts);
    let chart = ChartBuilder::new(120_000).build().unwrap();
    let ctx = context(&GAUGES, false);
    for (name, bytes) in &fonts {
        let skin = outline_support::import(dir.path()).skin;
        let install: Vec<Duration> = (0..10)
            .map(|_| {
                let t = Instant::now();
                let p = OutlineText::new(&skin, vec![(0, bytes.clone())], &default);
                let d = t.elapsed();
                p.expect("the font installs");
                d
            })
            .collect();
        let (im, ip) = stats(install);
        println!(
            "{name}: install median {:.2} ms, p95 {:.2} ms, snapshot {} bytes",
            ms(im),
            ms(ip),
            bytes.len()
        );
        let mut r = Renderer::new(
            Target::Offscreen {
                width: 1_920,
                height: 1_080,
            },
            choice,
        )
        .expect("a renderer on the chosen adapter");
        println!("{name}: adapter {}", r.gpu().adapter().name);
        let variant = skin.variant([1_920, 1_080], &Layout::AUTHOR);
        let plain = skin
            .sheet()
            .atlas(variant, r.atlas_limits(), &|| false)
            .unwrap()
            .side();
        let (bytes, d) = (bytes.clone(), Arc::clone(&default));
        let timed = Arc::new(Timed {
            inner: OutlineText::new(&skin, vec![(0, bytes)], &d).unwrap(),
            layout: Mutex::new(Duration::ZERO),
            raster: Mutex::new(Duration::ZERO),
        });
        let producer = Arc::clone(&timed);
        let b = outline_support::bundle_with(&r, dir.path(), move |_| producer);
        common::install(&mut r, &b, 1);
        let mut request = 1;
        let mut prepare = |r: &mut render::Renderer, title: &str| {
            request += 1;
            let labels = ChartText {
                title,
                subtitle: "サブタイトル",
                artist: "アーティスト",
                subartist: "",
                genre: "ジャンル",
            };
            let a = AttemptInput {
                chart: &chart,
                context: &ctx,
                text: &labels,
                target: r.target(),
            };
            let t = Instant::now();
            let token = r.stage(&b, &a, request).unwrap();
            let d = t.elapsed();
            r.commit(token).unwrap();
            d
        };
        // Cold: the installation restored before each attempt, its text
        // cache empty, so every line is laid out, placed and uploaded.
        let installation = r.installation(ScreenKind::Play).unwrap();
        let cold: Vec<Duration> = (0..30)
            .map(|i| {
                r.restore(ScreenKind::Play, &installation).unwrap();
                prepare(&mut r, &format!("曲名テスト {i}"))
            })
            .collect();
        let report = r.text_report().clone();
        let (lay, ras) = (*timed.layout.lock().unwrap(), *timed.raster.lock().unwrap());
        println!(
            "{name}: over the 30 cold preparations, layout {:.2} ms and rasterization {:.2} ms a preparation",
            ms(lay) / 30.0,
            ms(ras) / 30.0
        );
        if !report.notes.is_empty() {
            println!("{name}: notes {:?}", report.notes);
        }
        // Warm: the same labels again, every line a hit.
        let warm: Vec<Duration> = (0..30).map(|_| prepare(&mut r, "曲名テスト 29")).collect();
        let (cm, cp) = stats(cold);
        let (wm, wp) = stats(warm);
        let gate = if ms(cp) <= 8.0 { "within" } else { "past" };
        println!(
            "{name}: cold median {:.2} ms, p95 {:.2} ms ({gate} the 8 ms gate); warm median {:.2} ms, p95 {:.2} ms",
            ms(cm),
            ms(cp),
            ms(wm),
            ms(wp)
        );
        let side = b.side();
        let layer = u64::from(side) * u64::from(side) * 4 * 85 / 64;
        println!(
            "{name}: atlas side {plain} without text, {side} with; layer {layer} bytes with mips; \
             last cold preparation staged {} bytes for {} lines in {} rounds; scratch {} bytes",
            report.upload_bytes,
            report.counts.placements,
            report.counts.rounds,
            r.memory().text_scratch
        );
    }
}
