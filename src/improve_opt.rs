// SPDX-License-Identifier: Apache-2.0
//! `improve_placement`'s optimizers, sequenced by `Detailed::improve`'s script:
//!
//! `mis -p 10 -t 0.005; gs -p 10 -t 0.005; vs -p 10 -t 0.005; ro -p 10 -t 0.005;
//!  default -p 5 -f 20 -gen rng -obj hpwl -cost (hpwl);` then `orient -f`, the checks, and — when
//! the technology has no one-site master — the one-site-gap repair.
//!
//! Built: the random improver (`default`: `DetailedRandom` + `RandomGenerator` + `DetailedHPWL`)
//! and `orient -f` (`DetailedOrient`). ⬜ `mis`, `gs`, `vs`, `ro` are not: a run that reaches one
//! stops with it named, unless [`Options::rng_skip`] stands in for them (see there).

use crate::improve::{Kind, Setup};

/// What the driver is asked to do.
#[derive(Debug, Clone, Default)]
pub struct Options {
    /// ⬜ **A correlation aid, not a feature.** With the stages before `default` unbuilt, the
    /// random improver cannot start from the reference's generator state by itself: those stages
    /// consume draws even when they move nothing. `Some(n)` treats them as no-ops and discards `n`
    /// draws before `default` — `n` recovered from the reference's `VYGS` fingerprint. Removed as
    /// each stage lands and reproduces the draws itself.
    pub rng_skip: Option<u64>,
    /// Print a `VYGS|stage|…` line at every stage boundary, the reference trace's format.
    pub trace_stages: bool,
    /// ⬜ **A correlation aid, like `rng_skip`.** A reference `VYGS|stage|default|begin` record:
    /// every movable cell is put where it says (and oriented) before `default` runs, so the random
    /// improver and flipping can be compared on designs where the unbuilt stages MOVE cells.
    pub inject: Option<String>,
}

#[derive(Debug, Default, serde::Serialize)]
pub struct Report {
    pub hpwl_before: u64,
    pub hpwl_after: u64,
    pub stage_lines: Vec<String>,
    pub not_done: Vec<String>,
}

/// `Detailed::improve` over upstream's fixed script.
pub fn improve(s: &mut Setup, opts: &Options) -> Result<Report, String> {
    let mut rep = Report { hpwl_before: s.total_hpwl(), ..Default::default() };
    let script: [&[&str]; 5] = [
        &["mis", "-p", "10", "-t", "0.005"],
        &["gs", "-p", "10", "-t", "0.005"],
        &["vs", "-p", "10", "-t", "0.005"],
        &["ro", "-p", "10", "-t", "0.005"],
        &["default", "-p", "5", "-f", "20", "-gen", "rng", "-obj", "hpwl", "-cost", "(hpwl)"],
    ];
    for args in script {
        let name = match args[0] {
            "mis" => "independent set matching",
            "gs" => "global swaps",
            "vs" => "vertical swaps",
            "ro" => "reordering",
            _ => "random improvement",
        };
        s.log.push(format!("[INFO DPL-0303] Running algorithm for {name}."));
        if args[0] == "default" {
            if let Some(n) = opts.rng_skip {
                for _ in 0..n {
                    s.rt.rng.next_u32();
                }
            }
            if let Some(rec) = &opts.inject {
                s.inject_cells(rec)?;
            }
        }
        stage_line(s, opts, &mut rep, args[0], "begin");
        match args[0] {
            "default" => {
                random_improver(s, args);
            }
            other => {
                if opts.rng_skip.is_none() {
                    return Err(format!("improve_placement stage '{other}' is not implemented"));
                }
                rep.not_done.push(other.to_string());
            }
        }
        stage_line(s, opts, &mut rep, args[0], "end");
    }
    stage_line(s, opts, &mut rep, "orient", "begin");
    orient(s, true);
    stage_line(s, opts, &mut rep, "orient", "end");
    s.run_checks();
    if s.rt.disallow_gaps {
        rep.not_done.push("one-site-gap repair (getOneSiteGapViolationsPerSegment)".into());
    }
    rep.hpwl_after = s.total_hpwl();
    Ok(rep)
}

/// `VYGS|stage|<cmd>|<when>|rng=<next draw of a copy>|<id>:<left>:<bottom>:<orient>;…`
fn stage_line(s: &Setup, opts: &Options, rep: &mut Report, cmd: &str, when: &str) {
    if !opts.trace_stages {
        return;
    }
    let mut copy = s.rt.rng.clone();
    let mut l = format!("VYGS|stage|{cmd}|{when}|rng={}|", copy.next_u32());
    for n in &s.nodes {
        if n.kind == Kind::Terminal || n.fixed {
            continue;
        }
        l.push_str(&format!("{}:{}:{}:{};", n.id, n.left, n.bottom, n.orient));
    }
    rep.stage_lines.push(l);
}

// ── the random improver ──────────────────────────────────────────────────────────────────────

/// `DetailedHPWL` — per-edge HPWL, skipping edges with ≤ 1 or ≥ 100 pins.
struct Hpwl {
    edge: Vec<u64>,
    /// `affected_edges_` — appended by every `delta`, drained only by `accept` (`reject` is the
    /// base class's no-op, so a rejected move's edges wait for the next accept).
    pending: Vec<usize>,
}

const SKIP_NETS_LARGER_THAN: usize = 100;

impl Hpwl {
    fn counts(s: &Setup, e: usize) -> bool {
        let n = s.rt.edges[e].len();
        !(n <= 1 || n >= SKIP_NETS_LARGER_THAN)
    }

    /// `curr` — recomputes and stores every counted edge.
    fn curr(&mut self, s: &Setup) -> f64 {
        let mut total = 0u64;
        for e in 0..s.rt.edges.len() {
            if !Self::counts(s, e) {
                continue;
            }
            let h = s.edge_hpwl(e);
            self.edge[e] = h;
            total += h;
        }
        total as f64
    }

    /// `delta(journal)` — old minus new over the move's affected edges; positive improves.
    fn delta(&mut self, s: &Setup) -> f64 {
        let (mut old, mut new) = (0u64, 0u64);
        for e in s.affected_edges() {
            if !Self::counts(s, e) {
                continue;
            }
            self.pending.push(e);
            old += self.edge[e];
            new += s.edge_hpwl(e);
        }
        old as f64 - new as f64
    }

    fn accept(&mut self, s: &Setup) {
        for e in std::mem::take(&mut self.pending) {
            self.edge[e] = s.edge_hpwl(e);
        }
    }
}

/// `DetailedRandom::run` for `default -p 5 -f 20 -gen rng -obj hpwl -cost (hpwl)`.
fn random_improver(s: &mut Setup, args: &[&str]) {
    let (mut per_candidate, mut passes, mut tol) = (3.0f64, 1i32, 0.01f64);
    let mut i = 1;
    while i < args.len() {
        match args[i] {
            "-f" if i + 1 < args.len() => { i += 1; per_candidate = args[i].parse().unwrap_or(3.0); }
            "-p" if i + 1 < args.len() => { i += 1; passes = args[i].parse().unwrap_or(1); }
            "-t" if i + 1 < args.len() => { i += 1; tol = args[i].parse().unwrap_or(0.01); }
            "-gen" | "-obj" | "-cost" => { i += 1; }
            _ => {}
        }
        i += 1;
    }
    let tol = tol.max(0.01);
    let passes = passes.max(1);
    s.log.push("[INFO DPL-0324] Random improver is using random generator.".into());
    s.log.push("[INFO DPL-0325] Random improver is using hpwl objective.".into());
    s.log.push("[INFO DPL-0326] Random improver cost string is (a).".into());
    let mut obj = Hpwl { edge: vec![0; s.rt.edges.len()], pending: Vec::new() };
    let i_cost = obj.curr(s);
    let mut gen = (0u64, 0u64, 0u64); // attempts, moves, swaps — cumulative, as the object lives on
    for p in 1..=passes {
        s.resort_segments();
        let change = go(s, &mut obj, per_candidate, &mut gen);
        s.log.push(format!("[INFO DPL-0327] Pass {p:3} of random improver; improvement in cost is {:.2} percent.", change * 100.0));
        if change < tol {
            break;
        }
    }
    s.resort_segments();
    let f_cost = obj.curr(s);
    let imp = (i_cost - f_cost) / i_cost * 100.0;
    s.log.push(format!("[INFO DPL-0328] End of random improver; improvement is {imp:.6} percent."));
}

/// `DetailedRandom::go` — one pass.
fn go(s: &mut Setup, obj: &mut Hpwl, per_candidate: f64, gen: &mut (u64, u64, u64)) -> f64 {
    let mut candidates: Vec<usize> = s.single.clone();
    for m in s.multi.iter().skip(2) {
        candidates.extend(m.iter().copied());
    }
    if candidates.is_empty() {
        s.log.push("[INFO DPL-0203] No movable cells found".into());
        return 0.0;
    }
    let max_attempts = (per_candidate * candidates.len() as f64).ceil() as i64;
    s.rt.rng.shuffle(&mut candidates);
    let init = obj.curr(s);
    let mut curr = init;
    let mut calls = 0u64;
    for _ in 0..max_attempts {
        // "Pick a generator at random" — one generator, but the draw is taken.
        let _g = s.rt.rng.get_random(1);
        calls += 1;
        if !generate(s, &candidates, gen) {
            continue;
        }
        let next = curr - obj.delta(s);
        if next <= curr {
            s.accept_move();
            obj.accept(s);
            curr = next;
        } else {
            s.reject_move();
        }
    }
    s.log.push(format!("[INFO DPL-0332] End of pass, Generator random called {calls} times."));
    s.log.push(format!("[INFO DPL-0335] Generator random, Cumulative attempts {}, swaps {}, moves {:5} since last reset.",
                       gen.0, gen.2, gen.1));
    let scratch = obj.curr(s);
    let mismatch = if (scratch - curr).abs() > 1.0e-3 { 'Y' } else { 'N' };
    s.log.push(format!("[INFO DPL-0333] End of pass, Objective hpwl, Initial cost {}, Scratch cost {}, Incremental cost {}, Mismatch? {mismatch}",
                       sci(init), sci(scratch), sci(curr)));
    s.log.push(format!("[INFO DPL-0338] End of pass, Total cost is {}.", sci(scratch)));
    (init - curr) / init
}

/// `{:.6e}` as fmt prints it: `1.493588e+07`.
fn sci(v: f64) -> String {
    let t = format!("{v:.6e}");
    let (m, e) = t.split_once('e').unwrap();
    let e: i32 = e.parse().unwrap();
    format!("{m}e{}{:02}", if e < 0 { '-' } else { '+' }, e.abs())
}

/// `RandomGenerator::generate` — a random single-height candidate, a random target in a
/// ±10-site/±10-row window around it, `tryMove` then `trySwap`, up to five tries.
fn generate(s: &mut Setup, candidates: &[usize], gen: &mut (u64, u64, u64)) -> bool {
    gen.0 += 1;
    let a = &s.arch;
    let ydim = a.rows.len() as i32;
    let mut xwid = a.rows[0].site_spacing as f64;
    let xdim = 0.max(((a.max_x - a.min_x) as f64 / xwid) as i32);
    xwid = (a.max_x - a.min_x) as f64 / xdim as f64;
    let ywid = (a.max_y - a.min_y) as f64 / ydim as f64;
    let (min_x, min_y, srh) = (a.min_x as f64, a.min_y as f64, a.rows[0].height as f64);

    let ndi = candidates[s.rt.rng.get_random(candidates.len())];
    if s.arch.height_in_rows(&s.nodes[ndi]) != 1 {
        return false;
    }
    let (rlx, rly) = (10, 10);
    for _ in 1..=5 {
        let n = &s.nodes[ndi];
        let yi = n.bottom as f64 + 0.5 * n.height as f64;
        let xi = n.left as f64 + 0.5 * n.width as f64;
        let si = s.cell_segs[ndi][0];
        let grid_xi = (xdim - 1).min(0.max(((xi - min_x) / xwid) as i32));
        let grid_yi = (ydim - 1).min(0.max(((yi - min_y) / ywid) as i32));
        let rel_x = s.rt.rng.get_random((2 * rlx + 1) as usize) as i32;
        let rel_y = s.rt.rng.get_random((2 * rly + 1) as usize) as i32;
        let grid_xj = (xdim - 1).min(0.max(grid_xi - rlx + rel_x));
        let grid_yj = (ydim - 1).min(0.max(grid_yi - rly + rel_y));
        let xj = min_x + grid_xj as f64 * xwid;
        let yj = min_y + grid_yj as f64 * ywid;
        let rj = ((ydim - 1).min(0.max(((yj - min_y) / srh) as i32))) as usize;
        let yj = s.arch.rows[rj].bottom as f64;
        let sj = s.segs_in_row[rj].iter().copied()
            .find(|&sid| xj >= s.segments[sid].min_x as f64 && xj <= s.segments[sid].max_x as f64);
        let Some(sj) = sj else { continue };
        if s.nodes[ndi].group != s.segments[sj].reg {
            continue;
        }
        let (x0, y0) = (s.nodes[ndi].left, s.nodes[ndi].bottom);
        let (tx, ty) = (xj.round() as i32, yj.round() as i32);
        let trace = std::env::var_os("VYGES_IMPROVE_TRACE").is_some();
        if s.try_move(ndi, x0, y0, si, tx, ty, sj) {
            if trace { eprintln!("VYGR|try|{}|{}|{}|{}|move", s.nodes[ndi].name, tx, ty, sj); }
            gen.1 += 1;
            return true;
        }
        if s.try_swap(ndi, x0, y0, si, tx, ty, sj) {
            if trace { eprintln!("VYGR|try|{}|{}|{}|{}|swap", s.nodes[ndi].name, tx, ty, sj); }
            gen.2 += 1;
            return true;
        }
        if trace { eprintln!("VYGR|try|{}|{}|{}|{}|fail", s.nodes[ndi].name, tx, ty, sj); }
    }
    false
}

// ── orient -f ────────────────────────────────────────────────────────────────────────────────

/// `isLegalSym(masterSym, orient)`.
fn is_legal_sym(sym: (bool, bool, bool), o: &str) -> bool {
    let (x, y, r90) = sym;
    match o {
        "R0" => true,
        "MX" => x,
        "MY" => y,
        "R180" => x && y,
        "R90" | "R270" => r90,
        "MXR90" | "MYR90" => r90 && x && y,
        _ => false,
    }
}

/// `DetailedOrient::run(mgr, "orient -f")`.
fn orient(s: &mut Setup, flip: bool) {
    s.log.push("[INFO DPL-0380] Cell flipping.".into());
    let init = s.total_hpwl_xy();
    if init == 0 {
        return;
    }
    let (errors, changed) = orient_cells(s);
    if errors != 0 {
        s.log.push(format!("[WARNING DPL-0381] Encountered {errors} issues when orienting cells for rows."));
    }
    s.log.push(format!("[INFO DPL-0382] Changed {changed} cell orientations for row compatibility."));
    if flip {
        let n = flip_cells(s);
        s.log.push(format!("[INFO DPL-0383] Performed {n} cell flips."));
    }
    let curr = s.total_hpwl_xy();
    let imp = (init as f64 - curr as f64) / init as f64 * 100.0;
    s.log.push(format!("[INFO DPL-0384] End of flipping; objective is {}, improvement is {imp:.2} percent.", sci(curr as f64)));
}

/// `orientCells` — every movable cell oriented for the lowest row it is in.
fn orient_cells(s: &mut Setup) -> (usize, usize) {
    let (mut errors, mut changed) = (0, 0);
    for nd in 0..s.nodes.len() {
        let n = &s.nodes[nd];
        if n.kind == Kind::Terminal || n.fixed {
            continue;
        }
        let bottom = s.cell_segs[nd].iter().map(|&sg| s.segments[sg].row).min();
        let Some(row) = bottom else {
            errors += 1;
            continue;
        };
        let orig = s.nodes[nd].orient.clone();
        let ok = if s.arch.height_in_rows(&s.nodes[nd]) == 1 {
            orient_single(s, nd, row)
        } else {
            orient_multi(s, nd, row)
        };
        if !ok {
            errors += 1;
        }
        if orig != s.nodes[nd].orient {
            changed += 1;
        }
    }
    (errors, changed)
}

/// `orientSingleHeightCellForRow`.
fn orient_single(s: &mut Setup, nd: usize, row: usize) -> bool {
    let row_o = s.arch.rows[row].orient.clone();
    let cell_o = s.nodes[nd].orient.clone();
    let sym = s.rt.sym[nd];
    match row_o.as_str() {
        "R0" | "MY" => match cell_o.as_str() {
            "R0" | "MY" => is_legal_sym(sym, &cell_o),
            "MX" => { s.adjust_orient(nd, "R0"); true }
            "R180" => if is_legal_sym(sym, "MY") { s.adjust_orient(nd, "MY"); true } else { false },
            _ => false,
        },
        "MX" | "R180" => match cell_o.as_str() {
            "MX" | "R180" => is_legal_sym(sym, &cell_o),
            "R0" => if is_legal_sym(sym, "MX") { s.adjust_orient(nd, "MX"); true } else { false },
            "MY" => if is_legal_sym(sym, "R180") { s.adjust_orient(nd, "R180"); true } else { false },
            _ => false,
        },
        _ => false,
    }
}

/// `orientMultiHeightCellForRow` — flip about X when the rails want it.
fn orient_multi(s: &mut Setup, nd: usize, row: usize) -> bool {
    let a = &s.arch;
    let rails = |i: usize| s.rt.grid.grid_y(a.rows.get(i).map_or(i32::MIN, |r| r.bottom))
        .map_or((crate::drc::Power::Unknown, crate::drc::Power::Unknown), |g| s.rt.power.row_rails(g));
    let (top, bot) = s.rt.power.master_rails(&s.nodes[nd].master);
    let span = (s.nodes[nd].height as f64 / a.rows[row].height as f64).round() as usize;
    let (ok, flip) = crate::drc::power_compatible(bot, top, row, span, a.rows.len(),
                                                  &|i| rails(i).0, &|i| rails(i).1);
    if !ok {
        return false;
    }
    let sym = s.rt.sym[nd];
    if flip {
        let new = match s.nodes[nd].orient.as_str() {
            "R0" => "MX",
            "MY" => "R180",
            "MX" => "R0",
            "R180" => "MY",
            _ => return false,
        };
        if is_legal_sym(sym, new) {
            s.adjust_orient(nd, new);
            return true;
        }
        return false;
    }
    is_legal_sym(sym, &s.nodes[nd].orient)
}

/// `flipCells` — flip single-height cells about Y where that shortens their nets, in rows whose
/// site has Y symmetry. ⛔ Transcribed quirks: a flip that then fails the width check is NOT
/// undone (the cell stays flipped, uncounted), and `flipCellPadding` sets both pads to the left.
fn flip_cells(s: &mut Setup) -> usize {
    let mut mask = vec![0u64; s.rt.edges.len()];
    let mut traversal = 0u64;
    let mut nflips = 0;
    for sg in 0..s.segments.len() {
        let row = s.segments[sg].row;
        if !s.arch.rows[row].sym_y {
            continue;
        }
        let nodes = s.cells_in_seg[sg].clone();
        for i in 0..nodes.len() {
            let ndi = nodes[i];
            if s.arch.height_in_rows(&s.nodes[ndi]) != 1 {
                continue;
            }
            let (mut old, mut new) = (0.0f64, 0.0f64);
            traversal += 1;
            for &p in &s.rt.node_pins[ndi].clone() {
                let e = s.rt.pins[p].edge;
                let np = s.rt.edges[e].len();
                if np <= 1 || np >= SKIP_NETS_LARGER_THAN || mask[e] == traversal {
                    continue;
                }
                mask[e] = traversal;
                let (mut omin, mut omax) = (f64::MAX, f64::MIN);
                let (mut nmin, mut nmax) = (f64::MAX, f64::MIN);
                for &q in &s.rt.edges[e] {
                    let pin = &s.rt.pins[q];
                    let n = &s.nodes[pin.node];
                    let mut x = n.left as f64 + 0.5 * n.width as f64 + pin.ox as f64;
                    omin = omin.min(x);
                    omax = omax.max(x);
                    if pin.node == ndi {
                        x = n.left as f64 + 0.5 * n.width as f64 - pin.ox as f64;
                    }
                    nmin = nmin.min(x);
                    nmax = nmax.max(x);
                }
                old += omax - omin;
                new += nmax - nmin;
            }
            if new >= old {
                continue;
            }
            let ndl = if i == 0 { None } else { Some(nodes[i - 1]) };
            let ndr = if i == nodes.len() - 1 { None } else { Some(nodes[i + 1]) };
            let mut lx = ndl.map_or(s.segments[sg].min_x, |l| s.nodes[l].right());
            if let Some(l) = ndl {
                lx += s.rt.pads[l].1;
            }
            let mut rx = ndr.map_or(s.segments[sg].max_x, |r| s.nodes[r].left);
            if let Some(r) = ndr {
                rx -= s.rt.pads[r].0;
            }
            let (pl, pr) = s.rt.pads[ndi];
            if s.nodes[ndi].left - pr < lx || s.nodes[ndi].right() + pl > rx {
                continue;
            }
            let orig = s.nodes[ndi].orient.clone();
            let flipped = match orig.as_str() {
                "R0" => "MY",
                "R180" => "MX",
                "MY" => "R0",
                "MX" => "R180",
                _ => continue,
            };
            s.adjust_orient(ndi, flipped);
            if s.has_placement_violation(ndi) {
                s.adjust_orient(ndi, &orig);
                continue;
            }
            let lx = ndl.map_or(s.segments[sg].min_x, |l| s.nodes[l].right());
            let rx = ndr.map_or(s.segments[sg].max_x, |r| s.nodes[r].left);
            if s.nodes[ndi].width > rx - lx {
                continue;
            }
            // `flipCellPadding`: left = right = padLeft.
            let l = s.rt.pads[ndi].0;
            s.rt.pads[ndi] = (l, l);
            let ls = s.rt.pad_sites[ndi].0;
            s.rt.pad_sites[ndi] = (ls, ls);
            nflips += 1;
        }
    }
    nflips
}

impl Setup {
    /// `Utility::hpwl(network, x, y)` — every edge of ≥ 2 pins, box from `mergeInit`.
    pub(crate) fn total_hpwl_xy(&self) -> u64 {
        (0..self.rt.edges.len()).filter(|&e| self.rt.edges[e].len() > 1).map(|e| self.edge_hpwl(e)).sum()
    }
}

/// `updateDbInstLocations` — every movable std cell's orient and location back to the database.
pub fn write_back(s: &Setup, db: &mut vyges_opendb::Db) -> Result<usize, String> {
    let core = s.rt.grid.core;
    let mut moved = 0;
    for n in &s.nodes {
        if n.kind != Kind::Cell || n.fixed {
            continue;
        }
        if !(db.inst_is_core(&n.name) || db.inst_is_end_cap(&n.name)) {
            continue;
        }
        if db.inst_get_orient(&n.name) != n.orient {
            db.set_inst_orient(&n.name, &n.orient).map_err(|e| e.to_string())?;
        }
        let (x, y) = (core.0 + n.left, core.1 + n.bottom);
        if db.inst_location(&n.name) != (x, y) {
            db.set_inst_location(&n.name, x, y).map_err(|e| e.to_string())?;
            moved += 1;
        }
    }
    Ok(moved)
}
