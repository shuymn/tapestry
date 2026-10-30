//! Preparing a chart for play: the ingestion and chart transformation
//! extension points (design.md "Extension Boundaries").

use std::path::Path;

use bms_model::{Chart, Speed};
use bms_parser::{LoadError, LoadOptions, WarningKind};
pub use replay::TransformId;

/// A chart read by an ingestion adapter, in the common chart model.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ingested {
    /// The resolved chart, with its identity.
    pub chart: Chart,
    /// What the adapter skipped or assumed, one line each. Missing sound
    /// files are not among them: the chart's sound table has each one,
    /// without a path, for the caller to sum up.
    pub warnings: Vec<String>,
}

/// Why a chart could not be ingested.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum IngestError {
    /// The loader refused the chart.
    #[error("the chart cannot be loaded")]
    Load(#[source] LoadError),
}

/// The ingestion extension point: reads one chart format into the common
/// chart model.
pub trait ChartIngest {
    /// Whether this adapter reads the file at `path`, by its name.
    fn accepts(&self, path: &Path) -> bool;
    /// The largest file the adapter accepts. The session reads at most one
    /// byte more, so a larger file reaches the adapter cut there, and the
    /// adapter refuses it.
    fn max_bytes(&self) -> usize;
    /// Reads a chart from its file bytes; sounds resolve inside `dir`,
    /// `seed` resolves any randomness, which is recorded, and the chart's
    /// times are real times at `speed`, with the 1× chart's identity
    /// (design.md "Practice Speed Changes").
    ///
    /// # Errors
    ///
    /// The adapter's refusal.
    fn ingest(
        &self,
        bytes: &[u8],
        dir: &Path,
        seed: u64,
        speed: Speed,
    ) -> Result<Ingested, IngestError>;
}

/// BMS and BME charts, through [`bms_parser::load_bms`].
#[derive(Clone, Copy, Debug, Default)]
pub struct BmsIngest;

impl ChartIngest for BmsIngest {
    fn accepts(&self, path: &Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("bms") || e.eq_ignore_ascii_case("bme"))
    }

    fn max_bytes(&self) -> usize {
        LoadOptions::default().limits.max_file_bytes
    }

    fn ingest(
        &self,
        bytes: &[u8],
        dir: &Path,
        seed: u64,
        speed: Speed,
    ) -> Result<Ingested, IngestError> {
        let opts = LoadOptions {
            seed,
            speed,
            ..LoadOptions::default()
        };
        let loaded = bms_parser::load_bms(bytes, dir, &opts).map_err(IngestError::Load)?;
        Ok(Ingested {
            chart: loaded.chart,
            warnings: loaded
                .warnings
                .iter()
                .filter(|w| w.kind != WarningKind::MissingSoundFile)
                .map(|w| format!("{w:?}"))
                .collect(),
        })
    }
}

/// The chart transformation extension point: changes the resolved chart
/// before evaluation (later RANDOM or MIRROR options). Any randomness comes
/// from `seed`, which is recorded. A note moved in time moves its display
/// positions with it; the session validates the output
/// ([`Chart::validate`]) and refuses a chart that breaks an invariant.
pub trait ChartTransform {
    /// Which transform this is.
    fn id(&self) -> TransformId;
    /// The transformed chart.
    fn apply(&self, chart: Chart, seed: u64) -> Chart;
}

/// Returns the chart unchanged.
#[derive(Clone, Copy, Debug, Default)]
pub struct Passthrough;

impl ChartTransform for Passthrough {
    fn id(&self) -> TransformId {
        replay::PASSTHROUGH
    }

    fn apply(&self, chart: Chart, _seed: u64) -> Chart {
        chart
    }
}
