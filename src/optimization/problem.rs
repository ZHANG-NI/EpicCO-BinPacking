use rand::prelude::IndexedRandom;
use rand::SeedableRng;
use rand::rngs::SmallRng;
use slotmap::SlotMap;

use crate::core::cost::Cost;
use crate::core::entities::layout::Layout;
use crate::core::entities::node::NodeKey;
use crate::core::insertion::insertion_blueprint::InsertionBlueprint;
use crate::core::layout_index::{LayoutIndex, LayoutKey};
use crate::core::orientation::Orientation;
use crate::optimization::instance::Instance;
use crate::optimization::rr::cache_updates::IOCUpdates;
use crate::optimization::solutions::problem_solution::ProblemSolution;
use crate::optimization::solutions::solution::Solution;
use crate::util::assertions;

/// Problem is the main representation of the optimization problem.
/// A Problem is based on an Instance and contains a collection of Layouts.
/// Its main purpose is to be easily modifiable
/// It can create a snapshot of itself in the form of a ProblemSolution and use these to restore itself to a prior state.
pub struct Problem<'a> {
    instance: &'a Instance,
    parttype_qtys: Vec<usize>,
    part_area_excluded: u64,
    part_weights: Vec<u64>,
    weighted_excluded: u64,
    sheettype_qtys: Vec<usize>,
    layouts: ProblemLayouts<'a>,
    empty_layouts: Vec<Layout<'a>>,
    rng: SmallRng,
    solution_id_changed_layouts: Option<usize>,
    solution_id_counter: usize,
    layout_id_counter: usize,
    fit_deficit_on: bool,
    fit_deficit_max_items: usize,
}

impl<'a> Problem<'a> {
    pub fn new(
        instance: &'a Instance,
        random_seed: Option<u64>,
        leftover_valuation_power: f32,
    ) -> Self {
        let parttype_qtys = instance.parts().iter().map(|(_, qty)| *qty).collect::<Vec<_>>();
        let part_area_excluded = instance.total_part_area();
        let sheettype_qtys = instance.sheets().iter().map(|(_, qty)| *qty).collect::<Vec<_>>();
        let random = match random_seed {
            Some(seed) => SmallRng::seed_from_u64(seed),
            None => SmallRng::from_rng(&mut rand::rng())
        };

        let mut problem = Problem {
            instance,
            parttype_qtys,
            part_area_excluded,
            part_weights: vec![1; instance.parts().len()],
            weighted_excluded: part_area_excluded,
            sheettype_qtys,
            layouts : ProblemLayouts::new(),
            empty_layouts : Vec::new(),
            solution_id_changed_layouts : None,
            rng: random,
            solution_id_counter : 0,
            layout_id_counter : 0,
            fit_deficit_on: false,
            fit_deficit_max_items: usize::MAX,
        };

        //Initiate the empty layouts
        for (sheettype, _) in instance.sheets() {
            match sheettype.fixed_first_cut_orientation {
                Some(orientation) => {
                    let empty_layout = Layout::new(
                        problem.next_layout_id(),
                        sheettype,
                        orientation,
                        leftover_valuation_power,
                    );
                    problem.empty_layouts.push(empty_layout);
                }
                None => {
                    let empty_layout_h = Layout::new(
                        problem.next_layout_id(),
                        sheettype,
                        Orientation::Horizontal,
                        leftover_valuation_power,
                    );
                    let empty_layout_v = Layout::new(
                        problem.next_layout_id(),
                        sheettype,
                        Orientation::Vertical,
                        leftover_valuation_power,
                    );
                    problem.empty_layouts.extend([empty_layout_h, empty_layout_v]);
                }
            }
        }
        problem
    }

    /// Modifies the problem by inserting an part according to the InsertionBlueprint.
    /// It returns which updates should be made to the InsertionOptionCache and whether or not a new layout was created.
    pub fn implement_insertion_blueprint(&mut self, blueprint: &InsertionBlueprint<'a>) -> IOCUpdates {
        self.register_part(blueprint.parttype().id(), 1);

        match blueprint.layout_index() {
            LayoutIndex::Existing(index) => {
                let blueprint_layout = &mut self.layouts.live[*index];
                let mut cache_updates = IOCUpdates::new(
                    *blueprint.layout_index(),
                    *blueprint.original_node_index(),
                );
                blueprint_layout.implement_insertion_blueprint(blueprint, &mut cache_updates);

                self.layouts.mark_changed(*index);

                cache_updates
            }
            LayoutIndex::Empty(index) => {
                let next_layout_id = self.next_layout_id();
                let empty_layout = &self.empty_layouts[*index as usize];

                //Create a copy of the empty layout and register it
                let empty_layout_clone = empty_layout.clone_with_id(next_layout_id);
                let clone_index = self.register_layout(empty_layout_clone);

                //Implement the blueprint
                let mut cache_updates = IOCUpdates::new(
                    LayoutIndex::Existing(clone_index),
                    *blueprint.original_node_index(),
                );
                self.layouts.live[clone_index].implement_insertion_blueprint(blueprint, &mut cache_updates);

                cache_updates
            }
        }
    }

    pub fn remove_node(&mut self, node_index: NodeKey, layout_index: LayoutIndex) -> Option<u64> {
        let index = match layout_index {
            LayoutIndex::Empty(_) => panic!("Cannot remove a node from an empty layout"),
            LayoutIndex::Existing(index) => index,
        };
        self.layouts.mark_changed(index);

        if node_index == *self.layouts.live[index].top_node_index() {
            return Some(self.unregister_layout(layout_index));
        }

        let removed_part_ids = self.layouts.live[index].remove_node(node_index);
        for p_id in removed_part_ids {
            self.unregister_part(p_id, 1);
        }

        if self.layouts.live[index].is_empty() {
            Some(self.unregister_layout(layout_index))
        } else {
            None
        }
    }

    pub fn apply_merge(&mut self, key: LayoutKey, p: NodeKey, s1: NodeKey, s2: NodeKey) -> bool {
        self.layouts.mark_changed(key);
        self.layouts.live[key].apply_merge(p, s1, s2)
    }

    pub fn apply_stretch(&mut self, key: LayoutKey, e: NodeKey, d: u64) {
        self.layouts.mark_changed(key);
        self.layouts.live[key].apply_stretch(e, d);
    }

    pub fn cost(&mut self) -> Cost {
        let mut cost = self.layouts.live.iter_mut()
            .fold(Cost::empty(), |acc, (_,l)| acc + l.cost(false));

        cost.part_area_excluded = self.part_area_excluded();

        cost.part_area_included = self.instance.total_part_area() - cost.part_area_excluded;

        debug_assert_eq!(self.weighted_excluded, self.calc_weighted_excluded());
        cost.weighted_excluded = self.weighted_excluded;
        cost.fit_deficit = if self.fit_deficit_on && cost.part_area_excluded > 0 { self.calc_fit_deficit() } else { 0 };

        cost
    }

    pub fn set_fit_deficit(&mut self, on: bool, max_items: usize) {
        self.fit_deficit_on = on;
        self.fit_deficit_max_items = max_items;
    }

    /// Sum over excluded parts of min over the empty nodes of existing layouts of the part area left uncovered
    /// when the part is laid over the node (w*h - min(w,nw)*min(h,nh)).
    fn calc_fit_deficit(&self) -> u64 {
        let n_excl: usize = self.parttype_qtys.iter().sum();
        if n_excl > self.fit_deficit_max_items { return 0; }
        let mut total = 0u64;
        for (id, q) in self.parttype_qtys.iter().enumerate() {
            if *q == 0 { continue; }
            let pt = self.instance.parttype(id);
            let (w, h) = (pt.width(), pt.height());
            let a = w * h;
            let mut best = a;
            for (_, layout) in self.layouts.live.iter() {
                let nodes = layout.nodes();
                for nk in layout.sorted_empty_nodes() {
                    let n = &nodes[*nk];
                    let na = n.width() * n.height();
                    if a.saturating_sub(na) >= best { break; }
                    let d = a - u64::min(w, n.width()) * u64::min(h, n.height());
                    if d < best { best = d; if best == 0 { break; } }
                }
                if best == 0 { break; }
            }
            total += *q as u64 * best;
        }
        total
    }

    pub fn create_solution(&mut self, old_solution: Option<ProblemSolution<'a>>, cached_cost: Option<Cost>) -> ProblemSolution<'a> {
        debug_assert!(cached_cost.as_ref().is_none_or(|cost| cost == &self.cost()));
        self.layouts.discard_detached();
        let id = self.next_solution_id();
        let cost = cached_cost.unwrap_or_else(|| self.cost());
        let solution = match old_solution {
            Some(old_solution) => {
                debug_assert!(old_solution.id() == self.solution_id_changed_layouts.unwrap());
                ProblemSolution::new(self, cost, id, old_solution)
            }
            None => {
                ProblemSolution::new_force_copy_all(self, cost, id)
            }
        };

        debug_assert!(assertions::problem_matches_solution(self, &solution), "{:#?},{:#?}", id, self.solution_id_changed_layouts);

        self.reset_changed_layouts(solution.id());

        solution
    }

    pub fn restore_from_problem_solution(&mut self, solution: &ProblemSolution<'a>) {
        assert_eq!(
            self.solution_id_changed_layouts,
            Some(solution.id()),
            "can only restore the latest problem solution",
        );

        self.layouts.restore_from_solution(solution);

        self.parttype_qtys = solution.parttype_qtys().to_vec();
        self.part_area_excluded = solution.cost().part_area_excluded;
        self.weighted_excluded = solution.cost().weighted_excluded;
        self.sheettype_qtys = solution.sheettype_qtys().to_vec();

        debug_assert!(assertions::problem_matches_solution(self, solution));

        self.reset_changed_layouts(solution.id());
    }

    pub fn usage(&self) -> f64 {
        let total_included_part_area = self.instance.total_part_area() - self.part_area_excluded();
        let total_used_sheet_area = self.layouts().iter().map(
            |(_, layout)| { layout.sheettype().area() }
        ).sum::<u64>();

        total_included_part_area as f64 / total_used_sheet_area as f64
    }

    fn calc_weighted_excluded(&self) -> u64 {
        self.parttype_qtys.iter().zip(self.part_weights.iter()).enumerate()
            .filter(|(_, (q, _))| **q > 0)
            .map(|(id, (q, w))| *q as u64 * self.instance.parttype(id).area() * *w)
            .sum()
    }

    /// Guided local search: among the part types that are currently excluded, raise the weight of the one
    /// with the highest utility (excluded area / current weight), as in standard GLS.
    /// Returns true if a weight was raised.
    pub fn bump_excluded_weights(&mut self) -> bool {
        // offer S2_v2: raise the weight of every part type that is currently excluded
        let mut bumped = false;
        for (q, w) in self.parttype_qtys.iter().zip(self.part_weights.iter_mut()) {
            if *q > 0 {
                *w += 1;
                bumped = true;
            }
        }
        self.weighted_excluded = self.calc_weighted_excluded();
        bumped
    }

    pub fn reset_part_weights(&mut self) {
        self.part_weights.iter_mut().for_each(|w| *w = 1);
        self.weighted_excluded = self.calc_weighted_excluded();
    }

    pub fn instance(&self) -> &'a Instance {
        self.instance
    }

    pub fn parttype_qtys(&self) -> &[usize] {
        &self.parttype_qtys
    }

    pub fn sheettype_qtys(&self) -> &[usize] {
        &self.sheettype_qtys
    }

    /// Number of sheets of each sheet type currently in use
    pub fn used_sheet_counts(&self) -> Vec<usize> {
        let mut counts = vec![0usize; self.sheettype_qtys.len()];
        for (_, l) in self.layouts.live.iter() {
            counts[l.sheettype().id] += 1;
        }
        counts
    }

    /// Restrict (Some) or restore (None) the available stock of every sheet type.
    /// With caps, at most caps[t] sheets of type t may be in use (the used ones must already respect it).
    pub fn set_stock_caps(&mut self, caps: Option<&[usize]>) {
        let used = self.used_sheet_counts();
        for t in 0..self.sheettype_qtys.len() {
            let stock = self.instance.sheettype_qty(t).unwrap();
            let cap = match caps { Some(c) => usize::min(c[t], stock), None => stock };
            self.sheettype_qtys[t] = cap.saturating_sub(used[t]);
        }
    }

    pub fn rng(&mut self) -> &mut SmallRng {
        &mut self.rng
    }

    pub fn choose_removable_node(&mut self, layout_index: LayoutKey) -> NodeKey {
        let layouts = &self.layouts.live;
        *layouts[layout_index]
            .removable_nodes()
            .choose(&mut self.rng)
            .expect("layout has no removable node")
    }

    pub fn layouts(&self) -> &SlotMap<LayoutKey, Layout<'a>> {
        &self.layouts.live
    }

    pub fn layout_keys(&self) -> &[LayoutKey] {
        self.layouts.keys()
    }

    pub fn layout(&self, layout_index: &LayoutIndex) -> &Layout<'a>{
        match layout_index{
            LayoutIndex::Existing(index) => &self.layouts.live[*index],
            LayoutIndex::Empty(index) => &self.empty_layouts[*index as usize],
        }
    }

    pub fn register_layout(&mut self, layout: Layout<'a>) -> LayoutKey {
        self.register_sheet(layout.sheettype().id, 1);
        for parttype_id in layout.included_part_ids() {
            self.register_part(parttype_id, 1);
        }
        self.layouts.insert(layout)
    }

    pub fn unregister_layout(&mut self, layout_index: LayoutIndex) -> u64 {
        match layout_index {
            LayoutIndex::Empty(_) => panic!("Cannot unregister empty layout"),
            LayoutIndex::Existing(li) => {
                let layout = &self.layouts.live[li];
                let sheettype_id = layout.sheettype().id;
                let sheet_value = layout.sheettype().value;
                let included_parts = layout.included_part_ids();
                self.layouts.detach(li);

                self.unregister_sheet(sheettype_id, 1);
                for parttype_id in included_parts {
                    self.unregister_part(parttype_id, 1);
                }
                sheet_value
            }
        }
    }

    fn reset_changed_layouts(&mut self, solution_id_changed_layouts: usize) {
        self.layouts.reset_changes();
        self.solution_id_changed_layouts = Some(solution_id_changed_layouts);
    }

    fn register_part(&mut self, parttype_id: usize, qty: usize) {
        debug_assert!(self.parttype_qtys[parttype_id] >= qty);
        self.parttype_qtys[parttype_id] -= qty;
        self.part_area_excluded -= self.instance.parttype(parttype_id).area() * qty as u64;
        self.weighted_excluded -= self.instance.parttype(parttype_id).area() * qty as u64 * self.part_weights[parttype_id];
    }

    fn unregister_part(&mut self, parttype_id: usize, qty: usize) {
        debug_assert!(self.parttype_qtys[parttype_id] + qty <= self.instance.parttype_qty(parttype_id).unwrap());
        self.parttype_qtys[parttype_id] += qty;
        self.part_area_excluded += self.instance.parttype(parttype_id).area() * qty as u64;
        self.weighted_excluded += self.instance.parttype(parttype_id).area() * qty as u64 * self.part_weights[parttype_id];
    }

    fn register_sheet(&mut self, sheettype_id: usize, qty: usize) {
        debug_assert!(self.sheettype_qtys[sheettype_id] >= qty);
        self.sheettype_qtys[sheettype_id] -= qty;
    }

    fn unregister_sheet(&mut self, sheettype_id: usize, qty: usize) {
        debug_assert!(self.sheettype_qtys[sheettype_id] + qty <= self.instance.sheettype_qty(sheettype_id).unwrap());
        self.sheettype_qtys[sheettype_id] += qty;
    }

    fn next_layout_id(&mut self) -> usize {
        self.layout_id_counter += 1;
        self.layout_id_counter
    }

    fn next_solution_id(&mut self) -> usize {
        self.solution_id_counter += 1;
        self.solution_id_counter
    }

    pub fn empty_layouts(&self) -> &[Layout<'a>] {
        &self.empty_layouts
    }

    pub fn changed_layouts(&self) -> &[LayoutKey] {
        &self.layouts.changed
    }

    fn part_area_excluded(&self) -> u64 {
        debug_assert_eq!(
            self.part_area_excluded,
            self.parttype_qtys.iter().enumerate().fold(0, |area, (id, qty)| {
                area + self.instance.parttype(id).area() * *qty as u64
            }),
        );
        self.part_area_excluded
    }
}

/// Owns live layout membership and the bookkeeping needed to sample, snapshot, and restore it.
///
/// All insertions, detachments, and restores pass through this type so live keys, detached layouts,
/// and the changed-layout set stay synchronized with the layout arena.
struct ProblemLayouts<'a> {
    live: SlotMap<LayoutKey, Layout<'a>>,
    keys: Vec<LayoutKey>,
    detached: Vec<(LayoutKey, Layout<'a>)>,
    changed: Vec<LayoutKey>,
}

impl<'a> ProblemLayouts<'a> {
    fn new() -> Self {
        Self {
            live: SlotMap::with_key(),
            keys: Vec::new(),
            detached: Vec::new(),
            changed: Vec::new(),
        }
    }

    fn insert(&mut self, layout: Layout<'a>) -> LayoutKey {
        let key = self.live.insert(layout);
        debug_assert!(!self.keys.contains(&key));
        self.keys.push(key);
        self.mark_changed(key);
        key
    }

    fn detach(&mut self, key: LayoutKey) {
        let layout = self.live.detach(key).expect("Layout not found");
        let position = self.keys.iter()
            .position(|candidate| *candidate == key)
            .expect("live layout key is not tracked");
        self.keys.swap_remove(position);
        self.detached.push((key, layout));
        self.mark_changed(key);
    }

    fn restore_from_solution(&mut self, solution: &ProblemSolution<'a>) {
        for key in std::mem::take(&mut self.changed) {
            match (self.live.contains_key(key), solution.layouts().get(key)) {
                (true, Some(layout)) => self.live[key].restore_from(layout),
                (true, None) => {
                    self.live.remove(key);
                    self.untrack(key);
                },
                (false, Some(layout)) => {
                    let detached_index = self.detached.iter()
                        .position(|(detached_key, _)| *detached_key == key)
                        .expect("changed layout key was not detached");
                    self.detached.swap_remove(detached_index);
                    self.live.reattach(key, layout.as_ref().clone());
                    self.keys.push(key);
                },
                (false, None) => (),
            }
        }
        self.discard_detached();
    }

    fn discard_detached(&mut self) {
        for (key, layout) in self.detached.drain(..) {
            self.live.reattach(key, layout);
            self.live.remove(key);
        }
    }

    fn mark_changed(&mut self, key: LayoutKey) {
        if !self.changed.contains(&key) {
            self.changed.push(key);
        }
    }

    fn reset_changes(&mut self) {
        self.changed.clear();
    }

    fn keys(&self) -> &[LayoutKey] {
        debug_assert_eq!(self.keys.len(), self.live.len());
        debug_assert!(self.keys.iter().all(|key| self.live.contains_key(*key)));
        debug_assert!(self.keys.iter().enumerate().all(|(i, key)| !self.keys[..i].contains(key)));
        &self.keys
    }

    fn untrack(&mut self, key: LayoutKey) {
        let position = self.keys.iter()
            .position(|candidate| *candidate == key)
            .expect("live layout key is not tracked");
        self.keys.swap_remove(position);
    }
}

impl<'a> PartialEq for Problem<'a> {
    fn eq(&self, other: &Problem<'a>) -> bool {
        std::ptr::eq(self, other)
    }
}
