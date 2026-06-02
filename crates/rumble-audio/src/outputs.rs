//! Pipeline-stage *output* descriptors and the live-value transport.
//!
//! `settings_schema()` lets a stage describe its **inputs** (sliders,
//! toggles) so the UI can render controls generically. This module is the
//! mirror image for **outputs** — the live feedback a stage produces while
//! running (a VAD voice-probability meter, a gate on/off lamp). A stage
//! declares the *shape* of its feedback once via
//! [`ProcessorFactory::outputs`](crate::ProcessorFactory::outputs), and
//! emits the *values* per frame via
//! [`AudioProcessor::write_outputs`](crate::AudioProcessor::write_outputs).
//! The UI renders both generically — no per-processor code.
//!
//! # Why this is separate from level metering
//!
//! Signal-level metering (loudness at a tap point) deliberately does *not*
//! live here — see the note on [`crate::ProcessorResult`]. Loudness is a
//! property of the *signal*, measured at fixed taps by the audio task, so
//! it stays meaningful even if the user removes every processor. The
//! outputs here are the complement: values that are intrinsic to a
//! *stage* (RNNoise's voice probability has no signal tap that yields it —
//! only the denoiser computes it), so the stage itself must declare and
//! emit them.
//!
//! # Transport
//!
//! Values ride a fixed-capacity `Copy` [`OutputFrame`] on the same
//! single-writer/multi-reader [`crate`]-level snapshot channel the meter
//! uses, so emitting them never allocates in the audio callback and never
//! touches the `State` lock. The map from `(processor, output key)` to a
//! flat slot index is *derived deterministically* from the pipeline config
//! on both sides ([`OutputLayout::derive`]), so only the bare `[f32; N]`
//! needs to travel on the hot path — no key map rides along.

use serde::{Deserialize, Serialize};

use crate::{PipelineConfig, ProcessorRegistry};

/// Maximum number of live output values across the whole pipeline.
///
/// The [`OutputFrame`] is a fixed `Copy` array of this length so it slots
/// into the snapshot transport with zero allocation. A pipeline declaring
/// more outputs than this has its tail silently dropped by the writer —
/// callers building layouts should surface that (see [`OutputLayout`]).
pub const MAX_OUTPUTS: usize = 16;

/// Semantic role for a [`Zone`] or [`Mark`]. The UI maps each role onto a
/// theme colour rather than the descriptor carrying a raw colour, so
/// outputs stay theme-consistent and lint-clean by construction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Role {
    /// No particular meaning — a plain reference.
    Neutral,
    /// "Won't fire" / below threshold (e.g. the red sub-range of a VAD meter).
    Inactive,
    /// "Will fire" / healthy (the green sub-range).
    Active,
    /// Borderline / caution.
    Warning,
    /// Clipping / out-of-bounds.
    Danger,
}

/// A position on a [`OutputKind::Meter`] axis — either pinned to a literal
/// value or tracking a settings field. Shared by both [`Zone`] bounds and
/// [`Mark`] positions so a "trigger" threshold can drive a coloured band
/// edge and a tick identically.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Anchor {
    /// A static position in axis units.
    Fixed(f32),
    /// Track a settings field by key — e.g. a bound that follows the
    /// `vad_trigger` slider. The UI resolves this against the stage's
    /// current settings JSON, so the anchor moves as the user drags the
    /// control. Generalises the hard-coded threshold tick the old VU meter
    /// drew by hand.
    Setting(String),
}

impl Anchor {
    pub fn fixed(at: f32) -> Self {
        Anchor::Fixed(at)
    }

    pub fn setting(key: impl Into<String>) -> Self {
        Anchor::Setting(key.into())
    }

    /// Resolve to an axis value. `Fixed` yields its literal; `Setting` is
    /// looked up via `lookup` (the UI passes the stage's live settings),
    /// yielding `None` when the key is absent. Resolution lives here rather
    /// than in the UI so both zones and marks share one definition; the
    /// JSON shape stays a UI concern via the closure.
    pub fn resolve(&self, lookup: impl Fn(&str) -> Option<f32>) -> Option<f32> {
        match self {
            Anchor::Fixed(v) => Some(*v),
            Anchor::Setting(key) => lookup(key),
        }
    }
}

/// A filled sub-range `[lo, hi]` of a [`OutputKind::Meter`] axis, rendered
/// as a coloured region. Each bound is an [`Anchor`], so a band edge can
/// either sit at a fixed value or track a slider — e.g. the VAD meter's
/// "speech" band starts at the live `vad_trigger` threshold.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Zone {
    pub lo: Anchor,
    pub hi: Anchor,
    pub role: Role,
    #[serde(default)]
    pub label: Option<String>,
}

impl Zone {
    pub fn new(lo: Anchor, hi: Anchor, role: Role) -> Self {
        Self {
            lo,
            hi,
            role,
            label: None,
        }
    }

    /// A zone with both bounds pinned to literal axis values.
    pub fn fixed(lo: f32, hi: f32, role: Role) -> Self {
        Self::new(Anchor::Fixed(lo), Anchor::Fixed(hi), role)
    }

    pub fn label(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }
}

/// A single-point annotation on a [`OutputKind::Meter`] axis, rendered as
/// a thin tick. (What a first pass might call a "tick".)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Mark {
    pub at: Anchor,
    pub role: Role,
    #[serde(default)]
    pub label: Option<String>,
}

impl Mark {
    /// A mark pinned to a static axis position.
    pub fn fixed(at: f32, role: Role) -> Self {
        Self {
            at: Anchor::Fixed(at),
            role,
            label: None,
        }
    }

    /// A mark that follows the named settings field.
    pub fn setting(key: impl Into<String>, role: Role) -> Self {
        Self {
            at: Anchor::Setting(key.into()),
            role,
            label: None,
        }
    }

    pub fn label(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }
}

/// How a single output value should be visualised.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum OutputKind {
    /// A continuous value on a fixed `[min, max]` axis, decorated with
    /// coloured [`Zone`]s and point [`Mark`]s.
    Meter {
        min: f32,
        max: f32,
        /// Display unit (e.g. `"dB"`). `None` for a unitless ratio.
        #[serde(default)]
        unit: Option<String>,
        #[serde(default)]
        zones: Vec<Zone>,
        #[serde(default)]
        marks: Vec<Mark>,
    },
    /// An on/off lamp. The live value is read as on when non-zero.
    Indicator,
}

/// Static description of one live output a stage produces. Parallel to a
/// single property in `settings_schema()`, but for feedback.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutputSpec {
    /// Stable key the stage uses in [`OutputSink::set`]. Must be unique
    /// within a stage.
    pub key: String,
    /// Human-readable label for the UI.
    pub title: String,
    pub kind: OutputKind,
}

/// A frame of live output values, one per declared output across the
/// pipeline, addressed by flat slot index. `Copy` so it rides the
/// snapshot channel without allocating in the audio callback.
#[derive(Debug, Clone, Copy)]
pub struct OutputFrame {
    pub values: [f32; MAX_OUTPUTS],
    /// Number of slots actually populated this frame.
    pub len: u8,
}

impl Default for OutputFrame {
    fn default() -> Self {
        Self {
            values: [0.0; MAX_OUTPUTS],
            len: 0,
        }
    }
}

impl OutputFrame {
    /// The value at `slot`, or `None` if the slot is beyond what was
    /// populated this frame.
    pub fn get(&self, slot: usize) -> Option<f32> {
        if slot < self.len as usize {
            Some(self.values[slot])
        } else {
            None
        }
    }
}

/// Borrowed write window handed to one stage's
/// [`write_outputs`](crate::AudioProcessor::write_outputs). `set` resolves
/// a key against the stage's own specs (a short linear scan, no alloc) and
/// writes into that stage's slice of the frame.
pub struct OutputSink<'a> {
    specs: &'a [OutputSpec],
    slots: &'a mut [f32],
}

impl<'a> OutputSink<'a> {
    pub fn new(specs: &'a [OutputSpec], slots: &'a mut [f32]) -> Self {
        Self { specs, slots }
    }

    /// Publish `value` for the output named `key`. Unknown keys are
    /// ignored (the stage declared a different set).
    pub fn set(&mut self, key: &str, value: f32) {
        if let Some(i) = self.specs.iter().position(|s| s.key == key)
            && let Some(slot) = self.slots.get_mut(i)
        {
            *slot = value;
        }
    }
}

/// One placed output: where its value lives in the [`OutputFrame`] and
/// what it describes.
#[derive(Debug, Clone)]
pub struct OutputEntry {
    /// Index of the producing processor in the pipeline config.
    pub proc_index: usize,
    pub type_id: String,
    /// Flat index into [`OutputFrame::values`].
    pub slot: usize,
    pub spec: OutputSpec,
}

/// The flat slot assignment for a pipeline's outputs. Derived
/// deterministically from the config so the audio task (writer) and the UI
/// (reader) agree on slot indices without a side channel.
#[derive(Debug, Clone, Default)]
pub struct OutputLayout {
    pub entries: Vec<OutputEntry>,
    /// True if the pipeline declared more outputs than [`MAX_OUTPUTS`] and
    /// the tail was dropped. Callers should surface this rather than
    /// silently showing a partial set.
    pub truncated: bool,
}

impl OutputLayout {
    /// Build the slot assignment for `config` using `registry` for each
    /// stage's output descriptors. Must stay in lock-step with
    /// [`AudioPipeline::write_outputs`](crate::AudioPipeline::write_outputs):
    /// processors are visited in config order, stages with no outputs are
    /// skipped, and each stage's outputs occupy a contiguous slot window.
    pub fn derive(config: &PipelineConfig, registry: &ProcessorRegistry) -> Self {
        let mut entries = Vec::new();
        let mut offset = 0usize;
        let mut truncated = false;
        for (proc_index, pc) in config.processors.iter().enumerate() {
            let specs = registry.outputs(&pc.type_id);
            if specs.is_empty() {
                continue;
            }
            let n = specs.len();
            if offset + n > MAX_OUTPUTS {
                truncated = true;
                break;
            }
            for (i, spec) in specs.into_iter().enumerate() {
                entries.push(OutputEntry {
                    proc_index,
                    type_id: pc.type_id.clone(),
                    slot: offset + i,
                    spec,
                });
            }
            offset += n;
        }
        Self { entries, truncated }
    }

    /// Entries produced by the processor at `proc_index`.
    pub fn for_processor(&self, proc_index: usize) -> impl Iterator<Item = &OutputEntry> {
        self.entries.iter().filter(move |e| e.proc_index == proc_index)
    }
}
