use slotmap::SlotMap;
use itertools::Itertools;
use crate::core::{
    cost::Cost,
    insertion::insertion_blueprint::{InsertionBlueprint, InsertionNodeKind},
};
use crate::core::entities::node::{Node, NodeKey};
use crate::core::orientation::Orientation;
use crate::optimization::rr::cache_updates::IOCUpdates;
use crate::util::assertions;

use super::sheettype::SheetType;

#[derive(Debug, Clone)]
pub struct Layout<'a> {
    id : usize,
    sheettype: &'a SheetType,
    leftover_valuation_power: f32,
    nodes: LayoutNodes<'a>,
    cached_cost: Option<Cost>,
    /// id of the current content state: unique per mutation, copied with the content (clone / restore)
    version: u64,
}

static LAYOUT_VERSION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
fn next_layout_version() -> u64 { LAYOUT_VERSION.fetch_add(1, std::sync::atomic::Ordering::Relaxed) }

impl<'a> Layout<'a> {
    pub fn new(
        id: usize,
        sheettype: &'a SheetType,
        first_cut_orientation: Orientation,
        leftover_valuation_power: f32,
    ) -> Self {
        let top_node = Node::new(0, sheettype.width, sheettype.height, first_cut_orientation, None);
        let nodes = LayoutNodes::new(top_node);

        let mut layout = Self {
            id,
            sheettype,
            leftover_valuation_power,
            nodes,
            cached_cost: None,
            version: next_layout_version(),
        };

        //The top node cannot be modified, so we register a placeholder node to be able to insert parts
        let placeholder_node = Node::new(1, sheettype.width, sheettype.height, first_cut_orientation.rotate(), None);
        layout.register_node(placeholder_node, layout.nodes.top_node, true);

        layout
    }

    pub fn clone_with_id(&self, id : usize) -> Self{
        Self {
            id,
            ..self.clone()
        }
    }

    pub(crate) fn restore_from(&mut self, snapshot: &Self) {
        self.id = snapshot.id;
        self.sheettype = snapshot.sheettype;
        self.leftover_valuation_power = snapshot.leftover_valuation_power;
        self.nodes.restore_from(&snapshot.nodes);
        self.cached_cost.clone_from(&snapshot.cached_cost);
        self.version = snapshot.version;
    }

    /// id of the current content state (equal ids => identical node trees)
    pub fn version(&self) -> u64 {
        self.version
    }

    pub fn implement_insertion_blueprint(
        &mut self,
        blueprint: &InsertionBlueprint<'a>,
        updates: &mut IOCUpdates,
    ) {
        let original = *blueprint.original_node_index();
        let parent = self.nodes.arena[original].parent().expect("original node has no parent");
        let insertion_nodes = blueprint.nodes(&self.nodes.arena[original]);

        //unregister the original node
        self.unregister_node(original, &mut None);

        //create and register the replacements
        let mut inserted_nodes = [None; 5];
        for (i, insertion_node) in insertion_nodes.into_iter().enumerate() {
            let Some(insertion_node) = insertion_node else {
                break;
            };
            let parent = match insertion_node.parent {
                Some(parent_i) => {
                    debug_assert!(parent_i < i);
                    inserted_nodes[parent_i].expect("insertion node parent has not been created")
                }
                None => parent,
            };
            let parttype = match insertion_node.kind {
                InsertionNodeKind::Part => Some(blueprint.parttype()),
                InsertionNodeKind::Structure | InsertionNodeKind::Empty => None,
            };
            let is_empty = insertion_node.kind == InsertionNodeKind::Empty;
            let node = Node::new(
                self.nodes.arena[parent].level() + 1,
                insertion_node.width,
                insertion_node.height,
                insertion_node.next_cut_orient,
                parttype,
            );
            let node_index = self.register_node(node, parent, is_empty);
            if is_empty {
                updates.add_new_empty(node_index);
            }
            inserted_nodes[i] = Some(node_index);
        }

        debug_assert!(assertions::children_nodes_fit(&parent, &self.nodes.arena), "{:#?}", blueprint);
        self.nodes.debug_assert_valid();
    }

    pub fn remove_node(&mut self, node_index: NodeKey) -> Vec<usize>{
        /*®
           Scenario 1: Empty node present + other child(ren)
            -> expand existing waste piece

             ---******               ---******
                *$$$$*                  *$$$$*
                ******                  ******
                *XXXX*     ----->       *    *
                ******                  *    *
                *    *                  *    *
             ---******               ---******

             Scenario 2: No waste piece present
                -> convert Node to be removed into waste Node

             ---******               ---******
                *$$$$*                  *$$$$*
                ******    ----->        ******
                *XXXX*                  *    *
             ---******               ---******

             Scenario 3: No other children present besides waste piece
                -> convert parent into waste piece

             ---******               ---******
                *XXXX*                  *    *
                ******    ----->        *    *
                *    *                  *    *
             ---******               ---******

         */

        let parent_node_index = self.nodes.arena[node_index].parent().expect("Cannot remove a node without a parent");
        let parent_node = &self.nodes.arena[parent_node_index];

        //Check if there is an empty_node present
        let empty_node = parent_node.last_child.filter(|child| self.nodes.arena[*child].is_empty());
        debug_assert_eq!(
            empty_node,
            parent_node.children(&self.nodes.arena).find(|child| self.nodes.arena[*child].is_empty()),
        );

        let mut removed_parts = Some(vec![]);

        match empty_node {
            Some(empty_node_index) => {
                //Scenario 1 and 3
                if parent_node.first_child != parent_node.last_child || parent_node.parent().is_none() {
                    //Scenario 1 (also do this when the parent node is the root)
                    //Two children are merged into one

                    let node = &self.nodes.arena[node_index];
                    let empty_node = &self.nodes.arena[empty_node_index];
                    let replacement_node = match parent_node.next_cut_orient() {
                        Orientation::Horizontal => {
                            let new_height = empty_node.height() + node.height();
                            Node::new(node.level(), node.width(), new_height, node.next_cut_orient(), None)
                        }
                        Orientation::Vertical => {
                            let new_width = empty_node.width() + node.width();
                            Node::new(node.level(), new_width, node.height(), node.next_cut_orient(), None)
                        }
                    };

                    //Replace the empty node and the node to be removed with a enlarged empty node
                    self.unregister_node(empty_node_index, &mut removed_parts);
                    self.unregister_node(node_index, &mut removed_parts);
                    self.register_node(replacement_node, parent_node_index, true);
                } else {
                    //Scenario 3: replace the parent with an empty node
                    let grandparent_index = parent_node.parent().expect("grandparent node needs to be present");

                    //create empty parent
                    let empty_parent_node = Node::new(parent_node.level(), parent_node.width(), parent_node.height(), parent_node.next_cut_orient(), None);

                    //replace
                    self.unregister_node(parent_node_index, &mut removed_parts);
                    self.register_node(empty_parent_node, grandparent_index, true);
                }
            }
            None => {
                //Scenario 2: convert the node itself into an empty node

                //create empty replacement node
                let node = &self.nodes.arena[node_index];
                let replacement_node = Node::new(node.level(), node.width(), node.height(), node.next_cut_orient(), None);

                //replace
                self.unregister_node(node_index, &mut removed_parts);
                self.register_node(replacement_node, parent_node_index, true);
            }
        }

        self.nodes.debug_assert_valid();

        removed_parts.unwrap()
    }

    /// Find a stretch site for a part of size (pw, ph): an empty node E whose parent A lies in a parent P that
    /// stacks along X and has an empty last child R; E is wide enough across X but short along X by d <= R's X
    /// extent. Returns (E, d) with the least area taken from R.
    pub fn find_stretch_site(&self, pw: u64, ph: u64) -> Option<(NodeKey, u64)> {
        let arena = &self.nodes.arena;
        let mut best: Option<(u64, NodeKey, u64)> = None;
        for &e in self.nodes.empty_nodes_by_area.iter() {
            let en = &arena[e];
            // exactly one dimension must already suffice (both: it would fit directly)
            if (en.width() >= pw) == (en.height() >= ph) { continue; }
            let Some(a) = en.parent() else { continue };
            let Some(p) = arena[a].parent() else { continue };
            let pn = &arena[p];
            let Some(r) = pn.last_child.filter(|r| *r != a && arena[*r].is_empty()) else { continue };
            let rn = &arena[r];
            // X = stacking direction of P
            let (e_x, e_y, r_x, p_x, p_y) = match pn.next_cut_orient() {
                Orientation::Horizontal => (en.height(), en.width(), rn.height(), ph, pw),
                Orientation::Vertical => (en.width(), en.height(), rn.width(), pw, ph),
            };
            if e_y < p_y || e_x >= p_x { continue; }
            let d = p_x - e_x;
            if d > r_x { continue; }
            let a_y = match pn.next_cut_orient() {
                Orientation::Horizontal => arena[a].width(),
                Orientation::Vertical => arena[a].height(),
            };
            let taken = d * a_y;
            if best.is_none_or(|(t, _, _)| taken < t) {
                best = Some((taken, e, d));
            }
        }
        best.map(|(_, e, d)| (e, d))
    }

    /// Apply a stretch found by find_stretch_site: grow E's parent A by d along X, taking it from R.
    /// Returns the grown empty node (former E).
    pub fn apply_stretch(&mut self, e: NodeKey, d: u64) -> NodeKey {
        let a = self.nodes.arena[e].parent().unwrap();
        let p = self.nodes.arena[a].parent().unwrap();
        let r = self.nodes.arena[p].last_child.unwrap();
        let x_is_height = self.nodes.arena[p].next_cut_orient() == Orientation::Horizontal;
        let grow = |w: u64, h: u64| if x_is_height { (w, h + d) } else { (w + d, h) };
        let cap = |w: u64, h: u64| if x_is_height { (w, d) } else { (d, h) };
        // shrink R
        let rn = &self.nodes.arena[r];
        let (rw, rh, ro, rl) = (rn.width(), rn.height(), rn.next_cut_orient(), rn.level());
        self.unregister_node(r, &mut None);
        // remove E (re-added last, grown)
        let en = &self.nodes.arena[e];
        let (ew, eh, eo, el) = (en.width(), en.height(), en.next_cut_orient(), en.level());
        self.unregister_node(e, &mut None);
        // grow the other children of A
        let children: Vec<NodeKey> = self.nodes.arena[a].children(&self.nodes.arena).collect();
        for c in children {
            let cn = &self.nodes.arena[c];
            let (cw, ch, co, cl) = (cn.width(), cn.height(), cn.next_cut_orient(), cn.level());
            if let Some(pt) = cn.parttype_ref() {
                // wrap the item: structure (grown) = [item, cap]
                self.unregister_node(c, &mut None);
                let (gw, gh) = grow(cw, ch);
                let s = self.register_node(Node::new(cl, gw, gh, co, None), a, false);
                self.register_node(Node::new(cl + 1, cw, ch, co.rotate(), Some(pt)), s, false);
                let (kw, kh) = cap(cw, ch);
                self.register_node(Node::new(cl + 1, kw, kh, co.rotate(), None), s, true);
            } else {
                // structure stacking along X: grow its last empty child or append a cap
                let last = self.nodes.arena[c].last_child.unwrap();
                let (lw, lh, lo, ll, l_empty) = {
                    let ln = &self.nodes.arena[last];
                    (ln.width(), ln.height(), ln.next_cut_orient(), ln.level(), ln.is_empty())
                };
                if l_empty {
                    self.unregister_node(last, &mut None);
                    let (gw, gh) = grow(lw, lh);
                    self.register_node(Node::new(ll, gw, gh, lo, None), c, true);
                } else {
                    let (kw, kh) = cap(cw, ch);
                    self.register_node(Node::new(cl + 1, kw, kh, co.rotate(), None), c, true);
                }
                let (gw, gh) = grow(cw, ch);
                self.nodes.arena[c].set_size(gw, gh);
            }
        }
        let (gw, gh) = grow(ew, eh);
        let new_e = self.register_node(Node::new(el, gw, gh, eo, None), a, true);
        let an = &self.nodes.arena[a];
        let (gw, gh) = grow(an.width(), an.height());
        self.nodes.arena[a].set_size(gw, gh);
        let (nrw, nrh) = if x_is_height { (rw, rh - d) } else { (rw - d, rh) };
        if nrw > 0 && nrh > 0 {
            self.register_node(Node::new(rl, nrw, nrh, ro, None), p, true);
        }
        self.invalidate_caches();
        self.nodes.debug_assert_valid();
        new_e
    }

    /// Find a strip-merge site for a part of size (pw, ph): two structure children S1, S2 of a node P (P stacks
    /// along X, the strips stack along Y) whose content heights h1, h2 leave a free band of Y - max(h1, h2) at the
    /// far end of both strips; joined, that band is a (x1 + x2) x (Y - max) rectangle that can hold the part.
    /// Returns (P, S1, S2) with the smallest x1 + x2 (then the smallest fragmented area).
    /// Candidate strips are enumerated from the empty nodes (a strip's band is its empty last child).
    pub fn find_merge_site(&self, pw: u64, ph: u64, cands: &mut Vec<MergeCand>) -> Option<(NodeKey, NodeKey, NodeKey)> {
        let arena = &self.nodes.arena;
        let min_dim = pw.min(ph);
        cands.clear();
        for &e in self.nodes.empty_nodes_by_area.iter() {
            let en = &arena[e];
            if en.width() < min_dim && en.height() < min_dim { continue; }
            let Some(s) = en.parent() else { continue };
            let sn = &arena[s];
            if sn.first_child == Some(e) { continue; }
            let Some(p) = sn.parent() else { continue };
            let x_is_height = arena[p].next_cut_orient() == Orientation::Horizontal;
            let (big_y, p_x, p_y) = if x_is_height { (arena[p].width(), ph, pw) } else { (arena[p].height(), pw, ph) };
            let free = if x_is_height { en.width() } else { en.height() };
            if free < p_y { continue; }
            let x = if x_is_height { sn.height() } else { sn.width() };
            cands.push((p, s, x, big_y - free, big_y, p_x, p_y));
        }
        if cands.len() < 2 { return None; }
        cands.sort_unstable_by_key(|c| c.0);
        let mut best: Option<(u64, u64, NodeKey, NodeKey, NodeKey)> = None;
        let mut i0 = 0;
        while i0 < cands.len() {
            let mut i1 = i0 + 1;
            while i1 < cands.len() && cands[i1].0 == cands[i0].0 { i1 += 1; }
            for i in i0..i1 {
                for j in (i + 1)..i1 {
                    let (p, a, xa, ha, big_y, p_x, p_y) = cands[i];
                    let (_, b, xb, hb, _, _, _) = cands[j];
                    let hm = ha.max(hb);
                    if xa + xb < p_x || big_y - hm < p_y { continue; }
                    let frag = xa * (hm - ha) + xb * (hm - hb);
                    let cand = (xa + xb, frag, p, a, b);
                    if best.is_none_or(|bb| (cand.0, cand.1) < (bb.0, bb.1)) { best = Some(cand); }
                }
            }
            i0 = i1;
        }
        debug_assert_eq!(best.map(|(x, f, _, _, _)| (x, f)), self.find_merge_site_scan(pw, ph));
        best.map(|(_, _, p, a, b)| (p, a, b))
    }

    /// Find a strip-merge site for a part of size (pw, ph): two structure children S1, S2 of a node P (P stacks
    /// along X, the strips stack along Y) whose content heights h1, h2 leave a free band of Y - max(h1, h2) at the
    /// far end of both strips; joined, that band is a (x1 + x2) x (Y - max) rectangle that can hold the part.
    /// Returns (P, S1, S2) with the smallest x1 + x2 (then the smallest fragmented area).
    pub fn find_merge_site_scan(&self, pw: u64, ph: u64) -> Option<(u64, u64)> {
        let arena = &self.nodes.arena;
        let mut best: Option<(u64, u64, NodeKey, NodeKey, NodeKey)> = None;
        let mut strips: Vec<(NodeKey, u64, u64)> = Vec::new();
        for (p, pn) in arena.iter() {
            if !pn.has_children() || pn.parttype().is_some() { continue; }
            let x_is_height = pn.next_cut_orient() == Orientation::Horizontal;
            let (big_y, p_x, p_y) = if x_is_height { (pn.width(), ph, pw) } else { (pn.height(), pw, ph) };
            if big_y < p_y { continue; }
            strips.clear();
            for c in pn.children(arena) {
                let cn = &arena[c];
                if !cn.has_children() { continue; }
                let x = if x_is_height { cn.height() } else { cn.width() };
                let free = cn.last_child.filter(|l| arena[*l].is_empty())
                    .map(|l| if x_is_height { arena[l].width() } else { arena[l].height() }).unwrap_or(0);
                if free < p_y || free >= big_y { continue; }
                strips.push((c, x, big_y - free));
            }
            for i in 0..strips.len() {
                for j in (i + 1)..strips.len() {
                    let (a, xa, ha) = strips[i];
                    let (b, xb, hb) = strips[j];
                    let hm = ha.max(hb);
                    if xa + xb < p_x || big_y - hm < p_y { continue; }
                    let frag = xa * (hm - ha) + xb * (hm - hb);
                    let cand = (xa + xb, frag, p, a, b);
                    if best.is_none_or(|bb| (cand.0, cand.1) < (bb.0, bb.1)) { best = Some(cand); }
                }
            }
        }
        best.map(|(x, f, _, _, _)| (x, f))
    }

    fn snapshot(&self, n: NodeKey, depth: u8, max_depth: &mut u8) -> Snap<'a> {
        let node = &self.nodes.arena[n];
        *max_depth = (*max_depth).max(depth);
        Snap {
            width: node.width(),
            height: node.height(),
            orient: node.next_cut_orient(),
            parttype: node.parttype_ref(),
            children: node.children(&self.nodes.arena).map(|c| self.snapshot(c, depth + 1, max_depth)).collect(),
        }
    }

    fn rebuild(&mut self, snap: &Snap<'a>, parent: NodeKey) {
        let level = self.nodes.arena[parent].level() + 1;
        let is_empty = snap.parttype.is_none() && snap.children.is_empty();
        let k = self.register_node(Node::new(level, snap.width, snap.height, snap.orient, snap.parttype), parent, is_empty);
        for c in snap.children.iter() {
            self.rebuild(c, k);
        }
    }

    /// Apply a merge found by find_merge_site: P's children S1, S2 are replaced by M = [T = [S1', S2'], E], where
    /// Si' is Si cut to the common content height H and E is the joined free band. Returns false (nothing changed)
    /// if the tree would become too deep.
    pub fn apply_merge(&mut self, p: NodeKey, s1: NodeKey, s2: NodeKey) -> bool {
        let arena = &self.nodes.arena;
        let pn = &arena[p];
        let p_orient = pn.next_cut_orient();
        let s_orient = p_orient.rotate();
        let x_is_height = p_orient == Orientation::Horizontal;
        let big_y = if x_is_height { pn.width() } else { pn.height() };
        let mk = |x: u64, y: u64| if x_is_height { (y, x) } else { (x, y) };
        let mut max_depth = 0u8;
        let mut parts = Vec::with_capacity(2);
        for s in [s1, s2] {
            let sn = &arena[s];
            let x = if x_is_height { sn.height() } else { sn.width() };
            let mut kids: Vec<Snap<'a>> = sn.children(arena).map(|c| self.snapshot(c, 1, &mut max_depth)).collect();
            let free = match kids.last() {
                Some(l) if l.parttype.is_none() && l.children.is_empty() => {
                    let f = if x_is_height { l.width } else { l.height };
                    kids.pop();
                    f
                }
                _ => 0,
            };
            parts.push((x, big_y - free, kids));
        }
        let level_p = pn.level();
        if level_p as u32 + max_depth as u32 + 4 >= self.sheettype.max_stages as u32 { return false; }
        let hm = parts[0].1.max(parts[1].1);
        let big_x = parts[0].0 + parts[1].0;
        let r = pn.last_child.filter(|l| *l != s1 && *l != s2 && arena[*l].is_empty())
            .map(|l| (l, arena[l].width(), arena[l].height(), arena[l].next_cut_orient()));
        if let Some((rk, _, _, _)) = r { self.unregister_node(rk, &mut None); }
        self.unregister_node(s1, &mut None);
        self.unregister_node(s2, &mut None);
        let (mw, mh) = mk(big_x, big_y);
        let m = self.register_node(Node::new(level_p + 1, mw, mh, s_orient, None), p, false);
        let (tw, th) = mk(big_x, hm);
        let t = self.register_node(Node::new(level_p + 2, tw, th, p_orient, None), m, false);
        for (x, h, kids) in parts.iter() {
            let (sw, sh) = mk(*x, hm);
            let si = self.register_node(Node::new(level_p + 3, sw, sh, s_orient, None), t, false);
            for kid in kids.iter() { self.rebuild(kid, si); }
            if hm > *h {
                let (ew, eh) = mk(*x, hm - *h);
                self.register_node(Node::new(level_p + 4, ew, eh, p_orient, None), si, true);
            }
        }
        let (ew, eh) = mk(big_x, big_y - hm);
        self.register_node(Node::new(level_p + 2, ew, eh, p_orient, None), m, true);
        if let Some((_, rw, rh, ro)) = r {
            self.register_node(Node::new(level_p + 1, rw, rh, ro, None), p, true);
        }
        #[cfg(debug_assertions)]
        {
            let bad: Vec<_> = self.nodes.arena.iter().filter(|(k, n)| n.is_empty() != self.nodes.empty_nodes_by_area.contains(k)).map(|(_, n)| (n.level(), n.width(), n.height(), n.has_children(), n.parttype().is_some())).collect();
            if !bad.is_empty() { panic!("MERGE BAD {:?} big_x {} hm {} big_y {} parts {:?}", bad, big_x, hm, big_y, parts.iter().map(|(x,h,k)| (*x,*h,k.len())).collect::<Vec<_>>()); }
        }
        self.nodes.debug_assert_valid();
        true
    }

    fn invalidate_caches(&mut self) {
        self.cached_cost = None;
        self.version = next_layout_version();
    }

    fn calculate_cost(&self) -> Cost {
        debug_assert!(assertions::cached_sorted_empty_nodes_correct(&self.nodes.arena, &self.nodes.empty_nodes_by_area));
        let material_cost = Cost::empty().add_material_cost(self.sheettype.value);
        self.nodes.empty_nodes_by_area.iter()
            .map(|node_index| self.nodes.arena[*node_index].calculate_cost(self.leftover_valuation_power))
            .fold(material_cost, |acc, cost| acc + cost)
    }

    fn calculate_usage(&self) -> f64 {
        self.nodes.used_part_area as f64 / self.sheettype.area() as f64
    }

    fn register_node(&mut self, node: Node<'a>, parent: NodeKey, is_empty: bool) -> NodeKey {
        self.invalidate_caches();
        self.nodes.insert_child(node, parent, is_empty)
    }

    fn unregister_node(&mut self, node_index: NodeKey, removed_part_ids: &mut Option<Vec<usize>>) {
        self.invalidate_caches();
        self.nodes.remove_subtree(node_index, removed_part_ids);
    }

    pub fn included_part_ids(&self) -> Vec<usize> {
        self.nodes.arena.iter()
            .filter_map(|(_, node)| node.parttype().map(|parttype| parttype.id()))
            .collect_vec()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.arena.iter().all(|(_, n)| n.is_empty())
    }

    pub fn cost(&mut self, force_recalc: bool) -> Cost {
        let cost = match (self.cached_cost.as_ref(), force_recalc) {
            (Some(cost), false) => cost.clone(),
            _ => {
                let cost = self.calculate_cost();
                self.cached_cost = Some(cost.clone());
                cost
            }
        };
        debug_assert!(force_recalc || cost == self.cost(true));
        cost
    }

    pub fn cost_immut(&self, force_recalc: bool) -> Cost {
        let cost = match (self.cached_cost.as_ref(), force_recalc) {
            (Some(cost), false) => cost.clone(),
            _ => self.calculate_cost(),
        };
        debug_assert!(force_recalc || cost == self.cost_immut(true));
        cost
    }

    pub fn usage(&self) -> f64 {
        debug_assert!(assertions::cached_used_part_area_correct(&self.nodes.arena, self.nodes.used_part_area));
        self.calculate_usage()
    }

    pub fn sorted_empty_nodes(&self) -> &[NodeKey] {
        debug_assert!(assertions::node_arena_valid(&self.nodes.arena, &self.nodes.top_node), "{:#?}", self.nodes.empty_nodes_by_area.iter().map(|n| &self.nodes.arena[*n]).collect_vec());
        debug_assert!(assertions::cached_sorted_empty_nodes_correct(&self.nodes.arena, &self.nodes.empty_nodes_by_area), "{:#?}", self.nodes.empty_nodes_by_area.iter().map(|n| &self.nodes.arena[*n]).collect_vec());
        &self.nodes.empty_nodes_by_area
    }

    pub fn removable_nodes(&self) -> &[NodeKey] {
        debug_assert!(assertions::cached_removable_nodes_correct(&self.nodes.arena, &self.nodes.removable_nodes));
        &self.nodes.removable_nodes
    }

    pub fn sheettype(&self) -> &'a SheetType {
        self.sheettype
    }

    pub fn leftover_valuation_power(&self) -> f32 {
        self.leftover_valuation_power
    }

    pub fn top_node_index(&self) -> &NodeKey {
        &self.nodes.top_node
    }

    pub fn nodes(&self) -> &SlotMap<NodeKey, Node<'a>> {
        &self.nodes.arena
    }

}

/// Owns a layout's mutable node topology and its derived lookup state.
///
/// Keeping mutations here prevents the arena links, root, used area, and cached node lists from drifting apart.
#[derive(Debug, Clone)]
struct LayoutNodes<'a> {
    arena: SlotMap<NodeKey, Node<'a>>,
    top_node: NodeKey,
    used_part_area: u64,
    empty_nodes_by_area: Vec<NodeKey>,
    removable_nodes: Vec<NodeKey>,
}

impl<'a> LayoutNodes<'a> {
    fn new(top_node: Node<'a>) -> Self {
        let mut arena = SlotMap::with_key();
        let top_node = arena.insert(top_node);
        Self {
            arena,
            top_node,
            used_part_area: 0,
            empty_nodes_by_area: Vec::new(),
            removable_nodes: Vec::new(),
        }
    }

    fn restore_from(&mut self, snapshot: &Self) {
        self.arena.clone_from(&snapshot.arena);
        self.top_node = snapshot.top_node;
        self.used_part_area = snapshot.used_part_area;
        self.empty_nodes_by_area.clone_from(&snapshot.empty_nodes_by_area);
        self.removable_nodes.clone_from(&snapshot.removable_nodes);
    }

    fn insert_child(&mut self, node: Node<'a>, parent: NodeKey, is_empty: bool) -> NodeKey {
        if let Some(parttype) = node.parttype() {
            self.used_part_area += parttype.area();
        }

        debug_assert!(node.level() == self.arena[parent].level() + 1);

        let parent_had_children = self.arena[parent].has_children();
        let node_index = self.arena.insert(node);

        if self.arena[node_index].parttype().is_some() {
            self.register_removable(node_index);
        }

        if is_empty {
            debug_assert!(self.arena[node_index].is_empty());
            let node_area = self.arena[node_index].area();
            let result = self.empty_nodes_by_area.binary_search_by(&|key: &NodeKey| {
                self.arena[*key].area().cmp(&node_area).reverse()
            });
            let position = result.unwrap_or_else(|position| position);
            self.empty_nodes_by_area.insert(position, node_index);
        }

        let previous_sibling = self.arena[parent].last_child;
        self.arena[node_index].parent = Some(parent);
        self.arena[node_index].previous_sibling = previous_sibling;
        match previous_sibling {
            Some(previous_sibling) => self.arena[previous_sibling].next_sibling = Some(node_index),
            None => self.arena[parent].first_child = Some(node_index),
        }
        self.arena[parent].last_child = Some(node_index);
        if !parent_had_children {
            self.register_removable(parent);
        }

        debug_assert!(assertions::node_arena_valid(&self.arena, &self.top_node));
        node_index
    }

    fn remove_subtree(&mut self, node_index: NodeKey, removed_part_ids: &mut Option<Vec<usize>>) {
        if self.arena[node_index].is_empty() {
            let node_area = self.arena[node_index].area();
            let lower_index = self.empty_nodes_by_area.partition_point(|key| self.arena[*key].area() > node_area);

            if self.empty_nodes_by_area[lower_index] == node_index {
                self.empty_nodes_by_area.remove(lower_index);
            } else {
                let upper_index = self.empty_nodes_by_area.partition_point(|key| self.arena[*key].area() >= node_area);

                let mut node_found = false;
                for position in lower_index..upper_index {
                    if self.empty_nodes_by_area[position] == node_index {
                        self.empty_nodes_by_area.remove(position);
                        node_found = true;
                        break;
                    }
                }
                if !node_found {
                    panic!("Empty node not found in sorted_empty_nodes");
                }
            }
        }

        while let Some(child) = self.arena[node_index].first_child {
            self.remove_subtree(child, removed_part_ids);
        }

        self.unregister_removable(node_index);

        let node = self.arena.remove(node_index).expect("Node to be removed does not exist");
        debug_assert!(node.first_child.is_none() && node.last_child.is_none());

        if let &Some(parttype) = node.parttype() {
            if let Some(removed_parts) = removed_part_ids {
                removed_parts.push(parttype.id());
            }
            self.used_part_area -= parttype.area();
        }

        if let Some(parent) = node.parent() {
            match node.previous_sibling {
                Some(previous_sibling) => self.arena[previous_sibling].next_sibling = node.next_sibling,
                None => self.arena[parent].first_child = node.next_sibling,
            }
            match node.next_sibling {
                Some(next_sibling) => self.arena[next_sibling].previous_sibling = node.previous_sibling,
                None => self.arena[parent].last_child = node.previous_sibling,
            }
            if !self.arena[parent].has_children() {
                self.unregister_removable(parent);
            }
        }

        debug_assert!(assertions::node_arena_valid(&self.arena, &self.top_node));
    }

    fn register_removable(&mut self, node_index: NodeKey) {
        debug_assert!(self.arena[node_index].removable_position().is_none());
        debug_assert!(self.arena[node_index].parttype().is_some() || self.arena[node_index].has_children());
        let position = self.removable_nodes.len();
        self.arena[node_index].set_removable_position(Some(position));
        self.removable_nodes.push(node_index);
    }

    fn unregister_removable(&mut self, node_index: NodeKey) {
        let Some(position) = self.arena[node_index].removable_position() else {
            return;
        };
        let removed_node = self.removable_nodes.swap_remove(position);
        debug_assert_eq!(removed_node, node_index);
        self.arena[node_index].set_removable_position(None);
        if let Some(&moved_node) = self.removable_nodes.get(position) {
            self.arena[moved_node].set_removable_position(Some(position));
        }
    }

    fn debug_assert_valid(&self) {
        debug_assert!(assertions::node_arena_valid(&self.arena, &self.top_node));
        debug_assert!(assertions::cached_sorted_empty_nodes_correct(&self.arena, &self.empty_nodes_by_area), "{:#?}", self.empty_nodes_by_area.iter().map(|key| &self.arena[*key]).collect_vec());
        debug_assert!(assertions::cached_removable_nodes_correct(&self.arena, &self.removable_nodes));
    }
}

/// (P, S, x, h, big_y, p_x, p_y) of a candidate strip for a band merge
pub type MergeCand = (NodeKey, NodeKey, u64, u64, u64, u64, u64);

struct Snap<'a> {
    width: u64,
    height: u64,
    orient: Orientation,
    parttype: Option<&'a crate::core::entities::parttype::PartType>,
    children: Vec<Snap<'a>>,
}
