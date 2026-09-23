// SPDX-License-Identifier: Apache-2.0
//! `improve_placement` — `Opendp::improvePlacement` (`Optdp.cpp`).
//!
//! ⬜ **Milestone 1 only**: the state the optimizers work on — the network (`createNetwork` +
//! `Architecture::postProcess`'s filler nodes), the architecture rows, and `DetailedMgr` as
//! `ShiftLegalizer::legalize` leaves it: fixed / single / multi-height cells, blockages, segments,
//! each segment's cells IN ORDER, and the five checks. The optimizers (`mis`, `gs`, `vs`, `ro`, the
//! random improver, flipping) are not built; see `docs/openroad/dpl/improve-placement-scoping.md`.
//!
//! Correlated against `dpl-improve-trace.py` captures (`VYGI|` lines): [`Setup::dump`] prints the
//! same records, so the state is compared structure for structure, not by its log counts.

use std::collections::HashMap;

use vyges_opendb::Db;

use crate::drc::Power;
use crate::grid::Grid;

// ── the network ──────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Cell,
    Terminal,
    Filler,
}

/// `dpl::Node`, as far as milestone 1 reads it. Coordinates are core-relative.
#[derive(Debug, Clone)]
pub struct Node {
    pub id: usize,
    pub kind: Kind,
    pub name: String,
    pub master: String,
    pub left: i32,
    pub bottom: i32,
    pub width: i32,
    pub height: i32,
    pub fixed: bool,
    /// `Node::group_id_`, `-1` when in no placement group.
    pub group: i32,
    pub orient: String,
    /// `dbInst::isPlaced` — for an instance node; true for terminals and fillers.
    pub placed: bool,
}

impl Node {
    pub(crate) fn right(&self) -> i32 {
        self.left + self.width
    }
    pub(crate) fn top(&self) -> i32 {
        self.bottom + self.height
    }
    /// `getCenterX` — `left + width / 2`, integer division.
    pub(crate) fn center_x(&self) -> i32 {
        self.left + self.width / 2
    }
}

/// `Opendp::createNetwork` — the instances (`createNetwork`'s filter, name order), then every
/// placed, non-supply terminal in block order.
///
/// ⚠️ **A terminal's height is `yMax − yMax` = 0** (`Network::addNode(dbBTerm*)`), transcribed.
/// ⚠️ A placed-but-not-fixed BLOCK is forced fixed ("treating it as fixed during legalization").
fn create_network(db: &Db, core: (i32, i32, i32, i32)) -> Vec<Node> {
    let mut nodes = Vec::new();
    for name in crate::network::network_insts(db) {
        let master = db.inst_master(&name);
        let (x, y) = db.inst_location(&name);
        let mtype = db.master_get_type(&master).unwrap_or_default();
        let mut fixed = crate::negotiate::status_is_fixed(&db.inst_get_placement_status(&name));
        if crate::drc::canonical_master_type(&mtype).starts_with("BLOCK") {
            fixed = true;
        }
        nodes.push(Node {
            id: nodes.len(), kind: Kind::Cell, name: name.clone(), master: master.clone(),
            left: x - core.0, bottom: y - core.1,
            width: db.master_get_width(&master) as i32, height: db.master_get_height(&master) as i32,
            // `Network::addNode` makes every node R0; `improvePlacement` then turns placed ones to
            // their instance's orient (`improve_run::adopt_inst_orients`), moving their pins.
            fixed, group: -1, orient: "R0".into(),
            placed: !matches!(db.inst_get_placement_status(&name).as_str(), "NONE" | "UNPLACED"),
        });
    }
    for bt in db.block_get_b_terms() {
        let net = db.bterm_get_net(&bt);
        if net.is_empty() {
            continue;
        }
        let sig = db.net_get_sig_type(&net);
        if sig == "POWER" || sig == "GROUND" {
            continue;
        }
        if !matches!(db.bterm_get_first_pin_placement_status(&bt).as_str(), "PLACED" | "FIRM" | "LOCKED" | "COVER") {
            continue;
        }
        let (x0, y0, x1) = (db.bterm_get_b_box_x_min(&bt), db.bterm_get_b_box_y_min(&bt),
                            db.bterm_get_b_box_x_max(&bt));
        nodes.push(Node {
            id: nodes.len(), kind: Kind::Terminal, name: bt.clone(), master: String::new(),
            left: x0 - core.0, bottom: y0 - core.1, width: x1 - x0, height: 0,
            fixed: true, group: -1, orient: "R0".into(), placed: true,
        });
    }
    nodes
}

// ── the architecture ─────────────────────────────────────────────────────────────────────────

/// `Architecture::Row`. `left` is the sub-row origin; `right = left + num_sites × site_spacing`.
#[derive(Debug, Clone)]
pub struct Row {
    pub left: i32,
    pub num_sites: i32,
    pub site_spacing: i32,
    pub site_width: i32,
    /// The site's `SYMMETRY Y` — `DetailedOrient::flipCells` flips only in such rows.
    pub sym_y: bool,
    pub bottom: i32,
    pub height: i32,
    pub orient: String,
}

impl Row {
    pub(crate) fn right(&self) -> i32 {
        self.left + self.num_sites * self.site_spacing
    }
    fn top(&self) -> i32 {
        self.bottom + self.height
    }
}

#[derive(Debug, Clone, Default)]
pub struct Arch {
    pub rows: Vec<Row>,
    pub min_x: i32,
    pub max_x: i32,
    pub min_y: i32,
    pub max_y: i32,
}

impl Arch {
    fn bound(&mut self) {
        self.min_x = self.rows.iter().map(|r| r.left).min().unwrap_or(i32::MAX);
        self.max_x = self.rows.iter().map(|r| r.right()).max().unwrap_or(i32::MIN);
        self.min_y = self.rows.iter().map(|r| r.bottom).min().unwrap_or(i32::MAX);
        self.max_y = self.rows.iter().map(|r| r.top()).max().unwrap_or(i32::MIN);
    }

    /// `Architecture::find_closest_row`.
    pub(crate) fn find_closest_row(&self, y: i32) -> usize {
        let mut r = 0;
        if y > self.rows[0].bottom {
            // `lower_bound` on bottom, then step back if past `y`.
            let mut l = self.rows.partition_point(|row| row.bottom < y);
            if l == self.rows.len() || self.rows[l].bottom > y {
                l -= 1;
            }
            r = l;
            if r < self.rows.len() - 1
                && (self.rows[r + 1].bottom - y).abs() < (self.rows[r].bottom - y).abs()
            {
                r += 1;
            }
        }
        r
    }

    /// `getCellHeightInRows` — `lround(height / rows[0].height)`.
    pub(crate) fn height_in_rows(&self, nd: &Node) -> i32 {
        (nd.height as f64 / self.rows[0].height as f64).round() as i32
    }
}

/// `Opendp::createArchitecture` then `Architecture::postProcess`, which merges co-linear sub-rows
/// and ADDS a FILLER node to the network for every gap between the architecture's box and a row.
fn create_architecture(db: &Db, core: (i32, i32, i32, i32), nodes: &mut Vec<Node>) -> Arch {
    let n = db.num_rows().unwrap_or(0);
    let min_row_height = (0..n)
        .filter_map(|i| db.nth_row(i).ok().flatten())
        .map(|(_, site, _)| db.site_get_height(&site))
        .min()
        .unwrap_or(i32::MAX);
    let mut arch = Arch::default();
    for i in 0..n {
        let Ok(Some((_, site, orient))) = db.nth_row(i) else { continue };
        if db.site_get_class(&site).unwrap_or_default() == "PAD" {
            continue;
        }
        if db.nth_row_direction(i).unwrap_or_default() != "HORIZONTAL" {
            continue;
        }
        if db.site_get_height(&site) > min_row_height {
            continue;
        }
        let name = db.nth_row_name(i).unwrap_or_default();
        arch.rows.push(Row {
            left: db.row_get_origin_x(&name) - core.0,
            bottom: db.row_get_origin_y(&name) - core.1,
            site_spacing: db.row_get_spacing(&name),
            num_sites: db.row_get_site_count(&name),
            site_width: db.site_get_width(&site),
            sym_y: db.site_get_symmetry_y(&site),
            height: db.site_get_height(&site),
            orient,
        });
    }
    arch.bound();
    // Clip each row to the box: `endGap = siteWidth − siteSpacing`, per row.
    let (min_x, max_x) = (arch.min_x, arch.max_x);
    for row in &mut arch.rows {
        let mut origin = row.left;
        let end_gap = row.site_width - row.site_spacing;
        if origin < min_x {
            origin = min_x;
            row.left = origin;
        }
        if origin + row.num_sites * row.site_spacing + end_gap > max_x && row.site_spacing != 0 {
            row.num_sites = (max_x - end_gap - origin) / row.site_spacing;
        }
    }
    post_process(&mut arch, nodes);
    arch
}

/// `Architecture::postProcess`.
fn post_process(arch: &mut Arch, nodes: &mut Vec<Node>) {
    arch.rows.sort_by_key(|r| r.bottom); // stable
    arch.bound();
    let (xmin, xmax) = (arch.min_x, arch.max_x);
    let mut merged: Vec<Row> = Vec::new();
    let mut r = 0;
    while r < arch.rows.len() {
        let mut subrows = vec![arch.rows[r].clone()];
        r += 1;
        while r < arch.rows.len() && arch.rows[r].bottom == subrows[0].bottom {
            subrows.push(arch.rows[r].clone());
            r += 1;
        }
        let mut intervals: Vec<(i32, i32)> = subrows.iter().map(|s| (s.left, s.right())).collect();
        intervals.sort();
        let mut stack: Vec<(i32, i32)> = vec![intervals[0]];
        for iv in &intervals[1..] {
            let mut top = *stack.last().unwrap();
            if top.1 < iv.0 {
                stack.push(*iv);
            } else {
                if top.1 < iv.1 {
                    top.1 = iv.1;
                }
                stack.pop();
                stack.push(top);
            }
        }
        let mut intervals = stack;
        intervals.sort();
        let mut first = subrows[0].clone();
        if subrows.len() > 1 {
            let (lx, rx) = (intervals[0].0, intervals.last().unwrap().1);
            first.num_sites = (rx - lx) / first.site_spacing;
            first.left = lx;
        }
        let (h, yb) = (first.height, first.bottom);
        let mut filler = |lx: i32, rx: i32| {
            nodes.push(Node {
                id: nodes.len(), kind: Kind::Filler, name: "-".into(), master: String::new(),
                left: lx, bottom: yb, width: rx - lx, height: h, fixed: true, group: -1,
                orient: "R0".into(), placed: true,
            });
        };
        if xmin < intervals[0].0 {
            filler(xmin, intervals[0].0);
        }
        for i in 1..intervals.len() {
            if intervals[i].0 > intervals[i - 1].1 {
                filler(intervals[i - 1].1, intervals[i].0);
            }
        }
        if xmax > intervals.last().unwrap().1 {
            filler(intervals.last().unwrap().1, xmax);
        }
        merged.push(first);
    }
    merged.sort_by_key(|r| r.bottom);
    arch.rows = merged;
}

// ── the detailed manager ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BlockageType {
    Placement,
    FixedInstance,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Blockage {
    x_min: i32,
    x_max: i32,
    pad_left: i32,
    pad_right: i32,
    kind: BlockageType,
}

impl Blockage {
    fn padded_min(&self) -> i32 {
        self.x_min - self.pad_left
    }
    fn padded_max(&self) -> i32 {
        self.x_max + self.pad_right
    }
    pub(crate) fn x_min(&self) -> i32 {
        self.x_min
    }
    pub(crate) fn x_max(&self) -> i32 {
        self.x_max
    }
    /// `isFixedInstance() || isPlacement()` — every blockage `findBlockages` records.
    pub(crate) fn blocks(&self) -> bool {
        matches!(self.kind, BlockageType::FixedInstance | BlockageType::Placement)
    }
}

/// `DetailedSeg`.
#[derive(Debug, Clone)]
pub struct Segment {
    pub id: usize,
    pub row: usize,
    /// `regId_`, `-1` for the default region.
    pub reg: i32,
    pub min_x: i32,
    pub max_x: i32,
}

/// The five checks `ShiftLegalizer::legalize` ends with, in its order.
#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct Checks {
    pub wrong_region: usize,
    pub row_alignment: usize,
    pub site_alignment: usize,
    pub overlaps: usize,
    pub edge_spacing: usize,
    pub padding: usize,
}

/// `DetailedMgr` after `ShiftLegalizer::legalize`.
pub struct Setup {
    pub arch: Arch,
    pub nodes: Vec<Node>,
    pub fixed: Vec<usize>,
    pub single: Vec<usize>,
    /// `multiHeightCells_[rows]`.
    pub multi: Vec<Vec<usize>>,
    pub wide: Vec<usize>,
    pub(crate) blockages: Vec<Vec<Blockage>>,
    pub segments: Vec<Segment>,
    pub(crate) segs_in_row: Vec<Vec<usize>>,
    pub cells_in_seg: Vec<Vec<usize>>,
    /// `reverseCellToSegs_`.
    pub cell_segs: Vec<Vec<usize>>,
    pub checks: Checks,
    /// `DPL-0310` movement, and whether the snap or shift moved anything (`DPL-0200`).
    pub movement: (i64, i64),
    pub displaced: bool,
    pub log: Vec<String>,
    pub rt: crate::improve_run::Runtime,
}

/// `DetailedMgr`'s constructor, `ShiftLegalizer::legalize`, in upstream's call order.
pub fn setup(db: &Db, padding: &crate::negotiate::Padding) -> Result<Setup, String> {
    setup_with(db, padding, 1, (0, 0))
}

/// [`setup`] with `improve_placement`'s `-random_seed` and `-max_displacement {x y}`.
pub fn setup_with(db: &Db, padding: &crate::negotiate::Padding, seed: u32, max_disp: (i32, i32))
    -> Result<Setup, String> {
    // `importDb`: `createNetwork` (nodes at R0, then edges and pins), `createArchitecture`,
    // `setUpPlacementGroups`.
    let grid = Grid::build(db)?;
    let core = grid.core;
    let mut nodes = create_network(db, core);
    let pins = crate::improve_run::build_pins(db, &nodes);
    let arch = create_architecture(db, core, &mut nodes);
    if arch.rows.is_empty() {
        return Err("no rows".into());
    }
    let groups = crate::regions::placement_groups(db, core, &crate::network::network_insts(db));
    let index: HashMap<String, usize> = nodes.iter().filter(|n| n.kind == Kind::Cell)
        .map(|n| (n.name.clone(), n.id)).collect();
    for (gi, g) in groups.iter().enumerate() {
        for c in &g.cells {
            if let Some(&i) = index.get(c) {
                nodes[i].group = gi as i32;
            }
        }
    }
    let mut node_pins = pins.1.clone();
    node_pins.resize(nodes.len(), Vec::new());
    let levels = routing_levels(db);
    let rt = crate::improve_run::Runtime::new(db, grid, &nodes, (pins.0, node_pins, pins.2), padding, &levels);
    let mut s = Setup {
        arch, nodes, fixed: Vec::new(), single: Vec::new(), multi: Vec::new(), wide: Vec::new(),
        blockages: Vec::new(), segments: Vec::new(), segs_in_row: Vec::new(),
        cells_in_seg: Vec::new(), cell_segs: Vec::new(), checks: Checks::default(),
        movement: (0, 0), displaced: false, log: Vec::new(), rt,
    };
    s.cell_segs = vec![Vec::new(); s.nodes.len()];
    // The node orients, then `initGrid` + `setFixedGridCells`, then the manager's constructor.
    crate::improve_run::adopt_inst_orients(&mut s, db);
    s.paint_fixed();
    // The manager's constructor (`limit` both ways), `setSeed`, `setMaxDisplacement`: a non-zero
    // request is in ROW-0 HEIGHTS on BOTH axes — upstream multiplies x by the row height too.
    let limit = (s.arch.max_x - s.arch.min_x).max(s.arch.max_y - s.arch.min_y) << 1;
    s.log.push(format!("[INFO DPL-0401] Setting random seed to {seed}."));
    s.rt.rng = crate::improve_run::Mt19937::new(seed);
    let h = s.arch.rows[0].height;
    let dx = if max_disp.0 != 0 { max_disp.0 * h } else { limit }.min(limit);
    let dy = if max_disp.1 != 0 { max_disp.1 * h } else { limit }.min(limit);
    s.rt.max_disp = (dx, dy);
    s.log.push(format!("[INFO DPL-0402] Setting maximum displacement {} {} to {dx} {dy} units.",
                       max_disp.0, max_disp.1));

    // `ShiftLegalizer::legalize`.
    s.collect_fixed_cells();
    s.collect_single_height_cells();
    s.collect_multi_height_cells();
    s.collect_wide_cells();
    s.find_blockages(db, core);
    s.find_segments(&groups, core);
    s.snap();
    s.shift();
    s.run_checks();
    Ok(s)
}

fn routing_levels(db: &Db) -> HashMap<String, i32> {
    let layers = db.layers_with_direction().unwrap_or_default();
    let types: Vec<(String, String)> = layers.iter()
        .map(|(n, _)| (n.clone(), db.layer_get_type(n).unwrap_or_default())).collect();
    crate::drc::routing_levels(&types)
}

impl Setup {
    /// `collectFixedCells` — every fixed node: instances, terminals and fillers alike.
    fn collect_fixed_cells(&mut self) {
        self.fixed = self.nodes.iter().filter(|n| n.fixed).map(|n| n.id).collect();
        self.log.push(format!("[INFO DPL-0320] Collected {} fixed cells.", self.fixed.len()));
    }

    /// `collectSingleHeightCells`.
    fn collect_single_height_cells(&mut self) {
        let arch = &self.arch;
        self.single = self.nodes.iter()
            .filter(|n| n.kind != Kind::Terminal && !n.fixed && arch.height_in_rows(n) == 1)
            .map(|n| n.id).collect();
        self.log.push(format!("[INFO DPL-0318] Collected {} single height cells.", self.single.len()));
    }

    /// `collectMultiHeightCells` — by rows spanned; the matrix is at least 2 long.
    fn collect_multi_height_cells(&mut self) {
        self.multi = vec![Vec::new(); 2];
        for n in &self.nodes {
            if n.kind == Kind::Terminal || n.fixed || self.arch.height_in_rows(n) == 1 {
                continue;
            }
            let spanned = self.arch.height_in_rows(n).max(0) as usize;
            if spanned >= self.multi.len() {
                self.multi.resize(spanned + 1, Vec::new());
            }
            self.multi[spanned].push(n.id);
        }
        for (i, m) in self.multi.iter().enumerate() {
            if !m.is_empty() {
                self.log.push(format!(
                    "[INFO DPL-0319] Collected {} multi-height cells spanning {} rows.", m.len(), i));
            }
        }
    }

    /// `collectWideCells` — ⛔ called BEFORE `findSegments` ("XXX: This requires segments!"), so
    /// the segment list is empty and this finds nothing. Transcribed as the empty scan it is.
    fn collect_wide_cells(&mut self) {
        self.wide.clear();
        for (s, seg) in self.segments.iter().enumerate() {
            for &c in &self.cells_in_seg[s] {
                if self.nodes[c].width > seg.max_x - seg.min_x {
                    self.wide.push(c);
                }
            }
        }
        self.log.push(format!("[INFO DPL-0321] Collected {} wide cells.", self.wide.len()));
    }

    /// `findBlockages(false)` — fixed non-terminal nodes (padded by `getCellSpacing` with no
    /// neighbour) and the network's hard blockages, per row, sorted and merged.
    fn find_blockages(&mut self, db: &Db, core: (i32, i32, i32, i32)) {
        let pads = self.rt.pads.clone();
        let a = &self.arch;
        let mut bl: Vec<Vec<Blockage>> = vec![Vec::new(); a.rows.len()];
        for &f in &self.fixed {
            let nd = &self.nodes[f];
            if nd.kind == Kind::Terminal {
                continue;
            }
            let (xmin, xmax) = (a.min_x.max(nd.left), a.max_x.min(nd.right()));
            let (ymin, ymax) = (a.min_y.max(nd.bottom), a.max_y.min(nd.top()));
            // `getCellSpacing(nullptr, nd)` is nd's LEFT pad; `(nd, nullptr)` its right.
            let (pl, pr) = pads[f];
            for (r, row) in a.rows.iter().enumerate() {
                if ymin < row.top() && ymax > row.bottom {
                    bl[r].push(Blockage { x_min: xmin, x_max: xmax, pad_left: pl, pad_right: pr,
                                          kind: BlockageType::FixedInstance });
                }
            }
        }
        let boxes = db.blockage_boxes().unwrap_or_default();
        for (i, b) in boxes.iter().enumerate() {
            if db.blockage_is_soft(i) {
                continue;
            }
            let (xmin, xmax) = (a.min_x.max(b.0 - core.0), a.max_x.min(b.2 - core.0));
            let (ymin, ymax) = (a.min_y.max(b.1 - core.1), a.max_y.min(b.3 - core.1));
            for (r, row) in a.rows.iter().enumerate() {
                if ymin < row.top() && ymax > row.bottom {
                    bl[r].push(Blockage { x_min: xmin, x_max: xmax, pad_left: 0, pad_right: 0,
                                          kind: BlockageType::Placement });
                }
            }
        }
        let key = |b: &Blockage| (b.padded_min(), b.padded_max());
        for row in &mut bl {
            if row.is_empty() {
                continue;
            }
            row.sort_by_key(key);
            let mut stack = vec![row[0]];
            for b in &row[1..] {
                let mut top = *stack.last().unwrap();
                if top.padded_max() < b.padded_min() {
                    stack.push(*b);
                } else {
                    if top.padded_max() < b.padded_max() {
                        top.pad_right = b.pad_right;
                        top.x_max = b.x_max;
                    }
                    stack.pop();
                    stack.push(top);
                }
            }
            stack.sort_by_key(key);
            *row = stack;
        }
        self.blockages = bl;
    }

    /// `findSegments` — the gaps between blockages per row, then sliced by region intervals, then
    /// snapped to sites.
    fn find_segments(&mut self, groups: &[crate::regions::Group], core: (i32, i32, i32, i32)) {
        let a = &self.arch;
        self.log.push(format!("[INFO DPL-0322] Image ({}, {}) - ({}, {})",
            a.min_x + core.0, a.min_y + core.1, a.max_x + core.0, a.max_y + core.1));
        let mut segs: Vec<Segment> = Vec::new();
        let mut in_row: Vec<Vec<usize>> = vec![Vec::new(); a.rows.len()];
        let push = |segs: &mut Vec<Segment>, in_row: &mut Vec<Vec<usize>>, r: usize, x1: i32, x2: i32, reg: i32| {
            let id = segs.len();
            segs.push(Segment { id, row: r, reg, min_x: x1, max_x: x2 });
            in_row[r].push(id);
        };
        for (r, row) in a.rows.iter().enumerate() {
            let (lx, rx) = (row.left, row.right());
            let b = &self.blockages[r];
            if b.is_empty() {
                let (x1, x2) = (a.min_x.max(lx), a.max_x.min(rx));
                if x2 > x1 {
                    push(&mut segs, &mut in_row, r, x1, x2, -1);
                }
                continue;
            }
            if b[0].padded_min() > a.min_x.max(lx) {
                let x1 = a.min_x.max(lx);
                let x2 = a.max_x.min(rx).min(b[0].padded_min());
                if x2 > x1 {
                    push(&mut segs, &mut in_row, r, x1, x2, -1);
                }
            }
            for i in 1..b.len() {
                if b[i].padded_min() > b[i - 1].padded_max() {
                    let x1 = a.min_x.max(lx).max(b[i - 1].padded_max());
                    let x2 = a.max_x.min(rx).min(b[i].padded_min());
                    if x2 > x1 {
                        push(&mut segs, &mut in_row, r, x1, x2, -1);
                    }
                }
            }
            let last = b[b.len() - 1];
            if last.padded_max() < a.max_x.min(rx) {
                let x1 = a.max_x.min(rx).min(a.min_x.max(lx).max(last.padded_max()));
                let x2 = a.max_x.min(rx);
                if x2 > x1 {
                    push(&mut segs, &mut in_row, r, x1, x2, -1);
                }
            }
        }
        // Slice by region intervals. ⚠️ New segments are appended to the row's list WHILE it is
        // scanned, and the scan reaches them — upstream's loop re-reads `size()`.
        for (reg, g) in groups.iter().enumerate() {
            let intervals = self.find_region_intervals(g);
            for (r, ivs) in intervals.iter().enumerate() {
                for &(il, ir) in ivs {
                    let mut s = 0;
                    while s < in_row[r].len() {
                        let sid = in_row[r][s];
                        let (sl, sr) = (segs[sid].min_x, segs[sid].max_x);
                        s += 1;
                        if ir <= sl || il >= sr {
                            continue;
                        }
                        if il <= sl && ir >= sr {
                            segs[sid].reg = reg as i32;
                        } else if il > sl && ir >= sr {
                            segs[sid].max_x = il;
                            push(&mut segs, &mut in_row, r, il, sr, reg as i32);
                        } else if ir < sr && il <= sl {
                            segs[sid].min_x = ir;
                            push(&mut segs, &mut in_row, r, sl, ir, reg as i32);
                        } else {
                            segs[sid].max_x = il;
                            let old_reg = segs[sid].reg;
                            push(&mut segs, &mut in_row, r, il, ir, reg as i32);
                            push(&mut segs, &mut in_row, r, ir, sr, old_reg);
                        }
                    }
                }
            }
        }
        // Segment ends onto sites. ⚠️ C++ integer division truncates toward zero, as Rust's does.
        for seg in &mut segs {
            let row = &a.rows[seg.row];
            let (origin, sp) = (row.left, row.site_spacing);
            let mut ix = (seg.min_x - origin) / sp;
            if origin + ix * sp < seg.min_x {
                ix += 1;
            }
            seg.min_x = origin + ix * sp;
            let ix = (seg.max_x - origin) / sp;
            seg.max_x = origin + ix * sp;
        }
        self.cells_in_seg = vec![Vec::new(); segs.len()];
        self.rt.seg_util = vec![0; segs.len()];
        self.segments = segs;
        self.segs_in_row = in_row;
    }

    /// `findRegionIntervals` — per row, the site-aligned spans a region's rects cover over the
    /// row's WHOLE height, sorted and merged. ⚠️ Row `r` is taken at `min_y + r × single row
    /// height`, assuming stacked rows, as upstream does.
    fn find_region_intervals(&self, g: &crate::regions::Group) -> Vec<Vec<(i32, i32)>> {
        let a = &self.arch;
        let h = a.rows[0].height;
        let mut out: Vec<Vec<(i32, i32)>> = vec![Vec::new(); a.rows.len()];
        for rect in &g.rects {
            for (r, row) in a.rows.iter().enumerate() {
                let lb = a.min_y + r as i32 * h;
                let ub = lb + h;
                if rect.3 >= ub && rect.1 <= lb {
                    let (origin, sp) = (row.left, row.site_spacing);
                    let i0 = (rect.0 - origin) / sp;
                    let mut i1 = (rect.2 - origin) / sp;
                    if origin + i1 * sp != rect.2 {
                        i1 += 1;
                    }
                    if i1 > i0 {
                        out[r].push((origin + i0 * sp, origin + i1 * sp));
                    }
                }
            }
        }
        for ivs in &mut out {
            if ivs.is_empty() {
                continue;
            }
            ivs.sort();
            let mut stack = vec![ivs[0]];
            for iv in &ivs[1..] {
                let mut top = *stack.last().unwrap();
                if top.1 < iv.0 {
                    stack.push(*iv);
                } else {
                    if top.1 < iv.1 {
                        top.1 = iv.1;
                    }
                    stack.pop();
                    stack.push(top);
                }
            }
            stack.sort();
            *ivs = stack;
        }
        out
    }

    /// `checkMasterSymmetry(arch, nd, row)`: R0/MY rows take anything; MX/R180 rows need the
    /// master's X symmetry; any other orient refuses.
    pub(crate) fn master_symmetry_ok(&self, sym_x: bool, row: usize) -> bool {
        match self.arch.rows[row].orient.as_str() {
            "R0" | "MY" => true,
            "MX" | "R180" => sym_x,
            _ => false,
        }
    }

    /// `findClosestSegment` — the nearest segment of the cell's region, preferring one wide enough.
    fn find_closest_segment(&self, nd: &Node, sym_x: bool) -> Option<usize> {
        let a = &self.arch;
        let row = a.find_closest_row(nd.bottom);
        let (mut dist1, mut dist2) = (i32::MAX as i64, i32::MAX as i64);
        let (mut best1, mut best2): (Option<usize>, Option<usize>) = (None, None);
        let consider = |sid: usize, vert: i64, best1: &mut Option<usize>, best2: &mut Option<usize>,
                            dist1: &mut i64, dist2: &mut i64| {
            let seg = &self.segments[sid];
            if nd.group != seg.reg || !self.master_symmetry_ok(sym_x, seg.row) {
                return;
            }
            let xx = seg.min_x.max((seg.max_x - nd.width).min(nd.left));
            let hori = (xx - nd.left).abs().max(0) as i64;
            let fits = nd.width <= seg.max_x - seg.min_x;
            if best1.is_none() || hori + vert < *dist1 {
                *best1 = Some(sid);
                *dist1 = hori + vert;
            }
            if fits && (best2.is_none() || hori + vert < *dist2) {
                *best2 = Some(sid);
                *dist2 = hori + vert;
            }
        };
        for &sid in &self.segs_in_row[row] {
            consider(sid, 0, &mut best1, &mut best2, &mut dist1, &mut dist2);
        }
        let n = a.rows.len() as i64;
        let srh = a.rows[0].height as i64;
        for offset in 1..=n {
            let vert = offset * srh;
            let below = row as i64 - offset;
            if below >= 0 && (vert <= dist1 || vert <= dist2) {
                for &sid in &self.segs_in_row[below as usize] {
                    consider(sid, vert, &mut best1, &mut best2, &mut dist1, &mut dist2);
                }
            }
            let above = row as i64 + offset;
            if above <= n - 1 && (vert <= dist1 || vert <= dist2) {
                for &sid in &self.segs_in_row[above as usize] {
                    consider(sid, vert, &mut best1, &mut best2, &mut dist1, &mut dist2);
                }
            }
        }
        best2.or(best1)
    }

    /// `findClosestSpanOfSegmentsDfs`.
    fn span_dfs(&self, sid: usize, xmin: i32, xmax: i32, top: usize, stack: &mut Vec<usize>,
                out: &mut Vec<Vec<usize>>) {
        stack.push(sid);
        let row = self.segments[sid].row;
        if row < top {
            for &next in &self.segs_in_row[row + 1] {
                let s = &self.segments[next];
                if xmax.min(s.max_x) - xmin.max(s.min_x) > 0 {
                    self.span_dfs(next, xmin.max(s.min_x), xmax.min(s.max_x), top, stack, out);
                }
            }
        } else {
            out.push(stack.clone());
        }
        stack.pop();
    }

    /// `findClosestSpanOfSegments` — for a multi-height cell, the nearest vertical run of
    /// segments, all in the cell's region, power- and symmetry-compatible at the bottom row.
    fn find_closest_span(&self, nd: &Node, sym_x: bool, power: &crate::negotiate::PowerModel,
                         grid: &Grid) -> Option<Vec<usize>> {
        let a = &self.arch;
        let spanned = a.height_in_rows(nd);
        if spanned <= 1 {
            return None;
        }
        let (mut disp1, mut disp2) = (f64::MAX, f64::MAX);
        let (mut best1, mut best2): (Vec<usize>, Vec<usize>) = (Vec::new(), Vec::new());
        let (bot, top) = power.master_rails(&nd.master).into_bot_top();
        for r in 0..a.rows.len() {
            // `Architecture::powerCompatible(nd, row r)` over the architecture's rows.
            let rails = |i: usize| grid.grid_y(a.rows.get(i).map_or(i32::MIN, |row| row.bottom))
                .map_or((Power::Unknown, Power::Unknown), |g| power.row_rails(g));
            let span = (nd.height as f64 / a.rows[r].height as f64).round() as usize;
            let (ok, _) = crate::drc::power_compatible(bot, top, r, span, a.rows.len(),
                                                       &|i| rails(i).0, &|i| rails(i).1);
            if !ok || !self.master_symmetry_ok(sym_x, r) {
                continue;
            }
            let t = r + spanned as usize - 1;
            if t >= a.rows.len() {
                continue;
            }
            for &sid in &self.segs_in_row[r] {
                let mut cands = Vec::new();
                let s = &self.segments[sid];
                self.span_dfs(sid, s.min_x, s.max_x, t, &mut Vec::new(), &mut cands);
                for c in cands {
                    if c.iter().any(|&x| self.segments[x].reg != nd.group) {
                        continue;
                    }
                    let first = &self.segments[c[0]];
                    let dy = (nd.bottom - a.rows[first.row].bottom).abs();
                    let (mut xmin, mut xmax) = (first.min_x, first.max_x);
                    for &x in &c[1..] {
                        xmin = xmin.max(self.segments[x].min_x);
                        xmax = xmax.min(self.segments[x].max_x);
                    }
                    let width = xmax - xmin;
                    let ww = nd.width.min(width);
                    let (lx, rx) = (xmin + ww / 2, xmax - ww / 2);
                    let xc = nd.center_x();
                    let dx = (xc - lx.max(rx.min(xc))).abs();
                    let d = (dx + dy) as f64;
                    if best1.is_empty() || d < disp1 {
                        best1 = c.clone();
                        disp1 = d;
                    }
                    if (best2.is_empty() || d < disp2) && nd.width <= width {
                        best2 = c;
                        disp2 = d;
                    }
                }
            }
        }
        if !best2.is_empty() { Some(best2) } else if !best1.is_empty() { Some(best1) } else { None }
    }

    /// `addCellToSegment` — into the segment's list SORTED by centre X (`lower_bound`: before the
    /// first cell whose centre is not less).
    pub(crate) fn add_cell_to_segment(&mut self, nd: usize, seg: usize) {
        let x = self.nodes[nd].center_x();
        let nodes = &self.nodes;
        let at = self.cells_in_seg[seg].partition_point(|&c| nodes[c].center_x() < x);
        self.cells_in_seg[seg].insert(at, nd);
        self.cell_segs[nd].push(seg);
    }

    /// `assignCellsToSegments` for the single-height cells, then each multi-height bucket from 2
    /// rows up — ShiftLegalizer's "Snap". Positions are clamped into the chosen segment(s).
    fn snap(&mut self) {
        let mut orig: Vec<(i32, i32)> = self.nodes.iter().map(|n| (n.left, n.bottom)).collect();
        let mut lists: Vec<Vec<usize>> = Vec::new();
        if !self.single.is_empty() {
            lists.push(self.single.clone());
        }
        for i in 2..self.multi.len() {
            if !self.multi[i].is_empty() {
                lists.push(self.multi[i].clone());
            }
        }
        for list in lists {
            let (mut n_assigned, mut mx, mut my) = (0usize, 0i64, 0i64);
            for nd in list {
                let node = self.nodes[nd].clone();
                if self.arch.height_in_rows(&node) == 1 {
                    let Some(sid) = self.find_closest_segment(&node, self.rt.sym[nd].0) else { continue };
                    self.add_cell_to_segment_util(nd, sid);
                    n_assigned += 1;
                    let seg = &self.segments[sid];
                    let xx = seg.min_x.max((seg.max_x - node.width).min(node.left));
                    let yy = self.arch.rows[seg.row].bottom;
                    mx += (node.left - xx).abs() as i64;
                    my += (node.bottom - yy).abs() as i64;
                    self.nodes[nd].left = xx;
                    self.nodes[nd].bottom = yy;
                    self.paint_in_grid(nd);
                } else {
                    let Some(span) = self.find_closest_span(&node, self.rt.sym[nd].0, &self.rt.power, &self.rt.grid) else { continue };
                    let (mut xmin, mut xmax) = (self.segments[span[0]].min_x, self.segments[span[0]].max_x);
                    for &sid in &span {
                        xmin = xmin.max(self.segments[sid].min_x);
                        xmax = xmax.min(self.segments[sid].max_x);
                        self.add_cell_to_segment_util(nd, sid);
                    }
                    n_assigned += 1;
                    let xx = xmin.max((xmax - node.width).min(node.left));
                    let yy = self.arch.rows[self.segments[span[0]].row].bottom;
                    mx += (node.left - xx).abs() as i64;
                    my += (node.bottom - yy).abs() as i64;
                    self.nodes[nd].left = xx;
                    self.nodes[nd].bottom = yy;
                    self.paint_in_grid(nd);
                }
            }
            self.log.push(format!(
                "[INFO DPL-0310] Assigned {} cells into segments.  Movement in X-direction is {:.6}, movement in Y-direction is {:.6}.",
                n_assigned, mx as f64, my as f64));
            self.movement.0 += mx;
            self.movement.1 += my;
        }
        for (i, n) in self.nodes.iter().enumerate() {
            if !n.fixed && n.kind == Kind::Cell && (n.left, n.bottom) != orig[i] {
                self.displaced = true;
            }
        }
        orig.clear();
    }

    /// `ShiftLegalizer::shift` + `clump` + `merge` + `violated`: the movable cells clumped
    /// between per-segment dummies (weight 1e8) to remove overlap. On a legal placement nothing
    /// moves; transcribed in full because the later optimizers use the same arrangement.
    fn shift(&mut self) {
        let nnodes = self.nodes.len();
        let nsegs = self.segments.len();
        let total = nnodes + 2 * nsegs;
        // Dummy nodes: left `nnodes + i`, right `nnodes + nsegs + i`, width 0.
        let left_of = |i: usize, s: &Setup| (s.segments[i].min_x, 0);
        let right_of = |i: usize, s: &Setup| (s.segments[i].max_x, 0);
        let pos_width = |id: usize, s: &Setup| -> (i32, i32) {
            if id < nnodes {
                (s.nodes[id].left, s.nodes[id].width)
            } else if id < nnodes + nsegs {
                left_of(id - nnodes, s)
            } else {
                right_of(id - nnodes - nsegs, s)
            }
        };
        let mut incoming: Vec<Vec<usize>> = vec![Vec::new(); total];
        for i in 0..nsegs {
            let mut seq = vec![nnodes + i];
            seq.extend(self.cells_in_seg[i].iter().copied());
            seq.push(nnodes + nsegs + i);
            for w in seq.windows(2) {
                incoming[w[1]].push(w[0]);
            }
        }
        let cells: Vec<usize> = {
            let mut c = self.single.clone();
            for i in 2..self.multi.len() {
                c.extend(self.multi[i].iter().copied());
            }
            c.into_iter().filter(|&n| !self.cell_segs[n].is_empty()).collect()
        };
        #[derive(Clone)]
        struct Clump {
            nodes: Vec<usize>,
            wposn: f64,
            weight: f64,
            posn: i32,
        }
        let mut offset = vec![0i32; total];
        let mut ptr = vec![usize::MAX; total];
        let mut clumps: Vec<Clump> = Vec::new();
        for i in 0..nsegs {
            let id = nnodes + i;
            let x = self.segments[i].min_x;
            ptr[id] = clumps.len();
            clumps.push(Clump { nodes: vec![id], wposn: 1.0e8 * x as f64, weight: 1.0e8, posn: x });
        }
        for &nd in &cells {
            let n = &self.nodes[nd];
            let mut posn = n.left;
            for &sid in &self.cell_segs[nd] {
                let s = &self.segments[sid];
                posn = posn.max(s.min_x).min(s.max_x - n.width);
            }
            ptr[nd] = clumps.len();
            clumps.push(Clump { nodes: vec![nd], wposn: n.left as f64, weight: 1.0, posn });
        }
        for i in 0..nsegs {
            let id = nnodes + nsegs + i;
            let x = self.segments[i].max_x;
            ptr[id] = clumps.len();
            clumps.push(Clump { nodes: vec![id], wposn: 1.0e8 * x as f64, weight: 1.0e8, posn: x });
        }
        for start in 0..clumps.len() {
            let mut r = start;
            loop {
                // `violated(r, l, dist)` — the worst overlap against any left neighbour.
                let (mut l, mut worst, mut dist) = (usize::MAX, i32::MAX, i32::MAX);
                for &ndr in &clumps[r].nodes {
                    for &ndl in &incoming[ndr] {
                        let t = ptr[ndl];
                        if t == r {
                            continue;
                        }
                        let pdst = clumps[r].posn + offset[ndr];
                        let psrc = clumps[t].posn + offset[ndl];
                        let gap = pos_width(ndl, self).1;
                        let diff = pdst - (psrc + gap);
                        if diff < 0 && diff < worst {
                            worst = diff;
                            l = t;
                            dist = offset[ndl] + gap - offset[ndr];
                        }
                    }
                }
                if l == usize::MAX {
                    break;
                }
                let moved = std::mem::take(&mut clumps[r].nodes);
                for &nd in &moved {
                    offset[nd] += dist;
                    ptr[nd] = l;
                }
                let (rw, rwt) = (clumps[r].wposn, clumps[r].weight);
                clumps[l].nodes.extend(moved);
                clumps[l].wposn += rw - dist as f64 * rwt;
                clumps[l].weight += rwt;
                clumps[l].posn = (clumps[l].wposn / clumps[l].weight).floor() as i32;
                r = l;
            }
        }
        for &nd in &cells {
            let row = self.cell_segs[nd].iter().map(|&s| self.segments[s].row).min().unwrap();
            let new_x = clumps[ptr[nd]].posn + offset[nd];
            let new_y = self.arch.rows[row].bottom;
            if (new_x, new_y) != (self.nodes[nd].left, self.nodes[nd].bottom) {
                self.displaced = true;
            }
            self.nodes[nd].left = new_x;
            self.nodes[nd].bottom = new_y;
        }
    }

    /// The five checks, in `legalize`'s order: region (313), row (315), site (314), overlap (311),
    /// edge spacing + padding (312).
    pub(crate) fn run_checks(&mut self) {
        let pads = self.rt.pads.clone();
        let a = &self.arch;
        let srh = a.rows[0].height;
        let movable = |n: &Node| n.kind != Kind::Terminal && !n.fixed;

        // `checkRegionAssignment`.
        let mut wrong = 0;
        for (s, seg) in self.segments.iter().enumerate() {
            wrong += self.cells_in_seg[s].iter().filter(|&&c| self.nodes[c].group != seg.reg).count();
        }
        self.log.push(format!("[INFO DPL-0313] Found {wrong} cells in wrong regions."));

        // `checkRowAlignment`.
        let mut row_err = 0;
        for n in self.nodes.iter().filter(|n| movable(n)) {
            let rb = a.find_closest_row(n.bottom) as i64;
            let rt = rb + a.height_in_rows(n) as i64 - 1;
            if rb < 0 || rt >= a.rows.len() as i64 {
                row_err += 1;
                continue;
            }
            if n.bottom != a.rows[rb as usize].bottom || n.top() != a.rows[rt as usize].top() {
                row_err += 1;
            }
        }
        self.log.push(format!("[INFO DPL-0315] Found {row_err} row alignment problems."));

        // `checkSiteAlignment` — only cells in segments.
        let mut site_err = 0;
        for n in self.nodes.iter().filter(|n| movable(n)) {
            if self.cell_segs[n.id].is_empty() {
                continue;
            }
            let mut rb = (n.bottom - a.min_y) / srh;
            let spanned = n.height / srh;
            let mut rt = rb + spanned - 1;
            if rb < 0 || rt >= a.rows.len() as i32 {
                site_err += 1;
            }
            rb = rb.max(0);
            rt = rt.min(a.rows.len() as i32 - 1);
            for r in rb..=rt {
                let row = &a.rows[r as usize];
                let sid = (n.left - row.left) / row.site_spacing;
                if n.left != row.left + sid * row.site_spacing {
                    site_err += 1;
                }
            }
        }
        self.log.push(format!("[INFO DPL-0314] Found {site_err} site alignment problems."));

        // `checkOverlapInSegments` — adjacent cells by centre, and cells outside the segment.
        let mut overlaps = 0;
        for (s, seg) in self.segments.iter().enumerate() {
            let mut t = self.cells_in_seg[s].clone();
            t.sort_by_key(|&c| self.nodes[c].center_x());
            for w in t.windows(2) {
                if self.nodes[w[0]].right() > self.nodes[w[1]].left {
                    overlaps += 1;
                }
            }
            for &c in &t {
                if self.nodes[c].left < seg.min_x || self.nodes[c].right() > seg.max_x {
                    overlaps += 1;
                }
            }
        }
        self.log.push(format!("[INFO DPL-0311] Found {overlaps} overlaps between adjacent cells."));

        // `checkEdgeSpacingInSegments` — `checkDRC` on the LEFT cell of every adjacent pair (the
        // last cell of a segment is never checked), and the padding gap.
        let drc_bad: Vec<bool> = (0..self.nodes.len())
            .map(|i| self.nodes[i].kind == Kind::Cell && self.has_placement_violation(i)).collect();
        let (mut err_n, mut err_p) = (0, 0);
        for s in 0..self.segments.len() {
            let mut t = self.cells_in_seg[s].clone();
            t.sort_by_key(|&c| self.nodes[c].left);
            for w in t.windows(2) {
                let gap = self.nodes[w[1]].left - self.nodes[w[0]].right();
                if drc_bad[w[0]] {
                    err_n += 1;
                }
                if gap < pads[w[0]].1 + pads[w[1]].0 {
                    err_p += 1;
                }
            }
        }
        self.log.push(format!(
            "[INFO DPL-0312] Found {err_n} edge spacing violations and {err_p} padding violations."));

        self.checks = Checks { wrong_region: wrong, row_alignment: row_err, site_alignment: site_err,
                               overlaps, edge_spacing: err_n, padding: err_p };
    }

    /// The `VYGI|` records `dpl-improve-trace.py` prints, for a structure-for-structure compare.
    pub fn dump(&self) -> Vec<String> {
        let mut out = vec![format!("VYGI|arch|{}|{}|{}|{}", self.arch.min_x, self.arch.max_x,
                                   self.arch.min_y, self.arch.max_y)];
        for (i, r) in self.arch.rows.iter().enumerate() {
            out.push(format!("VYGI|row|{}|{}|{}|{}|{}|{}|{}", i, r.left, r.right(), r.bottom,
                             r.height, r.site_spacing, r.orient));
        }
        for n in &self.nodes {
            let t = match n.kind { Kind::Cell => 'C', Kind::Terminal => 'T', Kind::Filler => 'F' };
            out.push(format!("VYGI|node|{}|{}|{}|{}|{}|{}|{}|{}|{}", n.id, t, n.name, n.left,
                             n.bottom, n.width, n.height, n.fixed as i32, n.group));
        }
        for (s, seg) in self.segments.iter().enumerate() {
            let cells: Vec<String> = self.cells_in_seg[s].iter().map(|c| c.to_string()).collect();
            out.push(format!("VYGI|seg|{}|{}|{}|{}|{}|{}", seg.id, seg.row, seg.reg, seg.min_x,
                             seg.max_x, cells.join(",")));
        }
        out
    }
}

trait BotTop {
    fn into_bot_top(self) -> (Power, Power);
}

impl BotTop for (Power, Power) {
    /// `PowerModel::master_rails` gives `(top, bottom)`; `power_compatible` wants `(bottom, top)`.
    fn into_bot_top(self) -> (Power, Power) {
        (self.1, self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(bottom: i32) -> Row {
        Row { left: 0, num_sites: 10, site_spacing: 100, site_width: 100, sym_y: true, bottom, height: 1000,
              orient: "R0".into() }
    }

    /// Upstream `find_closest_row`: below the first row → 0; a tie between two rows keeps the
    /// LOWER (`<`, not `<=`).
    #[test]
    fn find_closest_row_ties_go_down() {
        let a = Arch { rows: vec![row(0), row(1000), row(2000)], ..Default::default() };
        assert_eq!(a.find_closest_row(-50), 0);
        assert_eq!(a.find_closest_row(1000), 1);
        assert_eq!(a.find_closest_row(1500), 1, "a tie stays on the lower row");
        assert_eq!(a.find_closest_row(1501), 2);
        assert_eq!(a.find_closest_row(9000), 2);
    }
}
