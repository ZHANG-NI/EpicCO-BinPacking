use std::cmp::Ordering;
use std::time::Instant;
use once_cell::sync::Lazy;
use crate::core::cost::Cost;

pub mod util;
pub mod io;
pub mod optimization;
pub mod core;


pub static EPOCH: Lazy<Instant> = Lazy::new(Instant::now);
pub const COST_COMPARATOR: fn(&Cost, &Cost) -> Ordering = |a: &Cost, b: &Cost| {
    match a.weighted_excluded.cmp(&b.weighted_excluded).then(a.part_area_excluded.cmp(&b.part_area_excluded)).then(a.fit_deficit.cmp(&b.fit_deficit)) {
        Ordering::Equal => a.leftover_value.partial_cmp(&b.leftover_value).unwrap().reverse(),
        other => other
    }
};
