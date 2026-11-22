use serde::{Deserialize, Serialize};

/// Contains all the configurable parameters of the algorithm

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    pub avg_nodes_removed: usize,
    pub blink_rate: f32,
    pub max_run_time: Option<usize>,
    #[serde(rename = "maxRRIterations")]
    pub max_rr_iterations: Option<usize>,
    pub random_seed: Option<u64>,
    pub leftover_valuation_power: f32,
    pub history_length: usize,
    #[serde(default)]
    pub largest_bin_prob: f64,
    #[serde(default)]
    pub combo_prob: f64,
    #[serde(default = "default_combo_sheets_scale")]
    pub combo_sheets_scale: f64,
    #[serde(default = "default_combo_max_remove")]
    pub combo_max_remove: usize,
    #[serde(default = "default_combo_samples")]
    pub combo_samples: usize,
    #[serde(default)]
    pub gls_gate_ratio: f64,
    /// combo episodes: rank sampled multisets by slack per sheet instead of total area
    #[serde(default)]
    pub combo_rank_per_sheet: bool,
    #[serde(default)]
    pub gls_stagnation_iters: usize,
    #[serde(default)]
    pub stretch: bool,
    #[serde(default = "default_stretch_attempts")]
    pub stretch_attempts: usize,
    /// offer S1_v5/S1_v7: strip-band merge attempts per recreate (after a failed stretch)
    #[serde(default)]
    pub merge_attempts: usize,
    /// related-item ruin: probability per iteration that removals after the first prefer nodes sharing a dimension
    #[serde(default)]
    pub shaw_prob: f64,
    #[serde(default = "default_shaw_samples")]
    pub shaw_samples: usize,
    /// hole-growing ruin: probability per removal of removing a sibling of the largest leftover
    #[serde(default)]
    pub hole_grow_prob: f64,
    /// hole-growing only while the stuck gate (glsGateRatio) is closed
    #[serde(default)]
    pub hole_grow_gate: bool,
    /// record-to-record band on the secondary criterion: a candidate with the same excluded area as the LAHC reference
    /// is also accepted when its leftover value is at most this fraction below the reference's
    #[serde(default)]
    pub leftover_tolerance: f64,
    /// incomplete phase: rank states by the fit deficit of the excluded parts before the leftover value
    #[serde(default)]
    pub fit_deficit: bool,
    /// the fit deficit counts only when at most this many parts are excluded
    #[serde(default = "default_fit_deficit_max_items")]
    pub fit_deficit_max_items: usize,
    /// recreate part order: ties in the number of insertion options are broken by larger part area
    #[serde(default)]
    pub part_tie_area: bool,
    /// blink rate of the part selection (negative: use blinkRate)
    #[serde(default = "default_part_blink_rate")]
    pub part_blink_rate: f32,
    #[serde(default)]
    pub relax_fraction: f64,
    #[serde(default)]
    pub relax_min_iters: usize,
    #[serde(default)]
    pub relax_period_ratio: f64,
    pub rotation_allowed: bool,
    pub n_threads: usize,
    pub sheet_valuation_mode : SheetValuationMode,
    pub max_stages: Option<u8>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SheetValuationMode {
    Area,
    Cost
}
fn default_combo_samples() -> usize { 500 }
fn default_stretch_attempts() -> usize { usize::MAX }
fn default_combo_max_remove() -> usize { 3 }
fn default_combo_sheets_scale() -> f64 { f64::INFINITY }
fn default_shaw_samples() -> usize { 8 }

fn default_fit_deficit_max_items() -> usize { usize::MAX }
fn default_part_blink_rate() -> f32 { -1.0 }
