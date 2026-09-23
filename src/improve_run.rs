// SPDX-License-Identifier: Apache-2.0
//! `improve_placement`'s runtime: the state `DetailedMgr` moves cells in, and the moves.
//!
//! - [`Mt19937`] — `boost::mt19937`, consumed as raw `rng() % limit` (`getRandom`) and by the
//!   hand-written Fisher–Yates `Utility::random_shuffle`;
//! - pins and edges (`Network::addEdge` / `addPin`): offsets from the node CENTRE, in R0, turned
//!   by `Node::adjustCurrOrient` as the node's orientation changes;
//! - the manager's grid painting (`paintInGrid`, `eraseFromGrid`), `checkDRC`, the journal, and
//!   `tryMove` / `trySwap` with their helpers — the move machinery every optimizer calls.

use std::collections::{BTreeSet, HashMap};

use vyges_opendb::Db;

use crate::grid::Grid;
use crate::improve::{Kind, Node, Setup};

// ── the generator ────────────────────────────────────────────────────────────────────────────

/// `boost::mt19937` — the standard 32-bit Mersenne Twister.
#[derive(Clone)]
pub struct Mt19937 {
    mt: [u32; 624],
    idx: usize,
}

impl Mt19937 {
    pub fn new(seed: u32) -> Mt19937 {
        let mut mt = [0u32; 624];
        mt[0] = seed;
        for i in 1..624 {
            mt[i] = 1812433253u32.wrapping_mul(mt[i - 1] ^ (mt[i - 1] >> 30)).wrapping_add(i as u32);
        }
        Mt19937 { mt, idx: 624 }
    }

    pub fn next_u32(&mut self) -> u32 {
        if self.idx >= 624 {
            for i in 0..624 {
                let y = (self.mt[i] & 0x8000_0000) | (self.mt[(i + 1) % 624] & 0x7fff_ffff);
                let mut v = self.mt[(i + 397) % 624] ^ (y >> 1);
                if y & 1 != 0 {
                    v ^= 0x9908_b0df;
                }
                self.mt[i] = v;
            }
            self.idx = 0;
        }
        let mut y = self.mt[self.idx];
        self.idx += 1;
        y ^= y >> 11;
        y ^= (y << 7) & 0x9d2c_5680;
        y ^= (y << 15) & 0xefc6_0000;
        y ^= y >> 18;
        y
    }

    /// `DetailedMgr::getRandom(limit)` — `rng() % limit`, the draw consumed whatever `limit` is.
    pub fn get_random(&mut self, limit: usize) -> usize {
        (self.next_u32() as u64 % limit.max(1) as u64) as usize
    }

    /// `Utility::random_shuffle`: `for i in 1..n { swap(i, rng() % (i + 1)) }`.
    pub fn shuffle(&mut self, v: &mut [usize]) {
        for i in 1..v.len() {
            let r = (self.next_u32() as u64 % (i as u64 + 1)) as usize;
            v.swap(i, r);
        }
    }
}

// ── pins and edges ───────────────────────────────────────────────────────────────────────────

/// `dpl::Pin`: an offset from its node's centre.
#[derive(Debug, Clone)]
pub struct Pin {
    pub node: usize,
    pub edge: usize,
    pub ox: i32,
    pub oy: i32,
}

/// `Network::addEdge` over every non-supply net in block order: its iterms on network cells
/// (offset = master terminal bbox centre − master centre), then its placed terminals (offset 0).
pub fn build_pins(db: &Db, nodes: &[Node]) -> (Vec<Pin>, Vec<Vec<usize>>, Vec<Vec<usize>>) {
    let cell: HashMap<&str, usize> = nodes.iter().filter(|n| n.kind == Kind::Cell)
        .map(|n| (n.name.as_str(), n.id)).collect();
    let term: HashMap<&str, usize> = nodes.iter().filter(|n| n.kind == Kind::Terminal)
        .map(|n| (n.name.as_str(), n.id)).collect();
    let (mut pins, mut node_pins, mut edges) = (Vec::new(), vec![Vec::new(); nodes.len()], Vec::new());
    for net in db.block_get_nets() {
        let sig = db.net_get_sig_type(&net);
        if sig == "POWER" || sig == "GROUND" {
            continue;
        }
        let e = edges.len();
        let mut epins = Vec::new();
        for it in db.net_get_i_terms(&net) {
            let Some((inst, mterm)) = it.rsplit_once('/') else { continue };
            let master = db.inst_master(inst);
            if !crate::negotiate::is_core_auto_placeable(&db.master_get_type(&master).unwrap_or_default()) {
                continue;
            }
            let Some(&nd) = cell.get(inst) else { continue };
            // `mTerm->getBBox()` — the MASTER's, uninstanced; `xCenter` is integer `(lo+hi)/2`.
            let xx = (db.mterm_get_b_box_x_min(&master, mterm) + db.mterm_get_b_box_x_max(&master, mterm)) / 2;
            let yy = (db.mterm_get_b_box_y_min(&master, mterm) + db.mterm_get_b_box_y_max(&master, mterm)) / 2;
            let ox = xx - db.master_get_width(&master) as i32 / 2;
            let oy = yy - db.master_get_height(&master) as i32 / 2;
            node_pins[nd].push(pins.len());
            epins.push(pins.len());
            pins.push(Pin { node: nd, edge: e, ox, oy });
        }
        for bt in db.net_get_b_terms(&net) {
            if !matches!(db.bterm_get_first_pin_placement_status(&bt).as_str(), "PLACED" | "FIRM" | "LOCKED" | "COVER") {
                continue;
            }
            let Some(&nd) = term.get(bt.as_str()) else { continue };
            node_pins[nd].push(pins.len());
            epins.push(pins.len());
            pins.push(Pin { node: nd, edge: e, ox: 0, oy: 0 });
        }
        edges.push(epins);
    }
    (pins, node_pins, edges)
}

// ── the runtime ──────────────────────────────────────────────────────────────────────────────

/// One `MoveCellAction`.
#[derive(Debug, Clone)]
struct Action {
    node: usize,
    orig: (i32, i32),
    new: (i32, i32),
    orig_segs: Vec<i32>,
    new_segs: Vec<i32>,
}

/// Everything `DetailedMgr` holds beyond `ShiftLegalizer`'s structures.
pub struct Runtime {
    pub grid: Grid,
    pub pins: Vec<Pin>,
    pub node_pins: Vec<Vec<usize>>,
    pub edges: Vec<Vec<usize>>,
    /// Padding in DBU and in sites.
    pub pads: Vec<(i32, i32)>,
    pub pad_sites: Vec<(i32, i32)>,
    pub sites: Vec<String>,
    /// Master `(X, Y, R90)` symmetry.
    pub sym: Vec<(bool, bool, bool)>,
    pub used_layers: Vec<u32>,
    pub classes: Vec<crate::drc::Class>,
    pub edge_model: crate::edges::EdgeModel,
    pub power: crate::negotiate::PowerModel,
    pub seg_util: Vec<i64>,
    pub orig: Vec<(i32, i32)>,
    pub max_disp: (i32, i32),
    pub move_limit: usize,
    pub disallow_gaps: bool,
    pub rng: Mt19937,
    journal: Vec<Action>,
    affected_nodes: BTreeSet<usize>,
    affected_edges: BTreeSet<usize>,
}

impl Runtime {
    pub fn new(db: &Db, grid: Grid, nodes: &[Node], pins: (Vec<Pin>, Vec<Vec<usize>>, Vec<Vec<usize>>),
               padding: &crate::negotiate::Padding, levels: &HashMap<String, i32>) -> Runtime {
        let sw = grid.site_width;
        let mtype = |n: &Node| db.master_get_type(&n.master).unwrap_or_default();
        let pad_sites: Vec<(i32, i32)> = nodes.iter().map(|n| {
            if n.kind != Kind::Cell { (0, 0) } else { padding.of(&n.name, &n.master, &mtype(n)) }
        }).collect();
        let pads = pad_sites.iter().map(|&(l, r)| (l * sw, r * sw)).collect();
        let cells: Vec<&str> = nodes.iter().filter(|n| n.kind == Kind::Cell).map(|n| n.master.as_str()).collect();
        let edge_model = crate::edges::EdgeModel::build(db, &grid, cells.into_iter());
        let power = crate::negotiate::PowerModel::build(db, &grid, levels);
        Runtime {
            pins: pins.0, node_pins: pins.1, edges: pins.2, pads, pad_sites,
            sites: nodes.iter().map(|n| if n.kind == Kind::Cell { db.master_get_site(&n.master) } else { String::new() }).collect(),
            sym: nodes.iter().map(|n| if n.kind == Kind::Cell {
                (db.master_get_symmetry_x(&n.master), db.master_get_symmetry_y(&n.master), db.master_get_symmetry_r90(&n.master))
            } else { (false, false, false) }).collect(),
            used_layers: nodes.iter().map(|n| if n.kind != Kind::Cell { 0 } else {
                crate::drc::used_layers(db.master_pin_boxes(&n.master).unwrap_or_default().into_iter()
                    .filter_map(|(ln, ..)| levels.get(&db.layer_name_by_number(ln)).copied()))
            }).collect(),
            classes: nodes.iter().map(|n| crate::drc::classify(&mtype(n))).collect(),
            edge_model, power, seg_util: Vec::new(),
            orig: nodes.iter().map(|n| (n.left, n.bottom)).collect(),
            max_disp: (0, 0), move_limit: 100,
            disallow_gaps: !db.has_one_site_master(),
            rng: Mt19937::new(1),
            journal: Vec::new(), affected_nodes: BTreeSet::new(), affected_edges: BTreeSet::new(),
            grid,
        }
    }
}

/// `Node::adjustCurrOrient` — change the orientation keeping the lower-left, turning the pin
/// offsets (a quarter turn when switching between the R0 and R90 families, then a mirror in X
/// and/or Y) and swapping width/height across a quarter turn.
pub fn adjust_curr_orient(node: &mut Node, pins: &mut [Pin], node_pins: &[usize], new: &str) {
    let rot = |o: &str| matches!(o, "R90" | "MXR90" | "R270" | "MYR90");
    let mut cur = node.orient.clone();
    if new == cur {
        return;
    }
    if rot(&cur) {
        if !rot(new) {
            for &p in node_pins {
                let (dx, dy) = (pins[p].ox, pins[p].oy);
                pins[p].ox = -dy;
                pins[p].oy = dx;
            }
            std::mem::swap(&mut node.width, &mut node.height);
            cur = match cur.as_str() { "R90" => "R0", "MXR90" => "MX", "MYR90" => "MY", _ => "R180" }.into();
        }
    } else if rot(new) {
        for &p in node_pins {
            let (dx, dy) = (pins[p].ox, pins[p].oy);
            pins[p].ox = dy;
            pins[p].oy = -dx;
        }
        std::mem::swap(&mut node.width, &mut node.height);
        cur = match cur.as_str() { "R0" => "R90", "MX" => "MXR90", "MY" => "MYR90", _ => "R270" }.into();
    }
    let (mut mx, mut my) = (1, 1);
    if rot(&cur) {
        let t = |o: &str| matches!(o, "R90" | "MYR90");
        if t(&cur) != t(new) { mx = -1; }
        let t = |o: &str| matches!(o, "R90" | "MXR90");
        if t(&cur) != t(new) { my = -1; }
    } else {
        let t = |o: &str| matches!(o, "R0" | "MX");
        if t(&cur) != t(new) { mx = -1; }
        let t = |o: &str| matches!(o, "R0" | "MY");
        if t(&cur) != t(new) { my = -1; }
    }
    for &p in node_pins {
        pins[p].ox *= mx;
        pins[p].oy *= my;
    }
    node.orient = new.to_string();
}

impl Setup {
    fn sw(&self) -> i32 {
        self.rt.grid.site_width
    }

    /// `Node::adjustCurrOrient` on node `nd`.
    pub(crate) fn adjust_orient(&mut self, nd: usize, new: &str) {
        let pins = self.rt.node_pins[nd].clone();
        adjust_curr_orient(&mut self.nodes[nd], &mut self.rt.pins, &pins, new);
    }

    /// `gridWidth(cell)` — `divCeil(width, site width)`.
    fn grid_width(&self, nd: usize) -> i64 {
        let sw = self.sw();
        ((self.nodes[nd].width + sw - 1) / sw) as i64
    }

    /// `Grid::paintPixel(node, gridX(node), y)` with the node's padding.
    pub(crate) fn paint_at(&mut self, nd: usize, gy: i64) {
        let gx = (self.nodes[nd].left / self.sw()) as i64;
        let (w, h) = (self.grid_width(nd), self.nodes[nd].height);
        let (l, r) = self.rt.pad_sites[nd];
        self.rt.grid.paint_pixel(gx, gy, w, h, l as i64, r as i64, nd as u32);
    }

    /// `DetailedMgr::paintInGrid` — at `gridX`, `gridSnapDownY`, taking the row's site orientation.
    pub(crate) fn paint_in_grid(&mut self, nd: usize) {
        let gx = (self.nodes[nd].left / self.sw()) as i64;
        let Some(gy) = self.rt.grid.grid_snap_down_y(self.nodes[nd].bottom) else { return };
        let orient = self.rt.grid.site_orient_at(gx, gy as i64, &self.rt.sites[nd]);
        self.paint_at(nd, gy as i64);
        if let Some(o) = orient {
            self.adjust_orient(nd, &o);
        }
    }

    /// The journal's `paintInGrid` — the same, at `gridRoundY`.
    fn paint_in_grid_round(&mut self, nd: usize) {
        let gx = (self.nodes[nd].left / self.sw()) as i64;
        let gy = self.rt.grid.grid_round_y(self.nodes[nd].bottom) as i64;
        let orient = self.rt.grid.site_orient_at(gx, gy, &self.rt.sites[nd]);
        self.paint_at(nd, gy);
        if let Some(o) = orient {
            self.adjust_orient(nd, &o);
        }
    }

    /// `Grid::erasePixel` over the node's padded covering.
    fn erase_from_grid(&mut self, nd: usize) {
        let n = &self.nodes[nd];
        let (x0, y0, x1, y1) = self.rt.grid.covering(n.left, n.bottom, n.width, n.height);
        let (l, r) = self.rt.pad_sites[nd];
        self.rt.grid.erase_pixel(x0 - l as i64, y0, x1 + r as i64, y1, nd as u32);
    }

    /// `setFixedGridCells` — every fixed INSTANCE node, body and padding.
    pub(crate) fn paint_fixed(&mut self) {
        for nd in 0..self.nodes.len() {
            if self.nodes[nd].kind == Kind::Cell && self.nodes[nd].fixed {
                if let Some(gy) = self.rt.grid.grid_snap_down_y(self.nodes[nd].bottom) {
                    self.paint_at(nd, gy as i64);
                }
            }
        }
    }

    /// `PlacementDRC::checkDRC(node)` — at `gridX`, `gridRoundY`, the node's orientation: blocked
    /// layers, the one-site gap, padding, edge spacing, in that order.
    pub(crate) fn has_placement_violation(&self, nd: usize) -> bool {
        let n = &self.nodes[nd];
        let g = &self.rt.grid;
        let sw = self.sw();
        let x = n.left / sw;
        let y = g.grid_round_y(n.bottom) as i64;
        let x_end = x + self.grid_width(nd) as i32;
        let y_end = g.grid_end_y(y, n.height) as i32;
        let y = y as i32;
        let blocked = crate::drc::check_blocked_layers(x, x_end, y, y_end, self.rt.used_layers[nd],
            &|px, py| g.pixel(px as i64, py as i64).map(|p| p.blocked_layers));
        if !blocked {
            return true;
        }
        let gap = crate::drc::check_one_site_gap(self.rt.disallow_gaps, x, x_end, y, y_end,
            crate::drc::EdgeReading::OffGridIsOccupied,
            &|px, py| g.pixel(px as i64, py as i64).map(|p| p.cell.is_some()));
        if !gap {
            return true;
        }
        let me = nd as u32;
        let classes = &self.rt.classes;
        let at = |px: i32, py: i32| {
            let p = g.pixel(px as i64, py as i64)?;
            Some((p.cell.map(|o| (classes[o as usize], o == me)),
                  p.padding_reserved_by.map(|o| (classes[o as usize], o == me))))
        };
        let (pl, pr) = self.rt.pad_sites[nd];
        if !crate::drc::check_padding(x, x_end, y, y_end, pl, pr, classes[nd], &at) {
            return true;
        }
        let em = &self.rt.edge_model;
        let row_y = g.row_y.get(y as usize).copied().unwrap_or(0);
        !em.check(g, nd, &n.master, &n.orient, x * sw, row_y,
            &|px, py| g.pixel(px as i64, py as i64).and_then(|p| p.cell).map(|o| o as usize),
            &|o| em.edges_at(&self.nodes[o].master, &self.nodes[o].orient,
                             self.nodes[o].left, self.nodes[o].bottom))
    }

    /// `Architecture::getCellSpacing(left, right)` — padding right of `left` plus left of `right`.
    pub(crate) fn cell_spacing(&self, l: Option<usize>, r: Option<usize>) -> i32 {
        l.map_or(0, |i| self.rt.pads[i].1) + r.map_or(0, |i| self.rt.pads[i].0)
    }

    /// `removeCellFromSegment`.
    fn remove_cell_from_segment(&mut self, nd: usize, seg: usize) {
        if let Some(p) = self.cells_in_seg[seg].iter().position(|&c| c == nd) {
            self.cells_in_seg[seg].remove(p);
        }
        if let Some(p) = self.cell_segs[nd].iter().position(|&s| s == seg) {
            self.cell_segs[nd].remove(p);
        }
        self.rt.seg_util[seg] -= self.nodes[nd].width as i64;
    }

    /// `addCellToSegment` with the utilization it also keeps.
    pub(crate) fn add_cell_to_segment_util(&mut self, nd: usize, seg: usize) {
        self.add_cell_to_segment(nd, seg);
        self.rt.seg_util[seg] += self.nodes[nd].width as i64;
    }

    /// `resortSegment` — a STABLE sort by centre, utilization recomputed.
    pub(crate) fn resort_segments(&mut self) {
        for s in 0..self.segments.len() {
            let nodes = &self.nodes;
            self.cells_in_seg[s].sort_by_key(|&c| nodes[c].center_x());
            self.rt.seg_util[s] = self.cells_in_seg[s].iter().map(|&c| self.nodes[c].width as i64).sum();
        }
    }

    /// `alignPos` — the site-aligned left edge nearest `xi` with the cell inside `[xl, xr]`, using
    /// ROW 0's origin and spacing, whatever the row.
    fn align_pos(&self, nd: usize, xi: i32, xl: i32, xr: i32) -> Option<i32> {
        let (origin, sp) = (self.arch.rows[0].left, self.arch.rows[0].site_spacing);
        let xr = xr - self.nodes[nd].width;
        let mut xp = xl.max(xr.min(xi));
        let ix = (xp - origin) / sp;
        xp = origin + ix * sp;
        if xp < xl {
            xp += sp;
        } else if xp > xr {
            xp -= sp;
        }
        if xp < xl || xp > xr { None } else { Some(xp) }
    }

    /// [`Setup::align_pos`] for the optimizers.
    pub(crate) fn align_pos_pub(&self, nd: usize, xi: i32, xl: i32, xr: i32) -> Option<i32> {
        self.align_pos(nd, xi, xl, xr)
    }

    /// `DetailedMgr::eraseFromGrid`.
    pub(crate) fn erase_cell(&mut self, nd: usize) {
        self.erase_from_grid(nd);
    }

    /// `checkSiteOrientation` — the row at `gridSnapDownY(y)` offers the cell's site at `gridX(x)`.
    fn check_site_orientation(&self, nd: usize, x: i32, y: i32) -> bool {
        let g = &self.rt.grid;
        let Some(gy) = g.grid_snap_down_y(y) else { return false };
        g.site_orient_at((x / self.sw()) as i64, gy as i64, &self.rt.sites[nd]).is_some()
    }

    fn clear_move_list(&mut self) {
        self.rt.journal.clear();
        self.rt.affected_nodes.clear();
        self.rt.affected_edges.clear();
    }

    /// `addToMoveList` — commits the move at once (grid, segments, position) and journals it.
    /// ⚠️ The displacement limit is checked only in the single-segment form, as upstream.
    fn add_to_move_list(&mut self, nd: usize, cur: (i32, i32), cur_segs: Vec<i32>, new: (i32, i32),
                        new_segs: Vec<i32>, check_disp: bool) -> bool {
        if self.rt.journal.len() >= self.rt.move_limit {
            return false;
        }
        if !self.check_site_orientation(nd, new.0, new.1) {
            return false;
        }
        if check_disp {
            let (ox, oy) = self.rt.orig[nd];
            if (new.0 - ox).abs() > self.rt.max_disp.0 || (new.1 - oy).abs() > self.rt.max_disp.1 {
                return false;
            }
        }
        self.erase_from_grid(nd);
        for &s in &cur_segs {
            if s >= 0 {
                self.remove_cell_from_segment(nd, s as usize);
            }
        }
        self.nodes[nd].left = new.0;
        self.nodes[nd].bottom = new.1;
        self.paint_in_grid(nd);
        for &s in &new_segs {
            if s >= 0 {
                self.add_cell_to_segment_util(nd, s as usize);
            }
        }
        self.rt.affected_nodes.insert(nd);
        for &p in &self.rt.node_pins[nd] {
            self.rt.affected_edges.insert(self.rt.pins[p].edge);
        }
        self.rt.journal.push(Action { node: nd, orig: cur, new, orig_segs: cur_segs, new_segs });
        true
    }

    fn add_move(&mut self, nd: usize, cur: (i32, i32), cur_seg: i32, new: (i32, i32), new_seg: i32) -> bool {
        self.add_to_move_list(nd, cur, vec![cur_seg], new, vec![new_seg], true)
    }

    /// `Journal::undo` — the actions in reverse: erase, out of the new segments, back to the old
    /// position, repainted at `gridRoundY`, into the old segments.
    fn undo(&mut self) {
        let actions = std::mem::take(&mut self.rt.journal);
        for a in actions.iter().rev() {
            self.erase_from_grid(a.node);
            for &s in &a.new_segs {
                if s >= 0 {
                    self.remove_cell_from_segment(a.node, s as usize);
                }
            }
            self.nodes[a.node].left = a.orig.0;
            self.nodes[a.node].bottom = a.orig.1;
            self.paint_in_grid_round(a.node);
            for &s in &a.orig_segs {
                if s >= 0 {
                    self.add_cell_to_segment_util(a.node, s as usize);
                }
            }
        }
        self.rt.journal = actions;
    }

    pub(crate) fn accept_move(&mut self) {
        self.clear_move_list();
    }

    pub(crate) fn reject_move(&mut self) {
        self.undo();
        self.clear_move_list();
    }

    pub(crate) fn affected_edges(&self) -> Vec<usize> {
        self.rt.affected_edges.iter().copied().collect()
    }

    /// `verifyMove` — every affected node DRC-clean, else the move is rejected.
    fn verify_move(&mut self) -> bool {
        let nodes: Vec<usize> = self.rt.affected_nodes.iter().copied().collect();
        for nd in nodes {
            if self.has_placement_violation(nd) {
                self.reject_move();
                return false;
            }
        }
        true
    }

    /// `lower_bound(cellsInSeg[s], x, compareNodesX)`.
    fn lower_bound_x(&self, seg: usize, x: i32) -> usize {
        let nodes = &self.nodes;
        self.cells_in_seg[seg].partition_point(|&c| nodes[c].center_x() < x)
    }

    /// `tryMove`.
    pub(crate) fn try_move(&mut self, nd: usize, xi: i32, yi: i32, si: usize, xj: i32, yj: i32, sj: usize) -> bool {
        if !self.master_symmetry_ok(self.rt.sym[nd].0, self.segments[sj].row) {
            self.reject_move();
            return false;
        }
        let ok = if self.arch.height_in_rows(&self.nodes[nd]) == 1 {
            if si != sj { self.try_move1(nd, xi, yi, si, xj, yj, sj) } else { self.try_move2(nd, si, xj, yj, sj) }
        } else {
            self.try_move3(nd, xj, sj)
        };
        if ok {
            return self.verify_move();
        }
        self.reject_move();
        false
    }

    /// `trySwap`.
    pub(crate) fn try_swap(&mut self, nd: usize, xi: i32, yi: i32, si: usize, xj: i32, yj: i32, sj: usize) -> bool {
        if !self.master_symmetry_ok(self.rt.sym[nd].0, self.segments[sj].row) {
            self.reject_move();
            return false;
        }
        if self.try_swap1(nd, xi, yi, si, xj, yj, sj) {
            return self.verify_move();
        }
        self.reject_move();
        false
    }

    /// `tryMove1` — a single-height cell into ANOTHER segment, pushing neighbours aside.
    fn try_move1(&mut self, nd: usize, _xi: i32, _yi: i32, si: usize, xj: i32, yj: i32, sj: usize) -> bool {
        self.clear_move_list();
        if sj == si || self.nodes[nd].group != self.segments[sj].reg || self.arch.height_in_rows(&self.nodes[nd]) != 1 {
            return false;
        }
        let rj = self.segments[sj].row;
        let yj = if yj != self.arch.rows[rj].bottom { self.arch.rows[rj].bottom } else { yj };
        let (mut ndl, mut ndr) = (None, None);
        if !self.cells_in_seg[sj].is_empty() {
            let it = self.lower_bound_x(sj, xj);
            if it == self.cells_in_seg[sj].len() {
                ndl = self.cells_in_seg[sj].last().copied();
            } else {
                ndr = Some(self.cells_in_seg[sj][it]);
                if it > 0 {
                    ndl = Some(self.cells_in_seg[sj][it - 1]);
                }
            }
        }
        let seg_w = (self.segments[sj].max_x - self.segments[sj].min_x) as i64;
        let util = self.rt.seg_util[sj];
        let w = self.nodes[nd].width;
        let cur = (self.nodes[nd].left, self.nodes[nd].bottom);
        let (lx, rx, required) = match (ndl, ndr) {
            (None, None) => (self.segments[sj].min_x + self.cell_spacing(None, Some(nd)),
                             self.segments[sj].max_x - self.cell_spacing(Some(nd), None),
                             w + self.cell_spacing(None, Some(nd)) + self.cell_spacing(Some(nd), None)),
            (Some(l), None) => (self.nodes[l].right() + self.cell_spacing(Some(l), Some(nd)),
                                self.segments[sj].max_x - self.cell_spacing(Some(nd), None),
                                w + self.cell_spacing(Some(l), Some(nd)) + self.cell_spacing(Some(nd), None)),
            (None, Some(r)) => (self.segments[sj].min_x + self.cell_spacing(None, Some(nd)),
                                self.nodes[r].left - self.cell_spacing(Some(nd), Some(r)),
                                w + self.cell_spacing(None, Some(nd)) + self.cell_spacing(Some(nd), Some(r))),
            (Some(l), Some(r)) => (self.nodes[l].right() + self.cell_spacing(Some(l), Some(nd)),
                                   self.nodes[r].left - self.cell_spacing(Some(nd), Some(r)),
                                   w + self.cell_spacing(Some(l), Some(nd)) + self.cell_spacing(Some(nd), Some(r))
                                     - self.cell_spacing(Some(l), Some(r))),
        };
        if required as i64 + util > seg_w {
            return false;
        }
        let Some(xj) = self.align_pos(nd, xj, lx, rx) else { return false };
        if !self.add_move(nd, cur, si as i32, (xj, yj), sj as i32) {
            return false;
        }
        match (ndl, ndr) {
            (None, None) => true,
            (Some(l), None) => self.shift_left_helper(nd, xj, sj, l),
            (None, Some(r)) => self.shift_right_helper(nd, xj, sj, r),
            (Some(l), Some(r)) => self.shift_right_helper(nd, xj, sj, r) && self.shift_left_helper(nd, xj, sj, l),
        }
    }

    /// `tryMove2` — within the SAME segment, into the gap left or right of the cell nearest `xj`.
    fn try_move2(&mut self, nd: usize, si: usize, xj: i32, yj: i32, sj: usize) -> bool {
        self.clear_move_list();
        if sj != si || self.nodes[nd].group != self.segments[sj].reg || self.arch.height_in_rows(&self.nodes[nd]) != 1 {
            return false;
        }
        let rj = self.segments[sj].row;
        let yj = if yj != self.arch.rows[rj].bottom { self.arch.rows[rj].bottom } else { yj };
        let n = self.cells_in_seg[si].len() as i64 - 1;
        if self.cells_in_seg[sj].is_empty() {
            return false;
        }
        let it = self.lower_bound_x(sj, xj);
        let ix_j = if it == self.cells_in_seg[sj].len() { it - 1 } else { it };
        let ndj = self.cells_in_seg[sj][ix_j];
        let prev = if ix_j == 0 { None } else { Some(self.cells_in_seg[sj][ix_j - 1]) };
        let next = if ix_j as i64 == n { None } else { self.cells_in_seg[sj].get(ix_j + 1).copied() };
        let cur = (self.nodes[nd].left, self.nodes[nd].bottom);
        let w = self.nodes[nd].width;
        let lx = match prev {
            Some(p) => self.nodes[p].right() + self.cell_spacing(Some(p), Some(nd)),
            None => self.segments[sj].min_x + self.cell_spacing(None, Some(nd)),
        };
        let rx = self.nodes[ndj].left - self.cell_spacing(Some(nd), Some(ndj));
        if w <= rx - lx {
            let Some(xj) = self.align_pos(nd, xj, lx, rx) else { return false };
            return self.add_move(nd, cur, si as i32, (xj, yj), sj as i32);
        }
        let lx = self.nodes[ndj].right() + self.cell_spacing(Some(ndj), Some(nd));
        let rx = match next {
            Some(q) => self.nodes[q].left - self.cell_spacing(Some(nd), Some(q)),
            None => self.segments[sj].max_x - self.cell_spacing(Some(nd), None),
        };
        if w <= rx - lx {
            let Some(xj) = self.align_pos(nd, xj, lx, rx) else { return false };
            return self.add_move(nd, cur, si as i32, (xj, yj), sj as i32);
        }
        false
    }

    /// `tryMove3` — a multi-height cell into a gap spanning its rows.
    fn try_move3(&mut self, nd: usize, xj: i32, sj: usize) -> bool {
        self.clear_move_list();
        let spanned = self.arch.height_in_rows(&self.nodes[nd]);
        if spanned <= 1 || spanned as usize != self.cell_segs[nd].len() {
            return false;
        }
        let nrows = self.arch.rows.len() as i64;
        let mut rb = self.segments[sj].row as i64;
        while rb + spanned as i64 >= nrows {
            rb -= 1;
        }
        if rb < 0 {
            return false;
        }
        let rt = rb + spanned as i64 - 1;
        // `Architecture::powerCompatible(ndi, row rb)`.
        let a = &self.arch;
        let rails = |i: usize| self.rt.grid.grid_y(a.rows.get(i).map_or(i32::MIN, |r| r.bottom))
            .map_or((crate::drc::Power::Unknown, crate::drc::Power::Unknown), |g| self.rt.power.row_rails(g));
        let (top, bot) = self.rt.power.master_rails(&self.nodes[nd].master);
        let span = (self.nodes[nd].height as f64 / a.rows[rb as usize].height as f64).round() as usize;
        let (ok, _) = crate::drc::power_compatible(bot, top, rb as usize, span, a.rows.len(),
                                                   &|i| rails(i).0, &|i| rails(i).1);
        if !ok {
            return false;
        }
        let mut segs = Vec::new();
        for r in rb..=rt {
            let got = self.segs_in_row[r as usize].iter().copied().find(|&s| {
                let seg = &self.segments[s];
                seg.reg == self.nodes[nd].group && xj >= seg.min_x && xj <= seg.max_x
            });
            match got {
                Some(s) => segs.push(s),
                None => break,
            }
        }
        if segs.len() != spanned as usize {
            return false;
        }
        let (mut xmin, mut xmax) = (i32::MIN, i32::MAX);
        for &s in &segs {
            let cells = &self.cells_in_seg[s];
            let (mut left, mut rite) = (None, None);
            if !cells.is_empty() {
                let it = self.lower_bound_x(s, xj);
                if it == cells.len() {
                    // ⛔ Upstream steps `it_j` back from `end()` — onto the LAST cell, which is
                    // `left` already — so a cell that is itself last stays `left` and the move is
                    // refused below. Transcribed, not repaired.
                    left = cells.last().copied();
                    if left == Some(nd) {
                        left = if it != 0 { Some(cells[it - 1]) } else { None };
                    }
                } else {
                    rite = Some(cells[it]);
                    if it > 0 {
                        left = Some(cells[it - 1]);
                        if left == Some(nd) {
                            left = if it >= 2 { Some(cells[it - 2]) } else { None };
                        }
                    }
                }
            }
            if left == Some(nd) || rite == Some(nd) {
                return false;
            }
            let seg = &self.segments[s];
            let mut lx = left.map_or(seg.min_x, |l| self.nodes[l].right());
            let mut rx = rite.map_or(seg.max_x, |r| self.nodes[r].left);
            if let Some(l) = left {
                lx += self.cell_spacing(Some(l), Some(nd));
            }
            if let Some(r) = rite {
                rx -= self.cell_spacing(Some(nd), Some(r));
            }
            if self.nodes[nd].width <= rx - lx {
                xmin = xmin.max(lx);
                xmax = xmax.min(rx);
            } else {
                return false;
            }
        }
        if self.nodes[nd].width <= xmax - xmin {
            let Some(xj) = self.align_pos(nd, xj, xmin, xmax) else { return false };
            let old: Vec<i32> = self.cell_segs[nd].iter().map(|&s| s as i32).collect();
            let cur = (self.nodes[nd].left, self.nodes[nd].bottom);
            let yb = self.arch.rows[rb as usize].bottom;
            return self.add_to_move_list(nd, cur, old, (xj, yb), segs.iter().map(|&s| s as i32).collect(), false);
        }
        false
    }

    /// `shiftRightHelper`. ⛔ Upstream computes the site as `(xj - originX / siteSpacing)` —
    /// DIVISION FIRST — and the "aligned" position from it; transcribed as written.
    fn shift_right_helper(&mut self, ndi: usize, mut xj: i32, sj: usize, ndr: usize) -> bool {
        let Some(mut ix) = self.cells_in_seg[sj].iter().position(|&c| c == ndr) else { return false };
        let n = self.cells_in_seg[sj].len() - 1;
        let row = &self.arch.rows[self.segments[sj].row];
        let (origin, sp) = (row.left, row.site_spacing);
        let (mut ndi, mut ndr) = (ndi, ndr);
        while ix <= n && self.nodes[ndr].left < xj + self.nodes[ndi].width + self.cell_spacing(Some(ndi), Some(ndr)) {
            if self.arch.height_in_rows(&self.nodes[ndr]) != 1 {
                return false;
            }
            xj += self.nodes[ndi].width;
            xj += self.cell_spacing(Some(ndi), Some(ndr));
            let site = xj - origin / sp;
            let mut sx = origin.wrapping_add(site.wrapping_mul(sp));
            if xj != sx {
                if xj > sx {
                    sx = sx.wrapping_add(sp);
                }
                if xj != sx && xj < sx {
                    xj = sx;
                }
            }
            let cur = (self.nodes[ndr].left, self.nodes[ndr].bottom);
            if !self.add_move(ndr, cur, sj as i32, (xj, cur.1), sj as i32) {
                return false;
            }
            if xj + self.nodes[ndr].width + self.cell_spacing(Some(ndr), None) > self.segments[sj].max_x {
                return false;
            }
            if ix == n {
                break;
            }
            ndi = ndr;
            ix += 1;
            ndr = self.cells_in_seg[sj][ix];
        }
        true
    }

    /// `shiftLeftHelper`, with the same division-first site computation.
    fn shift_left_helper(&mut self, ndi: usize, mut xj: i32, sj: usize, ndl: usize) -> bool {
        let Some(mut ix) = self.cells_in_seg[sj].iter().position(|&c| c == ndl) else { return false };
        let row = &self.arch.rows[self.segments[sj].row];
        let (origin, sp) = (row.left, row.site_spacing);
        let (mut ndi, mut ndl) = (ndi, ndl);
        while self.nodes[ndl].right() + self.cell_spacing(Some(ndl), Some(ndi)) > xj {
            if self.arch.height_in_rows(&self.nodes[ndl]) != 1 {
                return false;
            }
            xj -= self.cell_spacing(Some(ndl), Some(ndi));
            xj -= self.nodes[ndl].width;
            let site = xj - origin / sp;
            let sx = origin.wrapping_add(site.wrapping_mul(sp));
            if xj != sx && xj > sx {
                xj = sx;
            }
            let cur = (self.nodes[ndl].left, self.nodes[ndl].bottom);
            if !self.add_move(ndl, cur, sj as i32, (xj, cur.1), sj as i32) {
                return false;
            }
            if xj - self.cell_spacing(None, Some(ndl)) < self.segments[sj].min_x {
                return false;
            }
            if ix == 0 {
                break;
            }
            ndi = ndl;
            ix -= 1;
            ndl = self.cells_in_seg[sj][ix];
        }
        true
    }

    /// `DetailedMgr::shift` — site-aligned positions for an ordered run of single-height cells in
    /// `[left, right]`, minimising displacement from `targets` by dynamic programming.
    fn shift_dp(&self, cells: &[usize], targets: &[i32], left: i32, right: i32, row: usize) -> Option<Vec<i32>> {
        let r = &self.arch.rows[row];
        let (origin, sp, sw) = (r.left, r.site_spacing, r.site_width);
        let n = cells.len();
        let mut i0 = (left - origin) / sp;
        if origin + i0 * sp < left {
            i0 += 1;
        }
        let mut i1 = (right - origin) / sp;
        if origin + i1 * sp + sw >= right {
            i1 -= 1;
        }
        let nsites = i1 - i0 + 1;
        let mut swid = vec![0i32; n];
        let mut rsites = 0;
        for i in 0..n {
            let mut w = self.nodes[cells[i]].width;
            if i != n - 1 {
                w += self.cell_spacing(Some(cells[i]), Some(cells[i + 1]));
            }
            swid[i] = w / sp;
            rsites += swid[i];
        }
        if rsites > nsites || nsites < 0 {
            return None;
        }
        let (mut site_l, mut site_r) = (vec![0; n], vec![0; n]);
        let mut k = i0;
        for i in 0..n {
            site_l[i] = k;
            k += swid[i];
        }
        k = i1 + 1;
        for i in (0..n).rev() {
            site_r[i] = k - swid[i];
            k = site_r[i];
            if site_r[i] < site_l[i] {
                return None;
            }
        }
        let ns = nsites as usize;
        let mut tcost = vec![vec![f64::MAX; n + 1]; ns + 1];
        let mut prev = vec![vec![(-1i64, -1i64); n + 1]; ns + 1];
        let mut cost = vec![vec![0.0f64; n + 1]; ns + 1];
        for j in 1..=n {
            for i in 1..=ns {
                let site = i0 + i as i32 - 1;
                if site < site_l[j - 1] || site > site_r[j - 1] {
                    continue;
                }
                cost[i][j] = ((origin + site * sp) - targets[j - 1]).abs() as f64;
            }
        }
        tcost[0][0] = 0.0;
        for j in 1..=n {
            let prev_wid = if j - 1 == 0 { 1 } else { swid[j - 2] } as i64;
            let curr_wid = swid[j - 1];
            for i in 1..=ns {
                let site = i0 + i as i32 - 1;
                let c = tcost[i - 1][j];
                if c < tcost[i][j] {
                    tcost[i][j] = c;
                    prev[i][j] = ((i - 1) as i64, j as i64);
                }
                let ii = i as i64 - prev_wid;
                if ii >= 0 && site + curr_wid - 1 <= i1 {
                    let c = tcost[ii as usize][j - 1] + cost[i][j];
                    if c < tcost[i][j] {
                        tcost[i][j] = c;
                        prev[i][j] = (ii, (j - 1) as i64);
                    }
                }
            }
        }
        let mut ok = false;
        let mut cur = (ns as i64, n as i64);
        while cur.0 != -1 && cur.1 != -1 {
            if cur == (0, 0) {
                ok = true;
            }
            cur = prev[cur.0 as usize][cur.1 as usize];
        }
        if !ok {
            return None;
        }
        let mut pos = vec![0i32; n];
        let mut cur = (ns as i64, n as i64);
        while cur.0 != -1 && cur.1 != -1 {
            if cur == (0, 0) {
                break;
            }
            let (ci, cj) = (cur.0 as usize, cur.1 as usize);
            if cj as i64 != prev[ci][cj].1 {
                pos[cj - 1] = origin + (i0 + ci as i32 - 1) * sp;
            }
            cur = prev[ci][cj];
        }
        Some(pos)
    }

    /// `trySwap1` — swap `ndi` with the cell nearest `xj` in segment `sj`; adjacent cells are
    /// re-placed together by [`Setup::shift_dp`].
    fn try_swap1(&mut self, ndi: usize, xi: i32, _yi: i32, si: usize, xj: i32, _yj: i32, sj: usize) -> bool {
        self.clear_move_list();
        if self.cells_in_seg[sj].is_empty() {
            return false;
        }
        let it = self.lower_bound_x(sj, xj);
        let ndj = if it == self.cells_in_seg[sj].len() { *self.cells_in_seg[sj].last().unwrap() } else { self.cells_in_seg[sj][it] };
        if ndj == ndi {
            return false;
        }
        if !self.master_symmetry_ok(self.rt.sym[ndj].0, self.segments[si].row) {
            return false;
        }
        if self.arch.height_in_rows(&self.nodes[ndi]) != 1 || self.arch.height_in_rows(&self.nodes[ndj]) != 1 {
            return false;
        }
        let ix_i = self.cells_in_seg[si].iter().position(|&c| c == ndi).unwrap_or(usize::MAX);
        let ix_j = self.cells_in_seg[sj].iter().position(|&c| c == ndj).unwrap_or(usize::MAX);
        let adjacent = si == sj && (ix_i.wrapping_add(1) == ix_j || ix_j.wrapping_add(1) == ix_i);
        let (mut xi, mut xj) = (xi, xj);
        if !adjacent {
            let n = self.cells_in_seg[si].len() - 1;
            let next = if ix_i == n { None } else { Some(self.cells_in_seg[si][ix_i + 1]) };
            let prev = if ix_i == 0 { None } else { Some(self.cells_in_seg[si][ix_i - 1]) };
            let rx = next.map_or(self.segments[si].max_x, |q| self.nodes[q].left) - self.cell_spacing(Some(ndj), next);
            let lx = prev.map_or(self.segments[si].min_x, |p| self.nodes[p].right()) + self.cell_spacing(prev, Some(ndj));
            if self.nodes[ndj].width > rx - lx {
                return false;
            }
            let Some(a) = self.align_pos(ndj, xi, lx, rx) else { return false };
            xi = a;
            let n = self.cells_in_seg[sj].len() - 1;
            let next = if ix_j == n { None } else { Some(self.cells_in_seg[sj][ix_j + 1]) };
            let prev = if ix_j == 0 { None } else { Some(self.cells_in_seg[sj][ix_j - 1]) };
            let rx = next.map_or(self.segments[sj].max_x, |q| self.nodes[q].left) - self.cell_spacing(Some(ndi), next);
            let lx = prev.map_or(self.segments[sj].min_x, |p| self.nodes[p].right()) + self.cell_spacing(prev, Some(ndi));
            if self.nodes[ndi].width > rx - lx {
                return false;
            }
            let Some(b) = self.align_pos(ndi, xj, lx, rx) else { return false };
            xj = b;
        } else if ix_i + 1 == ix_j {
            let n = self.cells_in_seg[sj].len() - 1;
            let next = if ix_j == n { None } else { Some(self.cells_in_seg[sj][ix_j + 1]) };
            let prev = if ix_i == 0 { None } else { Some(self.cells_in_seg[si][ix_i - 1]) };
            let rx = next.map_or(self.segments[sj].max_x, |q| self.nodes[q].left) - self.cell_spacing(Some(ndi), next);
            let lx = prev.map_or(self.segments[si].min_x, |p| self.nodes[p].right()) + self.cell_spacing(prev, Some(ndj));
            if self.nodes[ndj].width + self.nodes[ndi].width + self.cell_spacing(Some(ndj), Some(ndi)) > rx - lx {
                return false;
            }
            let row = self.segments[si].row;
            let Some(p) = self.shift_dp(&[ndj, ndi], &[xi, xj], lx, rx, row) else { return false };
            xi = p[0];
            xj = p[1];
        } else {
            let n = self.cells_in_seg[si].len() - 1;
            let next = if ix_i == n { None } else { Some(self.cells_in_seg[si][ix_i + 1]) };
            let prev = if ix_j == 0 { None } else { Some(self.cells_in_seg[sj][ix_j - 1]) };
            let rx = next.map_or(self.segments[si].max_x, |q| self.nodes[q].left) - self.cell_spacing(Some(ndj), next);
            let lx = prev.map_or(self.segments[sj].min_x, |p| self.nodes[p].right()) + self.cell_spacing(prev, Some(ndi));
            if self.nodes[ndi].width + self.nodes[ndj].width + self.cell_spacing(Some(ndi), Some(ndj)) > rx - lx {
                return false;
            }
            let row = self.segments[si].row;
            let Some(p) = self.shift_dp(&[ndi, ndj], &[xj, xi], lx, rx, row) else { return false };
            xj = p[0];
            xi = p[1];
        }
        let (x1, y1) = (self.nodes[ndi].left, self.nodes[ndi].bottom);
        let (x2, y2) = (self.nodes[ndj].left, self.nodes[ndj].bottom);
        if !self.add_move(ndi, (x1, y1), si as i32, (xj, y2), sj as i32) {
            return false;
        }
        if !self.add_move(ndj, (x2, y2), sj as i32, (xi, y1), si as i32) {
            // Upstream repaints `ndj` here and returns; the caller's reject undoes `ndi`.
            self.paint_in_grid(ndj);
            return false;
        }
        true
    }

    /// Correlation aid: place every movable cell as a `VYGS|stage` record says — out of its
    /// segments, moved, re-oriented, into the segment of its row containing it (single height)
    /// or the stacked run of them (multi height), in `addCellToSegment` order.
    pub(crate) fn inject_cells(&mut self, record: &str) -> Result<(), String> {
        let cells = record.split('|').nth(5).ok_or("malformed VYGS record")?;
        let mut want: Vec<(usize, i32, i32, String)> = Vec::new();
        for c in cells.split(';').filter(|c| !c.is_empty()) {
            let p: Vec<&str> = c.split(':').collect();
            if p.len() != 4 {
                return Err(format!("malformed cell {c}"));
            }
            want.push((p[0].parse().map_err(|_| "bad id")?, p[1].parse().map_err(|_| "bad x")?,
                       p[2].parse().map_err(|_| "bad y")?, p[3].to_string()));
        }
        for &(nd, ..) in &want {
            self.erase_from_grid(nd);
            for sg in self.cell_segs[nd].clone() {
                self.remove_cell_from_segment(nd, sg);
            }
        }
        for (nd, x, y, o) in &want {
            self.nodes[*nd].left = *x;
            self.nodes[*nd].bottom = *y;
            self.adjust_orient(*nd, o);
        }
        for &(nd, ..) in &want {
            let (x, y) = (self.nodes[nd].left, self.nodes[nd].bottom);
            let rows = self.arch.height_in_rows(&self.nodes[nd]).max(1) as usize;
            let r0 = self.arch.find_closest_row(y);
            for r in r0..(r0 + rows).min(self.arch.rows.len()) {
                let seg = self.segs_in_row[r].iter().copied()
                    .find(|&sg| x >= self.segments[sg].min_x && x < self.segments[sg].max_x);
                if let Some(sg) = seg {
                    self.add_cell_to_segment_util(nd, sg);
                }
            }
            let gy = self.rt.grid.grid_snap_down_y(y).unwrap_or(0) as i64;
            self.paint_at(nd, gy);
        }
        Ok(())
    }

    // ── objective ──────────────────────────────────────────────────────────────────────────

    /// `Edge::hpwl` — the pins at `centre + offset`, bounding box `dx + dy`.
    pub(crate) fn edge_hpwl(&self, e: usize) -> u64 {
        let (mut x0, mut y0, mut x1, mut y1) = (i32::MAX, i32::MAX, i32::MIN, i32::MIN);
        for &p in &self.rt.edges[e] {
            let pin = &self.rt.pins[p];
            let n = &self.nodes[pin.node];
            let x = n.left + n.width / 2 + pin.ox;
            let y = n.bottom + n.height / 2 + pin.oy;
            x0 = x0.min(x);
            x1 = x1.max(x);
            y0 = y0.min(y);
            y1 = y1.max(y);
        }
        if self.rt.edges[e].is_empty() {
            return 0;
        }
        ((x1 - x0) as i64 + (y1 - y0) as i64) as u64
    }

    /// `Utility::hpwl(network)` — every edge.
    pub(crate) fn total_hpwl(&self) -> u64 {
        (0..self.rt.edges.len()).map(|e| self.edge_hpwl(e)).sum()
    }
}

/// Every placed cell node takes its instance's orientation (`improvePlacement`'s loop before
/// `initGrid`). Nodes start at R0, as `Network::addNode` makes them.
pub fn adopt_inst_orients(s: &mut Setup, db: &Db) {
    for nd in 0..s.nodes.len() {
        if s.nodes[nd].kind == Kind::Cell && s.nodes[nd].placed {
            let o = db.inst_get_orient(&s.nodes[nd].name);
            s.adjust_orient(nd, &o);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `boost::mt19937` seeded 1: its first output is 1791095845 — the reference's own
    /// fingerprint at the start of `improve_placement` (`VYGS|stage|mis|begin|rng=1791095845`).
    #[test]
    fn mt19937_seed_one_matches_the_reference() {
        let mut r = Mt19937::new(1);
        assert_eq!(r.next_u32(), 1791095845);
        assert_eq!(r.next_u32(), 4282876139, "the second draw: blockage1-opt's vs end fingerprint");
    }
}
