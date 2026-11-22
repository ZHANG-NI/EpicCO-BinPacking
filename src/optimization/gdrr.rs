use std::cmp::Ordering;
use std::collections::VecDeque;

use colored::*;
use ordered_float::NotNan;
use rand::prelude::SliceRandom;
use rand::RngExt;
use rand::rngs::SmallRng;

use crate::core::cost::Cost;
use crate::core::entities::parttype::PartType;
use crate::core::insertion::insertion_blueprint::InsertionBlueprint;
use crate::core::layout_index::LayoutIndex;
use crate::optimization::config::Config;
use crate::optimization::instance::Instance;
use crate::optimization::problem::Problem;
use crate::optimization::rr::insertion_option_cache::InsertionOptionCache;
use crate::optimization::sol_collectors::local_sol_collector::LocalSolCollector;
use crate::optimization::solutions::problem_solution::ProblemSolution;
use crate::optimization::solutions::solution::Solution;
use crate::util::{assertions, blink};
use crate::timed_thread_println;
use crate::util::util;

/// Goal-Driven Ruin and Recreate algorithm
pub struct GDRR<'a> {
    config: &'a Config,
    problem: Problem<'a>,
    local_sol_collector: LocalSolCollector<'a>,
    n_stretch: usize,
    n_merge: usize,
    /// direct-mapped table of known repair-site failures: (layout version, part w, part h, kind)
    repair_fail: Vec<(u64, u64, u64, u8)>,
    merge_buf: Vec<crate::core::entities::layout::MergeCand>,
}


impl<'a> GDRR<'a> {
    pub fn new(instance: &'a Instance, config: &'a Config, local_sol_collector: LocalSolCollector<'a>) -> Self {
        let mut problem = Problem::new(
            instance,
            config.random_seed,
            config.leftover_valuation_power,
        );
        problem.set_fit_deficit(config.fit_deficit, config.fit_deficit_max_items);
        Self {
            config,
            problem,
            local_sol_collector,
            n_stretch: 0,
            n_merge: 0,
            repair_fail: vec![(0, 0, 0, 0); 1 << 14],
            merge_buf: Vec::new(),
        }
    }

    pub fn lahc(&mut self) {
        let stats = self.optimize_with_stats();
        let elapsed_ms = stats.elapsed.as_millis() as f64;

        timed_thread_println!("{}:\t ({:.2} iter/s, {:.2} acc/s, {} impr)",
                "GDRR finished".bright_magenta(),
                 stats.n_iterations as f64 / elapsed_ms * 1000.0,
                 stats.n_accepted as f64 / elapsed_ms * 1000.0,
                stats.n_improved
        );
        timed_thread_println!("stretches: {}", self.n_stretch);
        timed_thread_println!("merges: {}", self.n_merge);
        timed_thread_println!("{}:\t {}", "Final incomp".bright_yellow(),
            match self.local_sol_collector.best_incomplete_solution() {
                Some(sol) => {
                    util::solution_stats_string(sol)
                }
                None => "()".to_string()
            });
    }

    pub fn optimize(&mut self) {
        self.optimize_with_stats();
    }

    // Late Acceptance Hill Climbing metaheuristic
    fn optimize_with_stats(&mut self) -> OptimizationStats {
        let start_time = std::time::Instant::now();

        let max_rr_iterations = self.config.max_rr_iterations.unwrap_or(usize::MAX);

        let mut empty_problem_cost = Cost::new(0, 0.0, self.problem.instance().total_part_area(), 0);
        empty_problem_cost.weighted_excluded = u64::MAX;
        // offer S2_v1: guided local search weights on excluded part types
        let mut iters_since_improvement: usize = 0;
        let mut max_natural_gap: usize = 0;
        let mut bumped_since_improvement = false;
        // offer S2_v4: second GLS trigger, active only once the run is stuck at its best complete cost (no true drop
        // for at least glsGateRatio x the iterations it took to reach it): the raw excluded area (best of the current
        // phase) has not decreased for max(glsStagnationIters, 2 x the longest natural gap between its decreases).
        let mut p_iters: usize = 0;
        let mut p_max_gap: usize = 0;
        let mut p_bumped = false;

        let mut lahc_history: VecDeque<Cost> = VecDeque::with_capacity(self.config.history_length);
        lahc_history.push_back(empty_problem_cost.clone());
        let mut n_iterations = 0;
        let mut n_accepted = 0;
        let mut n_improved = 0;
        let mut mat_limit = self.local_sol_collector.material_limit();
        let mut local_optimum: Option<ProblemSolution> = None;
        let mut insertion_option_cache = InsertionOptionCache::new(self.problem.instance());
        // Strategic oscillation of the material limit
        let mut true_limit = mat_limit;
        let mut last_true_drop_iter: usize = 0;
        let mut last_event_iter: usize = 0;
        let mut best_excluded: u64 = u64::MAX;
        // bin-multiset episodes
        let mut combo_active = false;
        let mut tried_combos: Vec<Vec<usize>> = Vec::new();

        while n_iterations < max_rr_iterations && !self.local_sol_collector.terminate() {
            let mut reset_event = false;
            let mat_limit_budget: i128 = match local_optimum.as_ref() {
                Some(solution) => mat_limit as i128 - 1 - solution.cost().material_cost as i128,
                None => mat_limit as i128 - 1 - self.problem.cost().material_cost as i128,
            };

            let hole_on = !(self.config.hole_grow_gate && self.config.gls_gate_ratio > 0.0
                && (n_iterations - last_true_drop_iter) as f64 >= self.config.gls_gate_ratio * last_true_drop_iter as f64);
            let mat_limit_budget = self.ruin(mat_limit_budget, hole_on);
            let max_part_area_not_included = match local_optimum.as_ref() {
                Some(local_optimum) => u64::max(lahc_history.front().unwrap().part_area_excluded, local_optimum.cost().part_area_excluded),
                None => lahc_history.front().unwrap().part_area_excluded
            };

            self.recreate(
                mat_limit_budget,
                max_part_area_not_included,
                &mut insertion_option_cache,
            );

            let cost = self.problem.cost();

            let front = lahc_history.front().unwrap();
            let in_band = self.config.leftover_tolerance > 0.0
                && cost.weighted_excluded == front.weighted_excluded
                && cost.part_area_excluded == front.part_area_excluded
                && cost.fit_deficit == front.fit_deficit
                && cost.leftover_value as f64 >= front.leftover_value as f64 * (1.0 - self.config.leftover_tolerance);
            if in_band || crate::COST_COMPARATOR(&cost, lahc_history.front().unwrap()) <= Ordering::Equal ||
                (local_optimum.is_some() && crate::COST_COMPARATOR(&cost, local_optimum.as_ref().unwrap().cost()) <= Ordering::Equal) {
                //Solution is better or equivalent to the last entry in the history queue or the local optimum.

                local_optimum = Some(self.problem.create_solution(local_optimum.take(), Some(cost.clone())));

                lahc_history.pop_front();

                if crate::COST_COMPARATOR(&cost, lahc_history.back().unwrap_or(&empty_problem_cost)) == Ordering::Less {
                    //Current local optimum is better than the last value of the history queue
                    for _ in 0..(self.config.history_length - lahc_history.len()) {
                        lahc_history.push_back(cost.clone());
                    }
                    if cost.part_area_excluded < best_excluded {
                        // progress on the incomplete side: not stuck
                        best_excluded = cost.part_area_excluded;
                        last_event_iter = n_iterations;
                        if !p_bumped { p_max_gap = p_max_gap.max(p_iters); }
                        p_bumped = false;
                        p_iters = 0;
                    }
                    if cost.material_cost < true_limit {
                        self.local_sol_collector.report_problem_solution(local_optimum.as_ref().unwrap());
                    } else if cost.part_area_excluded == 0 && cost.material_cost < mat_limit {
                        // complete under the relaxed working limit only: continue the descent from it
                        mat_limit = cost.material_cost;
                        local_optimum = None;
                        lahc_history.clear();
                        lahc_history.push_back(empty_problem_cost.clone());
                        last_event_iter = n_iterations;
                        best_excluded = u64::MAX;
                        reset_event = true;
                    }
                    n_improved += 1;
                    if !bumped_since_improvement {
                        max_natural_gap = max_natural_gap.max(iters_since_improvement);
                    }
                    bumped_since_improvement = false;
                    iters_since_improvement = 0;
                } else {
                    //Current local optimum is not better, add the best cost to the history queue
                    for _ in 0..(self.config.history_length - lahc_history.len()) {
                        let best = lahc_history.back().unwrap_or(&empty_problem_cost).clone();
                        lahc_history.push_back(best);
                    }
                }
                n_accepted += 1;
            } else {
                self.problem.restore_from_problem_solution(local_optimum.as_ref().unwrap());
            }

            if self.local_sol_collector.material_limit() < true_limit {
                true_limit = self.local_sol_collector.material_limit();
                mat_limit = true_limit;
                local_optimum = None;
                lahc_history.clear();
                lahc_history.push_back(empty_problem_cost.clone());
                last_true_drop_iter = n_iterations;
                last_event_iter = n_iterations;
                tried_combos.clear();
                if combo_active {
                    self.problem.set_stock_caps(None);
                    combo_active = false;
                }
                best_excluded = u64::MAX;
                reset_event = true;
            } else if self.config.relax_fraction > 0.0 && true_limit < u64::MAX && local_optimum.is_some() {
                let period = usize::max(self.config.relax_min_iters,
                                        (self.config.relax_period_ratio * last_true_drop_iter as f64) as usize);
                let mut combo_started = false;
                // multiset jumps matter when sheets are coarse w.r.t. the solution (few sheets in use)
                let n_sheets = usize::max(1, self.problem.layouts().len());
                let p_combo = f64::min(self.config.combo_prob, self.config.combo_sheets_scale / n_sheets as f64);
                if n_iterations - last_event_iter > period && p_combo > 0.0
                    && self.problem.rng().random::<f64>() < p_combo {
                    // stuck: try an explicit multiset of bin types (problem equals the local optimum here)
                    let current = self.problem.used_sheet_counts();
                    if !tried_combos.contains(&current) { tried_combos.push(current.clone()); }
                    if let Some(combo) = self.sample_combo(true_limit, &tried_combos, &current) {
                        tried_combos.push(combo.clone());
                        if combo_active { self.problem.set_stock_caps(None); }
                        let mut counts = current;
                        for t in 0..combo.len() {
                            while counts[t] > combo[t] {
                                let worst = self.problem.layouts().iter()
                                    .filter(|(_, l)| l.sheettype().id == t)
                                    .min_by(|(_, a), (_, b)| a.usage().partial_cmp(&b.usage()).unwrap())
                                    .map(|(i, _)| i).unwrap();
                                let top = *self.problem.layouts()[worst].top_node_index();
                                self.problem.remove_node(top, LayoutIndex::Existing(worst));
                                counts[t] -= 1;
                            }
                        }
                        self.problem.set_stock_caps(Some(&combo));
                        combo_active = true;
                        mat_limit = true_limit;
                        local_optimum = None;
                        lahc_history.clear();
                        lahc_history.push_back(empty_problem_cost.clone());
                        last_event_iter = n_iterations;
                        best_excluded = u64::MAX;
                        reset_event = true;
                        combo_started = true;
                        timed_thread_println!("combo episode: {:?} (best {})", combo, true_limit);
                    }
                }
                if !combo_started && n_iterations - last_event_iter > period {
                    // stuck: relax the working material limit above the best complete cost
                    if combo_active {
                        self.problem.set_stock_caps(None);
                        combo_active = false;
                    }
                    mat_limit = true_limit + (true_limit as f64 * self.config.relax_fraction) as u64;
                    local_optimum = None;
                    lahc_history.clear();
                    lahc_history.push_back(empty_problem_cost.clone());
                    last_event_iter = n_iterations;
                    best_excluded = u64::MAX;
                    reset_event = true;
                    timed_thread_println!("relax: working limit {} (best {})", mat_limit, true_limit);
                }
            }
            if reset_event {
                self.problem.reset_part_weights();
                iters_since_improvement = 0;
                bumped_since_improvement = false;
                p_iters = 0;
                p_max_gap = 0;
                p_bumped = false;
            } else if self.config.gls_stagnation_iters > 0 {
                iters_since_improvement += 1;
                p_iters += 1;
                let gate_open = self.config.gls_gate_ratio > 0.0
                    && (n_iterations - last_true_drop_iter) as f64 >= self.config.gls_gate_ratio * last_true_drop_iter as f64;
                let primary_trigger = gate_open
                    && p_iters >= usize::max(self.config.gls_stagnation_iters, 2 * p_max_gap);
                if primary_trigger || iters_since_improvement >= usize::max(self.config.gls_stagnation_iters, 2 * max_natural_gap) {
                    iters_since_improvement = 0;
                    bumped_since_improvement = true;
                    p_iters = 0;
                    p_bumped = true;
                    if let Some(lo) = local_optimum.as_mut() {
                        if lo.cost().part_area_excluded > 0 {
                            //problem currently equals the local optimum: penalize its excluded part types
                            self.problem.bump_excluded_weights();
                            let new_cost = self.problem.cost();
                            lo.set_cost(new_cost.clone());
                            lahc_history.clear();
                            for _ in 0..self.config.history_length {
                                lahc_history.push_back(new_cost.clone());
                            }
                        }
                    }
                }
            }
            n_iterations += 1;
            if n_iterations % 100 == 0 {
                self.local_sol_collector.rx_sync()
            }

            debug_assert!(lahc_history.len() <= self.config.history_length, "{}", lahc_history.len());
        }
        OptimizationStats {
            elapsed: start_time.elapsed(),
            n_iterations,
            n_accepted,
            n_improved,
        }
    }

    /// Sample maximal multisets of sheet types (within stock) with total value below `limit` that can hold
    /// every part type by dimensions; return the one of largest total value not yet tried.
    fn sample_combo(&mut self, limit: u64, tried: &[Vec<usize>], current: &[usize]) -> Option<Vec<usize>> {
        let instance = self.problem.instance();
        let sheets = instance.sheets();
        let n_types = sheets.len();
        let part_area = instance.total_part_area();
        let mut best: Option<(f64, Vec<usize>)> = None;
        let mut cand: Vec<usize> = Vec::with_capacity(n_types);
        let mut masks: Vec<(u64, u64)> = Vec::new();
        for _ in 0..self.config.combo_samples {
            // neighbourhood of the current multiset: drop 1..=comboMaxRemove random sheets, then fill greedily
            let mut counts = current.to_vec();
            let n_cur: usize = counts.iter().sum();
            let n_remove = usize::min(n_cur, self.problem.rng().random_range(1..=self.config.combo_max_remove.max(1)));
            for _ in 0..n_remove {
                let total: usize = counts.iter().sum();
                let mut r = self.problem.rng().random_range(0..total);
                for t in 0..n_types {
                    if r < counts[t] { counts[t] -= 1; break; }
                    r -= counts[t];
                }
            }
            let mut area: u64 = (0..n_types).map(|t| counts[t] as u64 * sheets[t].0.value).sum();
            loop {
                cand.clear();
                for t in 0..n_types {
                    let (st, stock) = &sheets[t];
                    if counts[t] < *stock && area + st.value < limit {
                        cand.push(t);
                    }
                }
                if cand.is_empty() { break; }
                let t = cand[self.problem.rng().random_range(0..cand.len())];
                counts[t] += 1;
                area += sheets[t].0.value;
            }
            if area < part_area || area >= limit { continue; }
            // rank: waste allowance per sheet (slack / n_sheets) or total area
            let score = if self.config.combo_rank_per_sheet {
                (area - part_area) as f64 / usize::max(1, counts.iter().sum::<usize>()) as f64
            } else { area as f64 };
            if best.as_ref().is_some_and(|(a, _)| *a >= score) { continue; }
            if tried.contains(&counts) { continue; }
            // Necessary condition: for every part type, the parts that fit only in the sheet types of its
            // fit-set G must have total area <= the area of the multiset's sheets of the types in G.
            let fit_mask = |pt: &PartType| -> u64 {
                let mut m = 0u64;
                for t in 0..n_types.min(64) {
                    if counts[t] > 0 && pt.width() <= sheets[t].0.width && pt.height() <= sheets[t].0.height { m |= 1 << t; }
                }
                m
            };
            // aggregate part areas by distinct fit mask: same check, O(distinct masks^2) instead of O(part types^2)
            masks.clear();
            masks.extend(instance.parts().iter().map(|(pt, q)| (fit_mask(pt), pt.area() * *q as u64)));
            masks.sort_unstable_by_key(|(m, _)| *m);
            masks.dedup_by(|b, a| if a.0 == b.0 { a.1 += b.1; true } else { false });
            let masks = &masks;
            if n_types <= 64 && masks.iter().any(|(m, _)| *m == 0) { continue; }
            let feasible = masks.iter().all(|(g, _)| {
                let need: u64 = masks.iter().filter(|(m, _)| m & !g == 0).map(|(_, a)| *a).sum();
                let cap: u64 = (0..n_types.min(64)).filter(|t| g & (1 << t) != 0).map(|t| counts[t] as u64 * sheets[t].0.value).sum();
                need <= cap
            });
            if n_types <= 64 && !feasible { continue; }
            best = Some((score, counts));
        }
        best.map(|(_, c)| c)
    }

    fn ruin(&mut self, mut mat_limit_budget: i128, hole_on: bool) -> i128 {
        let n_nodes_to_remove = self.problem.rng().random_range(2..(self.config.avg_nodes_removed - 2) * 2 + 1) + 2;

        if mat_limit_budget >= 0 {
            // related-item ruin: removals after the first prefer nodes sharing a dimension with the first removed node
            let shaw = self.config.shaw_prob > 0.0 && self.problem.rng().random::<f64>() < self.config.shaw_prob;
            let mut seed_dims: Option<(u64, u64)> = None;
            for _i in 0..n_nodes_to_remove {
                //The bias sampler allows us to select a random layout for removing a node, but with a bias towards layouts with a low usage.
                //This is done to preserve 'good' layouts and give 'bad' layouts more opportunity to improve
                let n_layouts = self.problem.layouts().len();
                if n_layouts == 0 {
                    break;
                }
                let mut sampled_layouts: [_; 3] = std::array::from_fn(|_| {
                    let sample_index = self.problem.rng().random_range(0..n_layouts);
                    let layout_index = self.problem.layout_keys()[sample_index];
                    let usage = self.problem.layouts()[layout_index].usage();
                    (layout_index, NotNan::new(usage).expect("layout usage is NaN"))
                });
                sampled_layouts.sort_by(|a, b| a.1.cmp(&b.1));
                let sample = self.problem.rng().random::<f64>();
                let layout_index = sampled_layouts[match sample {
                    sample if sample <= 0.625 => 0,
                    sample if sample <= 0.875 => 1,
                    _ => 2,
                }].0;

                let mut selected_node = self.problem.choose_removable_node(layout_index);
                if !shaw && hole_on && self.config.hole_grow_prob > 0.0 && self.problem.rng().random::<f64>() < self.config.hole_grow_prob {
                    // offer S1_v12 hole-growing: remove a sibling of the bin's largest leftover so the freed space merges with it
                    let layout = &self.problem.layouts()[layout_index];
                    let arena = layout.nodes();
                    let mut cand: Option<crate::core::entities::node::NodeKey> = None;
                    if let Some(&e) = layout.sorted_empty_nodes().first() {
                        if let Some(p) = arena[e].parent() {
                            let sibs: Vec<_> = arena[p].children(arena)
                                .filter(|c| *c != e && (arena[*c].parttype().is_some() || arena[*c].has_children()))
                                .collect();
                            if !sibs.is_empty() { cand = Some(sibs[self.problem.rng().random_range(0..sibs.len())]); }
                        }
                    }
                    if let Some(c) = cand { selected_node = c; }
                }
                if shaw {
                    let layout = &self.problem.layouts()[layout_index];
                    let (lw, lh) = (layout.sheettype().width, layout.sheettype().height);
                    match seed_dims {
                        None => {
                            let n = &layout.nodes()[selected_node];
                            seed_dims = Some((n.width(), n.height()));
                        }
                        Some((sw, sh)) => {
                            let related = |n: &crate::core::entities::node::Node| -> bool {
                                (n.width() == sw && sw != lw) || (n.height() == sh && sh != lh)
                            };
                            if !related(&layout.nodes()[selected_node]) {
                                for _ in 1..self.config.shaw_samples {
                                    let c = self.problem.choose_removable_node(layout_index);
                                    if related(&self.problem.layouts()[layout_index].nodes()[c]) {
                                        selected_node = c;
                                        break;
                                    }
                                }
                            }
                        }
                    }
                }

                let removed_sheet_value = self.problem.remove_node(selected_node, LayoutIndex::Existing(layout_index));
                if let Some(removed_sheet_value) = removed_sheet_value {
                    mat_limit_budget += removed_sheet_value as i128;
                }
            }
        } else {
            while mat_limit_budget < 0 {
                //Search the lowest usage layout
                let min_usage_layout_index = self.problem.layouts().iter()
                    .map(|(i, l)| (i, l.usage()))
                    .min_by(|(_, a), (_, b)| {
                        a.partial_cmp(b).unwrap()
                    })
                    .map(|(i, _)| i);

                let Some(min_usage_layout_index) = min_usage_layout_index else {
                    break; // no existing layouts
                };
                let top_node = *self.problem.layouts()[min_usage_layout_index].top_node_index();

                // Release it and update mat_limit_exceedance.
                let removed_sheet_value = self.problem
                    .remove_node(top_node, LayoutIndex::Existing(min_usage_layout_index))
                    .expect("top node should remove entire layout");
                mat_limit_budget += removed_sheet_value as i128;
            }
        }
        mat_limit_budget
    }

    fn recreate(
        &mut self,
        mut mat_limit_budget: i128,
        max_part_area_excluded: u64,
        insertion_option_cache: &mut InsertionOptionCache<'a>,
    ) {
        let mut parttypes_to_consider: Vec<&PartType> = self.problem.parttype_qtys().iter().enumerate()
            .filter(|(_i, q)| { **q > 0 })
            .map(|(i, _q)| -> &PartType { self.problem.instance().parttype(i) }).collect();


        insertion_option_cache.clear();
        let mut part_area_not_included: u64 = 0;

        //Iterate all layouts which should be considered during this recreate iteration
        let layouts_to_consider = self.problem.layouts().iter().map(|(i, l)| (LayoutIndex::Existing(i), l))
            .chain(self.problem.empty_layouts().iter().enumerate()
                .filter(|(_, l)| self.problem.sheettype_qtys()[l.sheettype().id] > 0)
                .map(|(i, l)| (LayoutIndex::empty(i), l))
            );

        //Generate insertion options for all relevant parttypes and layouts
        insertion_option_cache.add_for_parttypes(&parttypes_to_consider, layouts_to_consider);
        debug_assert!(assertions::insertion_option_cache_is_valid(&self.problem, insertion_option_cache, &parttypes_to_consider));

        let mut existing_layout_blueprints = Vec::new();
        let mut new_layout_blueprints = Vec::new();
        let mut parttype_indices = Vec::new();
        let mut stretch_attempts = 0usize;
        let mut merge_attempts = 0usize;
        while !parttypes_to_consider.is_empty() && part_area_not_included <= max_part_area_excluded {
            existing_layout_blueprints.clear();
            new_layout_blueprints.clear();

            let elected_parttype = GDRR::select_next_parttype(
                &parttypes_to_consider,
                insertion_option_cache,
                &mut parttype_indices,
                self.problem.rng(),
                self.config,
            );
            let mut elected_blueprint = GDRR::select_insertion_blueprint(
                elected_parttype,
                insertion_option_cache,
                mat_limit_budget,
                &mut self.problem,
                self.config,
                &mut existing_layout_blueprints,
                &mut new_layout_blueprints,
            );
            if elected_blueprint.is_none() && self.config.stretch && stretch_attempts < self.config.stretch_attempts {
                // offer S1_v3: the part would be excluded: stretch a strip of some layout to make room for it
                stretch_attempts += 1;
                let (w, h) = (elected_parttype.width(), elected_parttype.height());
                let mut best: Option<(u64, crate::core::layout_index::LayoutKey, crate::core::entities::node::NodeKey)> = None;
                // layouts in random order, first one with a site (best site within it)
                let n_l = self.problem.layout_keys().len();
                let offset = if n_l > 0 { self.problem.rng().random_range(0..n_l) } else { 0 };
                for j in 0..n_l {
                    let key = self.problem.layout_keys()[(offset + j) % n_l];
                    let layout = &self.problem.layouts()[key];
                    if (layout.sheettype().area() as f64 * (1.0 - layout.usage())) < (w * h) as f64 { continue; }
                    let slot = repair_slot(layout.version(), w, h, 1);
                    if self.repair_fail[slot] == (layout.version(), w, h, 1) { continue; }
                    if let Some((e, d)) = layout.find_stretch_site(w, h) {
                        best = Some((d, key, e));
                        break;
                    }
                    self.repair_fail[slot] = (layout.version(), w, h, 1);
                }
                if let Some((d, key, e)) = best {
                    self.n_stretch += 1;
                    let li = LayoutIndex::Existing(key);
                    let mut before: Vec<_> = self.problem.layouts()[key].sorted_empty_nodes().to_vec();
                    self.problem.apply_stretch(key, e, d);
                    let layout = &self.problem.layouts()[key];
                    let mut after: Vec<_> = layout.sorted_empty_nodes().to_vec();
                    before.sort_unstable();
                    after.sort_unstable();
                    // update the cache only for the empty nodes that changed
                    for n in before.iter().filter(|n| after.binary_search(n).is_err()) {
                        insertion_option_cache.remove_for_node(&li, n);
                    }
                    for n in after.iter().filter(|n| before.binary_search(n).is_err()) {
                        insertion_option_cache.add_for_node(n, &layout.nodes()[*n], &li, parttypes_to_consider.iter());
                    }
                    existing_layout_blueprints.clear();
                    new_layout_blueprints.clear();
                    elected_blueprint = GDRR::select_insertion_blueprint(
                        elected_parttype,
                        insertion_option_cache,
                        mat_limit_budget,
                        &mut self.problem,
                        self.config,
                        &mut existing_layout_blueprints,
                        &mut new_layout_blueprints,
                    );
                    debug_assert!(elected_blueprint.is_some());
                }
            }

            if elected_blueprint.is_none() && merge_attempts < self.config.merge_attempts {
                merge_attempts += 1;
                // the part would be excluded: join the free bands of two sibling strips of some layout
                let (w, h) = (elected_parttype.width(), elected_parttype.height());
                let n_l = self.problem.layout_keys().len();
                let offset = if n_l > 0 { self.problem.rng().random_range(0..n_l) } else { 0 };
                let mut site = None;
                for j in 0..n_l {
                    let key = self.problem.layout_keys()[(offset + j) % n_l];
                    let layout = &self.problem.layouts()[key];
                    if (layout.sheettype().area() as f64 * (1.0 - layout.usage())) < (w * h) as f64 { continue; }
                    let slot = repair_slot(layout.version(), w, h, 2);
                    if self.repair_fail[slot] == (layout.version(), w, h, 2) { continue; }
                    if let Some((p, s1, s2)) = layout.find_merge_site(w, h, &mut self.merge_buf) {
                        site = Some((key, p, s1, s2));
                        break;
                    }
                    self.repair_fail[slot] = (layout.version(), w, h, 2);
                }
                if let Some((key, p, s1, s2)) = site {
                    let li = LayoutIndex::Existing(key);
                    let mut before: Vec<_> = self.problem.layouts()[key].sorted_empty_nodes().to_vec();
                    if self.problem.apply_merge(key, p, s1, s2) {
                        self.n_merge += 1;
                        let layout = &self.problem.layouts()[key];
                        let mut after: Vec<_> = layout.sorted_empty_nodes().to_vec();
                        before.sort_unstable();
                        after.sort_unstable();
                        for n in before.iter().filter(|n| after.binary_search(n).is_err()) {
                            insertion_option_cache.remove_for_node(&li, n);
                        }
                        for n in after.iter().filter(|n| before.binary_search(n).is_err()) {
                            insertion_option_cache.add_for_node(n, &layout.nodes()[*n], &li, parttypes_to_consider.iter());
                        }
                        existing_layout_blueprints.clear();
                        new_layout_blueprints.clear();
                        elected_blueprint = GDRR::select_insertion_blueprint(
                            elected_parttype,
                            insertion_option_cache,
                            mat_limit_budget,
                            &mut self.problem,
                            self.config,
                            &mut existing_layout_blueprints,
                            &mut new_layout_blueprints,
                        );
                        debug_assert!(elected_blueprint.is_some());
                    }
                }
            }

            if let Some(elected_blueprint) = elected_blueprint.as_ref() {
                let cache_updates = self.problem.implement_insertion_blueprint(elected_blueprint);
                if self.problem.parttype_qtys()[elected_parttype.id()] == 0 {
                    parttypes_to_consider.retain(|pt| pt.id() != elected_parttype.id());
                }
                insertion_option_cache.update_cache(&cache_updates, &parttypes_to_consider, &self.problem);

                if let LayoutIndex::Empty(index) = elected_blueprint.layout_index() {
                    //update mat_limit_budget
                    let empty_layout = &self.problem.empty_layouts()[*index as usize];
                    mat_limit_budget -= empty_layout.sheettype().value as i128;
                    let sheettype_id = empty_layout.sheettype().id;

                    if self.problem.sheettype_qtys()[sheettype_id] == 0 {
                        //There is no more stock left of this sheettype, remove all empty layouts with this sheettype from the cache
                        for (i, layout) in self.problem.empty_layouts().iter().enumerate()
                            .filter(|(_, layout)| layout.sheettype().id == sheettype_id)
                        {
                            insertion_option_cache.remove_all_for_layout(&LayoutIndex::empty(i), layout);
                        }
                    }
                }
                if insertion_option_cache.is_empty() {
                    break;
                }

                debug_assert!(assertions::insertion_option_cache_is_valid(&self.problem, insertion_option_cache, &parttypes_to_consider), "{:#?}\n{:#?}", elected_blueprint, cache_updates);
            } else {
                //if there is no insertion blueprint, the part cannot be added to the problem
                part_area_not_included += self.problem.parttype_qtys()[elected_parttype.id()] as u64
                    * elected_parttype.area();

                parttypes_to_consider.retain(|pt| pt.id() != elected_parttype.id());

                debug_assert!(assertions::insertion_option_cache_is_valid(&self.problem, insertion_option_cache, &parttypes_to_consider), "{:#?}", elected_blueprint);
            }
        }
    }

    fn select_next_parttype(
        parttypes: &[&'a PartType],
        insertion_option_cache: &InsertionOptionCache<'a>,
        indices: &mut Vec<usize>,
        rand: &mut SmallRng,
        config: &Config,
    ) -> &'a PartType {
        indices.clear();
        indices.extend(0..parttypes.len());
        indices.shuffle(rand);

        let tie_area = config.part_tie_area;
        let n_options = indices.iter().map(|i| {
            let parttype = parttypes[*i];
            let c = insertion_option_cache.count_for_parttype(parttype);
            if tie_area {
                (c << 32) | (0xFFFF_FFFFu64.saturating_sub(parttype.area()) as usize)
            } else { c }
        });

        let rate = if config.part_blink_rate >= 0.0 { config.part_blink_rate } else { config.blink_rate };
        let blink = blink::select_lowest_entry(n_options, rate, rand);
        let parttype_index = indices[blink];
        parttypes[parttype_index]
    }

    fn select_insertion_blueprint(
        parttype: &'a PartType,
        insertion_option_cache: &InsertionOptionCache<'a>,
        mat_limit_budget: i128,
        problem: &mut Problem,
        config: &Config,
        existing_layout_blueprints: &mut Vec<InsertionBlueprint<'a>>,
        new_layout_blueprints: &mut Vec<InsertionBlueprint<'a>>,
    ) -> Option<InsertionBlueprint<'a>> {
        for option in insertion_option_cache.options_for_parttype(parttype) {
            if existing_layout_blueprints.len() > 20 {
                break; //enough blueprints to consider
            }

            if let LayoutIndex::Existing(_) = option.layout_index() {
                option.append_blueprints(problem, existing_layout_blueprints);
            }
        }

        if !existing_layout_blueprints.is_empty() {
            existing_layout_blueprints.sort_by(|a, b| {
                crate::COST_COMPARATOR(a.cost(), b.cost())
            });
            let selected_blinked_index = blink::select_lowest_in_range(0..existing_layout_blueprints.len(), config.blink_rate, problem.rng());
            return Some(existing_layout_blueprints.swap_remove(selected_blinked_index));
        }

        for option in insertion_option_cache.options_for_parttype(parttype) {
            if let LayoutIndex::Empty(i) = option.layout_index() {
                if mat_limit_budget >= problem.empty_layouts()[*i as usize].sheettype().value as i128 {
                    option.append_blueprints(problem, new_layout_blueprints);
                }
            }
        }

        if new_layout_blueprints.is_empty() {
            return None;
        }

        if mat_limit_budget < (u64::MAX / 2) as i128 && config.largest_bin_prob > 0.0
            && problem.rng().random::<f64>() < config.largest_bin_prob {
            // fill the material budget: choose among the blueprints of the largest affordable sheet type
            let value_of = |bp: &InsertionBlueprint, problem: &Problem| -> u64 {
                match bp.layout_index() {
                    LayoutIndex::Empty(i) => problem.empty_layouts()[*i as usize].sheettype().value,
                    _ => 0,
                }
            };
            let max_value = new_layout_blueprints.iter().map(|bp| value_of(bp, problem)).max().unwrap();
            let n_max = new_layout_blueprints.iter().filter(|bp| value_of(bp, problem) == max_value).count();
            let mut pick = problem.rng().random_range(0..n_max);
            let mut selected_index = 0;
            for (i, bp) in new_layout_blueprints.iter().enumerate() {
                if value_of(bp, problem) == max_value {
                    if pick == 0 { selected_index = i; break; }
                    pick -= 1;
                }
            }
            return Some(new_layout_blueprints.swap_remove(selected_index));
        }

        let selected_index = problem.rng().random_range(0..new_layout_blueprints.len());
        Some(new_layout_blueprints.swap_remove(selected_index))
    }
}

struct OptimizationStats {
    elapsed: std::time::Duration,
    n_iterations: usize,
    n_accepted: usize,
    n_improved: usize,
}

/// slot of (layout version, part size, repair kind) in the repair-failure table (1 << 14 entries)
#[inline]
fn repair_slot(version: u64, w: u64, h: u64, kind: u8) -> usize {
    let x = version.wrapping_mul(0x9E3779B97F4A7C15) ^ w.wrapping_mul(0xC2B2AE3D27D4EB4F) ^ h.wrapping_mul(0x165667B19E3779F9) ^ (kind as u64);
    ((x ^ (x >> 29)) as usize) & ((1 << 14) - 1)
}
