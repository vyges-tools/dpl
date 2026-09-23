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
use crate::lemon::{ListDigraph, NetworkSimplex};

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
    /// ⬜ **A correlation aid, like `rng_skip`.** A file of reference `VYGS|stage` records: at
    /// [`Options::start_stage`]'s `begin`, every movable cell is put where that record says (and
    /// oriented), so one stage can be compared on its own whatever the others do.
    pub inject_file: Option<String>,
    /// Run from this stage (the stages before it are skipped; `rng_skip` and `inject_file` apply
    /// at its `begin`). `None` runs the whole script.
    pub start_stage: Option<String>,
    /// Stop after this stage's `end` (no `orient`, no checks).
    pub stop_stage: Option<String>,
    /// After `orient`, run the one-site-gap DETECTION on any design (not only those that disallow
    /// the gaps) and print `VYGG|<segment>|<node id>` per cell it records, then `VYGG|done` — the
    /// reference trace's format, and the only witness the detection has: no upstream case has a
    /// gap for it to find with the option that turns it on.
    pub report_gaps: bool,
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
    let mut started = opts.start_stage.is_none();
    for args in script {
        if !started {
            if opts.start_stage.as_deref() != Some(args[0]) {
                rep.not_done.push(format!("{} (skipped)", args[0]));
                continue;
            }
            started = true;
            if let Some(n) = opts.rng_skip {
                for _ in 0..n {
                    s.rt.rng.next_u32();
                }
            }
            if let Some(f) = &opts.inject_file {
                let text = std::fs::read_to_string(f).map_err(|e| format!("{f}: {e}"))?;
                let key = format!("VYGS|stage|{}|begin|", args[0]);
                let rec = text.lines().find(|l| l.starts_with(&key)).ok_or(format!("no {key} record"))?;
                s.inject_cells(rec)?;
            }
        }
        let name = match args[0] {
            "mis" => "independent set matching",
            "gs" => "global swaps",
            "vs" => "vertical swaps",
            "ro" => "reordering",
            _ => "random improvement",
        };
        s.log.push(format!("[INFO DPL-0303] Running algorithm for {name}."));
        stage_line(s, opts, &mut rep, args[0], "begin");
        match args[0] {
            "default" => random_improver(s, args),
            "gs" => global_swap(s, args),
            "vs" => vertical_swap(s, args),
            "mis" => mis(s, args),
            "ro" => reorder(s, args),
            other => return Err(format!("improve_placement stage '{other}' is not implemented")),
        }
        stage_line(s, opts, &mut rep, args[0], "end");
        if opts.stop_stage.as_deref() == Some(args[0]) {
            rep.hpwl_after = s.total_hpwl();
            return Ok(rep);
        }
    }
    // `disallow_one_site_gaps;` is appended to the script when the technology has no one-site
    // master: `doDetailedCommand` announces it, prints the stage's begin, and runs NOTHING — the
    // command falls through to `else { return; }` before any end.
    if s.rt.disallow_gaps && opts.stop_stage.is_none() {
        s.log.push("[INFO DPL-0303] Running algorithm for disallow_one_site_gaps.".into());
        stage_line(s, opts, &mut rep, "disallow_one_site_gaps", "begin");
    }
    stage_line(s, opts, &mut rep, "orient", "begin");
    orient(s, true);
    stage_line(s, opts, &mut rep, "orient", "end");
    if opts.report_gaps {
        for (seg, id) in one_site_gap_violations(s) {
            rep.stage_lines.push(format!("VYGG|{seg}|{id}"));
        }
        rep.stage_lines.push("VYGG|done".into());
    }
    s.run_checks();
    if s.rt.disallow_gaps {
        // ⬜ The REPAIR (`fixOneSiteGapViolations`) is not built: it moves nothing on the only
        // upstream case that reaches it (`gcd_no_one_site_gaps-opt`, final DEF identical without
        // it), so a transcription would have no witness. What IS built is the detection, so the
        // repair is named as not done exactly when a design has a gap it would have acted on.
        let v = one_site_gap_violations(s).len();
        if v > 0 {
            rep.not_done.push(format!(
                "one-site-gap repair (fixOneSiteGapViolations): {v} violation(s) left unrepaired"));
        }
    }
    rep.hpwl_after = s.total_hpwl();
    Ok(rep)
}

/// `getOneSiteGapViolationsPerSegment(v, false)` — `(segment, node id)` per cell it would record
/// (or, with `fix_violations`, try to fix).
///
/// Per segment of two or more cells (re-sorted by centre, as `resortSegment` leaves it): a cell
/// whose left edge is exactly one site (ROW 0's site width) from the previous distinct right edge,
/// and whose Y span overlaps a cell at that edge — spans CLOSED at both ends, so abutting rows
/// overlap; or a cell with a blockage one site beyond either edge and none at the edge itself.
/// ⚠️ `lastNode` starts as the segment's first cell, so the first cell never compares.
pub(crate) fn one_site_gap_violations(s: &mut Setup) -> Vec<(usize, usize)> {
    s.resort_segments();
    let one_site = s.arch.rows[0].site_width;
    let overlap = |b1: i32, t1: i32, b2: i32, t2: i32| (b2 <= b1 && b1 <= t2) || (b1 <= b2 && b2 <= t1);
    let mut out = Vec::new();
    for seg in 0..s.segments.len() {
        let cells = &s.cells_in_seg[seg];
        if cells.len() < 2 {
            continue;
        }
        let mut last = cells[0];
        let mut at_last_x = vec![cells[0]];
        for &nd in cells {
            let n = &s.nodes[nd];
            if n.right() != s.nodes[last].right() {
                if (n.left - s.nodes[last].right()).abs() == one_site {
                    for &c in &at_last_x {
                        if !overlap(s.nodes[c].bottom, s.nodes[c].top(), n.bottom, n.top()) {
                            continue;
                        }
                        out.push((seg, n.id));
                        // A fixed/terminal cell (DPL-0339) or a multi-height one (DPL-0340) is
                        // recorded and the scan CONTINUES — once per overlapping cell; the fix
                        // is tried on the first overlap only.
                        if !(n.kind == Kind::Terminal || n.fixed || s.arch.height_in_rows(n) != 1) {
                            break;
                        }
                    }
                }
                at_last_x.clear();
            }
            let blocked = |x| is_inside_a_blockage(s, nd, x);
            if (blocked(n.left - one_site) && !blocked(n.left))
                || (blocked(n.right() + one_site) && !blocked(n.right()))
            {
                out.push((seg, n.id));
            }
            at_last_x.push(nd);
            last = nd;
        }
    }
    out
}

/// `isInsideABlockage` — a blockage covering `x` (ends inclusive) in the rows the cell spans.
/// ⚠️ The row range is `[bottom row, top row)` with the top clamped to `rows - 1`, so a cell in
/// the LAST row checks no row at all.
///
/// ⚠️ **That clamp is UNWITNESSED**: a mutant without it still matches all 9 upstream cases' 8,571
/// detections — none has a blockage one site from a last-row cell. The blockage branch itself IS
/// witnessed (without it aes, gcd and ibex differ), as is the closed-interval overlap.
fn is_inside_a_blockage(s: &Setup, nd: usize, x: i32) -> bool {
    let r0 = &s.arch.rows[0];
    let n = &s.nodes[nd];
    let start = ((n.bottom - r0.bottom) / r0.height).max(0);
    let end = ((n.top() - r0.bottom) / r0.height).min(s.arch.rows.len() as i32 - 1);
    (start..end).any(|r| {
        let row = &s.blockages[r as usize];
        let i = row.partition_point(|b| b.x_max() < x);
        i < row.len() && row[i].blocks() && x >= row[i].x_min() && x <= row[i].x_max()
    })
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

// ── global swap (legacy) ─────────────────────────────────────────────────────────────────────

/// `-p N -t T` with `tol = max(tol, 0.01)`, `passes = max(passes, 1)`.
fn passes_tol(args: &[&str]) -> (i32, f64) {
    let (mut passes, mut tol) = (1i32, 0.01f64);
    let mut i = 1;
    while i < args.len() {
        match args[i] {
            "-p" if i + 1 < args.len() => { i += 1; passes = args[i].parse().unwrap_or(1); }
            "-t" if i + 1 < args.len() => { i += 1; tol = args[i].parse().unwrap_or(0.01); }
            _ => {}
        }
        i += 1;
    }
    (passes.max(1), tol.max(0.01))
}

/// `legacy::DetailedGlobalSwap::run` — passes of [`global_swap_pass`] until the HPWL change is
/// within `tol`.
fn global_swap(s: &mut Setup, args: &[&str]) {
    let (passes, tol) = passes_tol(args);
    let mut curr = s.total_hpwl_xy() as i64;
    let init = curr;
    if init == 0 {
        return;
    }
    for p in 1..=passes {
        let last = curr;
        global_swap_pass(s);
        curr = s.total_hpwl_xy() as i64;
        s.log.push(format!("[INFO DPL-0306] Pass {p:3} of global swaps; hpwl is {}.", sci(curr as f64)));
        if last == 0 || (curr - last).abs() as f64 / last as f64 <= tol {
            break;
        }
    }
    let imp = (init - curr) as f64 / init as f64 * 100.0;
    s.log.push(format!("[INFO DPL-0307] End of global swaps; objective is {}, improvement is {imp:.2} percent.", sci(curr as f64)));
}

/// `globalSwap` — every single-height cell once, in shuffled order, toward its nets' median box.
fn global_swap_pass(s: &mut Setup) {
    s.resort_segments();
    let mut candidates = s.single.clone();
    s.rt.rng.shuffle(&mut candidates);
    let mut obj = Hpwl { edge: vec![0; s.rt.edges.len()], pending: Vec::new() };
    let mut curr = obj.curr(s);
    for ndi in candidates {
        if !gs_generate(s, ndi) {
            continue;
        }
        let next = curr - obj.delta(s);
        if next <= curr {
            obj.accept(s);
            s.accept_move();
            curr = next;
        } else {
            s.reject_move();
        }
    }
}

/// `getRange` — the median box of the cell's nets (each net's box without this cell, turned into
/// a range for the cell centre and clamped to the chip). ⚠️ Nets of MORE than 100 pins are
/// skipped here (`>`), where the HPWL objective skips 100 or more (`>=`).
fn gs_range(s: &Setup, nd: usize) -> Option<(i32, i32, i32, i32)> {
    let a = &s.arch;
    let (xmin, xmax, ymin, ymax) = (a.min_x, a.max_x, a.min_y, a.max_y);
    let (mut xs, mut ys) = (Vec::new(), Vec::new());
    for &p in &s.rt.node_pins[nd] {
        let e = s.rt.pins[p].edge;
        let np = s.rt.edges[e].len();
        if np <= 1 || np > 100 {
            continue;
        }
        // `calculateEdgeBB(ed, nd)`: every pin but those on `nd`.
        let (mut x0, mut y0, mut x1, mut y1) = (i32::MAX, i32::MAX, i32::MIN, i32::MIN);
        let mut count = 0;
        for &q in &s.rt.edges[e] {
            let pin = &s.rt.pins[q];
            if pin.node == nd {
                continue;
            }
            let n = &s.nodes[pin.node];
            let (cx, cy) = (n.left + n.width / 2 + pin.ox, n.bottom + n.height / 2 + pin.oy);
            x0 = x0.min(cx);
            x1 = x1.max(cx);
            y0 = y0.min(cy);
            y1 = y1.max(cy);
            count += 1;
        }
        if count == 0 {
            continue;
        }
        let (ox, oy) = (s.rt.pins[p].ox, s.rt.pins[p].oy);
        xs.push(xmin.max(x0 - ox).min(xmax));
        xs.push(xmax.min(x1 - ox).max(xmin));
        ys.push(ymin.max(y0 - oy).min(ymax));
        ys.push(ymax.min(y1 - oy).max(ymin));
    }
    let t = xs.len();
    if t <= 1 {
        return None;
    }
    let mid = t >> 1;
    xs.sort();
    ys.sort();
    Some((xs[mid - 1], ys[mid - 1], xs[mid], ys[mid]))
}

/// `legacy::DetailedGlobalSwap::generate(ndi)`.
fn gs_generate(s: &mut Setup, ndi: usize) -> bool {
    let n = &s.nodes[ndi];
    let yi = n.bottom as f64 + 0.5 * n.height as f64;
    let xi = n.left as f64 + 0.5 * n.width as f64;
    let Some((mut bx0, mut by0, mut bx1, mut by1)) = gs_range(s, ndi) else { return false };
    if xi >= bx0 as f64 && xi <= bx1 as f64 && yi >= by0 as f64 && yi <= by1 as f64 {
        return false;
    }
    let (dx, dy) = s.rt.max_disp;
    let (lx0, ly0, lx1, ly1) = (n.left - dx, n.bottom - dy, n.left + dx, n.bottom + dy);
    if lx1 <= bx0 {
        bx0 = n.left;
        bx1 = lx1;
    } else if lx0 >= bx1 {
        bx0 = lx0;
        bx1 = n.left;
    } else {
        bx0 = bx0.max(lx0);
        bx1 = bx1.min(lx1);
    }
    if ly1 <= by0 {
        by0 = n.bottom;
        by1 = ly1;
    } else if ly0 >= by1 {
        by0 = ly0;
        by1 = n.bottom;
    } else {
        by0 = by0.max(ly0);
        by1 = by1.min(ly1);
    }
    if s.cell_segs[ndi].len() != 1 {
        return false;
    }
    let si = s.cell_segs[ndi][0];
    let xj = (0.5 * (bx0 + bx1) as f64 - 0.5 * n.width as f64).floor() as i32;
    let yj = (0.5 * (by0 + by1) as f64 - 0.5 * n.height as f64).floor() as i32;
    let rj = s.arch.find_closest_row(yj);
    let yj = s.arch.rows[rj].bottom;
    let Some(sj) = s.segs_in_row[rj].iter().copied()
        .find(|&sg| xj >= s.segments[sg].min_x && xj <= s.segments[sg].max_x) else { return false };
    if s.nodes[ndi].group != s.segments[sj].reg {
        return false;
    }
    let (x0, y0) = (s.nodes[ndi].left, s.nodes[ndi].bottom);
    s.try_move(ndi, x0, y0, si, xj, yj, sj) || s.try_swap(ndi, x0, y0, si, xj, yj, sj)
}

// ── vertical swap ────────────────────────────────────────────────────────────────────────────

/// `DetailedVerticalSwap::run`.
fn vertical_swap(s: &mut Setup, args: &[&str]) {
    let (passes, tol) = passes_tol(args);
    let mut curr = s.total_hpwl_xy() as i64;
    let init = curr;
    if init == 0 {
        return;
    }
    for p in 1..=passes {
        let last = curr;
        vertical_swap_pass(s);
        curr = s.total_hpwl_xy() as i64;
        s.log.push(format!("[INFO DPL-0308] Pass {p:3} of vertical swaps; hpwl is {}.", sci(curr as f64)));
        if last == 0 || (curr - last).abs() as f64 / last as f64 <= tol {
            break;
        }
    }
    let imp = (init - curr) as f64 / init as f64 * 100.0;
    s.log.push(format!("[INFO DPL-0309] End of vertical swaps; objective is {}, improvement is {imp:.2} percent.", sci(curr as f64)));
}

/// `verticalSwap` — as `globalSwap`, each cell once in shuffled order.
fn vertical_swap_pass(s: &mut Setup) {
    s.resort_segments();
    let mut candidates = s.single.clone();
    s.rt.rng.shuffle(&mut candidates);
    let mut obj = Hpwl { edge: vec![0; s.rt.edges.len()], pending: Vec::new() };
    let mut curr = obj.curr(s);
    for ndi in candidates {
        if !vs_generate(s, ndi) {
            continue;
        }
        let next = curr - obj.delta(s);
        if next <= curr {
            obj.accept(s);
            s.accept_move();
            curr = next;
        } else {
            s.reject_move();
        }
    }
}

/// `DetailedVerticalSwap::generate(ndi)` — toward the median box, but only one or two rows up or
/// down (a RANDOM one of the two), at the box's centre x.
fn vs_generate(s: &mut Setup, ndi: usize) -> bool {
    let n = &s.nodes[ndi];
    let yi = n.bottom as f64 + 0.5 * n.height as f64;
    let xi = n.left as f64 + 0.5 * n.width as f64;
    let Some((bx0, by0, bx1, by1)) = gs_range(s, ndi) else { return false };
    if xi >= bx0 as f64 && xi <= bx1 as f64 && yi >= by0 as f64 && yi <= by1 as f64 {
        return false;
    }
    if s.cell_segs[ndi].len() != 1 {
        return false;
    }
    let si = s.cell_segs[ndi][0];
    let ri = s.segments[si].row as i32;
    let xj = (0.5 * (bx0 + bx1) as f64 - 0.5 * n.width as f64).floor() as i32;
    let yj = (0.5 * (by0 + by1) as f64 - 0.5 * n.height as f64).floor() as i32;
    let nrows = s.arch.rows.len() as i32;
    let rj = if yj as f64 > yi {
        let (rmin, rmax) = ((nrows - 1).min(ri + 1), (nrows - 1).min(ri + 2));
        rmin + s.rt.rng.get_random((rmax - rmin + 1) as usize) as i32
    } else {
        let (rmax, rmin) = (0.max(ri - 1), 0.max(ri - 2));
        rmin + s.rt.rng.get_random((rmax - rmin + 1) as usize) as i32
    };
    let rj = rj as usize;
    let yj = s.arch.rows[rj].bottom;
    let Some(sj) = s.segs_in_row[rj].iter().copied()
        .find(|&sg| xj >= s.segments[sg].min_x && xj <= s.segments[sg].max_x) else { return false };
    if s.nodes[ndi].group != s.segments[sj].reg {
        return false;
    }
    let (x0, y0) = (s.nodes[ndi].left, s.nodes[ndi].bottom);
    s.try_move(ndi, x0, y0, si, xj, yj, sj) || s.try_swap(ndi, x0, y0, si, xj, yj, sj)
}

// ── independent set matching ─────────────────────────────────────────────────────────────────

const MIS_MAX_PROBLEM: usize = 25;
const MIS_MAX_TIMES_USED: u32 = 2;
const MIS_SKIP_EDGES_LARGER_THAN: usize = 100;

/// `DetailedMis` state kept across passes.
struct Mis {
    candidates: Vec<usize>,
    colors: Vec<i32>,
    step: (f64, f64),
    dim: (i32, i32),
    /// Buckets `[i][j]`, each the cells whose centre falls in it, in insertion order.
    grid: Vec<Vec<Vec<usize>>>,
    bin_of: std::collections::HashMap<usize, (usize, usize)>,
}

/// `DetailedMis::run` (wirelength objective).
fn mis(s: &mut Setup, args: &[&str]) {
    let (passes, tol) = passes_tol(args);
    s.log.push("[INFO DPL-0300] Set matching objective is wirelength.".into());
    s.resort_segments();
    let mut curr = s.total_hpwl_xy() as i64;
    let init = curr;
    if init == 0 {
        return;
    }
    let mut m = Mis { candidates: s.single.clone(), colors: Vec::new(), step: (0.0, 0.0), dim: (0, 0),
                      grid: Vec::new(), bin_of: Default::default() };
    for mh in s.multi.iter().skip(2) {
        m.candidates.extend(mh.iter().copied());
    }
    if m.candidates.is_empty() {
        s.log.push("[INFO DPL-0202] No movable cells found".into());
        return;
    }
    mis_color(s, &mut m);
    mis_build_grid(s, &mut m);
    for p in 1..=passes {
        s.log.push(format!("[INFO DPL-0301] Pass {p:3} of matching; objective is {}.", sci(curr as f64)));
        mis_place(s, &mut m);
        let last = curr;
        curr = s.total_hpwl_xy() as i64;
        if last == 0 || (curr - last).abs() as f64 / last as f64 <= tol {
            break;
        }
    }
    s.resort_segments();
    let imp = (init - curr) as f64 / init as f64 * 100.0;
    s.log.push(format!("[INFO DPL-0302] End of matching; objective is {}, improvement is {imp:.2} percent.", sci(curr as f64)));
}

/// `colorCells` — `ColorGraph` over every network node, an edge between two movable cells that
/// share a net of 2..=100 pins; greedy colouring in node order (node 0 takes colour 0 whatever
/// it is), smallest colour not used by a coloured neighbour.
fn mis_color(s: &Setup, m: &mut Mis) {
    let nn = s.nodes.len();
    let mut movable = vec![false; nn];
    for &c in &m.candidates {
        movable[c] = true;
    }
    let mut adj: Vec<Vec<usize>> = vec![Vec::new(); nn];
    for e in 0..s.rt.edges.len() {
        let pins = &s.rt.edges[e];
        if pins.len() <= 1 || pins.len() > MIS_SKIP_EDGES_LARGER_THAN {
            continue;
        }
        for (a, &pi) in pins.iter().enumerate() {
            let ni = s.rt.pins[pi].node;
            if !movable[ni] {
                continue;
            }
            for &pj in &pins[a + 1..] {
                let nj = s.rt.pins[pj].node;
                if !movable[nj] || nj == ni {
                    continue;
                }
                adj[ni].push(nj);
                adj[nj].push(ni);
            }
        }
    }
    for a in &mut adj {
        a.sort_unstable();
        a.dedup();
    }
    let mut color = vec![-1i32; nn];
    if nn > 0 {
        color[0] = 0;
    }
    let mut avail = vec![usize::MAX; nn];
    for v in 1..nn {
        for &u in &adj[v] {
            if color[u] != -1 {
                avail[color[u] as usize] = v;
            }
        }
        for cr in 0..nn {
            if avail[cr] != v {
                color[v] = cr as i32;
                break;
            }
        }
    }
    m.colors = (0..nn).map(|i| if movable[i] { color[i] } else { -1 }).collect();
}

/// `buildGrid` — bins of `sqrt(25)` average cells a side.
fn mis_build_grid(s: &Setup, m: &mut Mis) {
    let a = &s.arch;
    let (xmin, xmax, ymin, ymax) = (a.min_x as f64, a.max_x as f64, a.min_y as f64, a.max_y as f64);
    let (mut avg_h, mut avg_w) = (0.0, 0.0);
    for &c in &m.candidates {
        avg_h += s.nodes[c].height as f64;
        avg_w += s.nodes[c].width as f64;
    }
    avg_h /= m.candidates.len() as f64;
    avg_w /= m.candidates.len() as f64;
    let root = (MIS_MAX_PROBLEM as f64).sqrt();
    m.step = (avg_w * root, avg_h * root);
    m.dim = (((xmax - xmin) / m.step.0).ceil() as i32, ((ymax - ymin) / m.step.1).ceil() as i32);
    m.grid = vec![vec![Vec::new(); m.dim.1.max(0) as usize]; m.dim.0.max(0) as usize];
}

/// `place` — repopulate the bins, then each shuffled candidate as a seed (at most twice used).
fn mis_place(s: &mut Setup, m: &mut Mis) {
    for col in &mut m.grid {
        for b in col.iter_mut() {
            b.clear();
        }
    }
    let (xmin, ymin) = (s.arch.min_x as f64, s.arch.min_y as f64);
    m.bin_of.clear();
    for &c in &m.candidates {
        let n = &s.nodes[c];
        let y = n.bottom as f64 + 0.5 * n.height as f64;
        let x = n.left as f64 + 0.5 * n.width as f64;
        let j = (((y - ymin) / m.step.1) as i32).min(m.dim.1 - 1).max(0) as usize;
        let i = (((x - xmin) / m.step.0) as i32).min(m.dim.0 - 1).max(0) as usize;
        m.grid[i][j].push(c);
        m.bin_of.insert(c, (i, j));
    }
    let mut used = vec![0u32; s.nodes.len()];
    s.rt.rng.shuffle(&mut m.candidates);
    for k in 0..m.candidates.len() {
        let ndi = m.candidates[k];
        if used[ndi] >= MIS_MAX_TIMES_USED {
            continue;
        }
        let Some(nbrs) = mis_gather(s, m, ndi) else { continue };
        mis_solve(s, &nbrs);
        for &n in &nbrs {
            used[n] += 1;
        }
    }
}

/// `gatherNeighbours` — breadth-first over bins from the seed's, collecting same-colour,
/// same-size, same-region, power- and row-compatible cells; stop once 25 are held (checked after
/// a whole bin, so it can pass 25).
fn mis_gather(s: &Setup, m: &Mis, ndi: usize) -> Option<Vec<usize>> {
    let &(bi, bj) = m.bin_of.get(&ndi)?;
    let srh = s.arch.rows[0].height as f64;
    let spanned = |n: usize| (s.nodes[n].height as f64 / srh).round() as i64;
    let rails = |n: usize| s.rt.power.master_rails(&s.nodes[n].master);
    let mut out = vec![ndi];
    let mut visited = vec![vec![false; m.dim.1 as usize]; m.dim.0 as usize];
    let mut q = std::collections::VecDeque::new();
    q.push_back((bi, bj));
    while let Some((i, j)) = q.pop_front() {
        if visited[i][j] {
            continue;
        }
        visited[i][j] = true;
        for &ndj in &m.grid[i][j] {
            if ndj == ndi || m.colors[ndi] != m.colors[ndj] {
                continue;
            }
            let (a, b) = (&s.nodes[ndi], &s.nodes[ndj]);
            if a.width != b.width || a.height != b.height || a.group != b.group {
                continue;
            }
            if rails(ndi) != rails(ndj) || spanned(ndi) != spanned(ndj) {
                continue;
            }
            out.push(ndj);
        }
        if out.len() >= MIS_MAX_PROBLEM {
            break;
        }
        if i > 0 {
            q.push_back((i - 1, j));
        }
        if i + 1 < m.dim.0 as usize {
            q.push_back((i + 1, j));
        }
        if j > 0 {
            q.push_back((i, j - 1));
        }
        if j + 1 < m.dim.1 as usize {
            q.push_back((i, j + 1));
        }
    }
    Some(out)
}

/// `getHpwl(ndi, xi, yi)` — the HPWL of `ndi`'s nets (2..=100 pins) with its centre at `(xi, yi)`.
fn mis_hpwl(s: &Setup, ndi: usize, xi: i32, yi: i32) -> u64 {
    let mut total = 0u64;
    for &p in &s.rt.node_pins[ndi] {
        let e = s.rt.pins[p].edge;
        let np = s.rt.edges[e].len();
        if np <= 1 || np > MIS_SKIP_EDGES_LARGER_THAN {
            continue;
        }
        let (mut x0, mut y0, mut x1, mut y1) = (i32::MAX, i32::MAX, i32::MIN, i32::MIN);
        for &q in &s.rt.edges[e] {
            let pin = &s.rt.pins[q];
            let (x, y) = if pin.node == ndi {
                (xi + pin.ox, yi + pin.oy)
            } else {
                let n = &s.nodes[pin.node];
                (n.left + n.width / 2 + pin.ox, n.bottom + n.height / 2 + pin.oy)
            };
            x0 = x0.min(x);
            x1 = x1.max(x);
            y0 = y0.min(y);
            y1 = y1.max(y);
        }
        if x1 >= x0 && y1 >= y0 {
            total += ((x1 - x0) as i64 + (y1 - y0) as i64) as u64;
        }
    }
    total
}

/// `solveMatch` — assign the gathered cells to each other's spots at minimum total HPWL. A cell
/// may always keep its own spot; another spot only within the displacement limit. ⚠️ Upstream
/// solves this with LEMON's `NetworkSimplex`; this is an exact minimum-cost assignment
/// (Hungarian), identical wherever the optimum is unique.
fn mis_solve(s: &mut Setup, nodes: &[usize]) {
    let n = nodes.len();
    if n <= 1 {
        return;
    }
    let pos: Vec<(i32, i32)> = nodes.iter().map(|&c| (s.nodes[c].left, s.nodes[c].bottom)).collect();
    let segs: Vec<Vec<usize>> = nodes.iter().map(|&c| s.cell_segs[c].clone()).collect();
    // The graph exactly as `solveMatch` builds it — node and arc CREATION order decides which of
    // several optimal assignments `NetworkSimplex` returns (see `lemon`).
    let mut g = ListDigraph::new();
    let mut cell_node = Vec::with_capacity(n);
    let mut spot_node = Vec::with_capacity(n);
    for _ in 0..n {
        cell_node.push(g.add_node());
        spot_node.push(g.add_node());
    }
    let supply = g.add_node();
    let demand = g.add_node();
    let mut unit = Vec::new();
    let mut pair = Vec::new();
    for i in 0..n {
        let c = nodes[i];
        unit.push((g.add_arc(supply, cell_node[i]), 0));
        unit.push((g.add_arc(spot_node[i], demand), 0));
        for j in 0..n {
            if i != j {
                let (ox, oy) = s.rt.orig[c];
                if (pos[j].0 - ox).abs() > s.rt.max_disp.0 || (pos[j].1 - oy).abs() > s.rt.max_disp.1 {
                    continue;
                }
            }
            let w = s.nodes[c].width / 2;
            let h = s.nodes[c].height / 2;
            let icost = mis_hpwl(s, c, pos[j].0 + w, pos[j].1 + h) as f64;
            let cost = if icost > i32::MAX as f64 { i32::MAX } else { icost as i32 };
            let a = g.add_arc(cell_node[i], spot_node[j]);
            unit.push((a, cost));
            pair.push((a, i, j));
        }
    }
    let mut ns = NetworkSimplex::new(&g);
    for &(a, c) in &unit {
        ns.set_arc(a, 0, 1, c);
    }
    ns.st_supply(supply, demand, n as i32);
    if !ns.run() {
        return;
    }
    let mut assign: Vec<usize> = (0..n).collect();
    for &(a, i, j) in &pair {
        if ns.flow(a) != 0 {
            assign[i] = j;
        }
    }
    let mut applied: Vec<(usize, (i32, i32), Vec<usize>, (i32, i32), Vec<usize>)> = Vec::new();
    // Applied in the flow map's `ArcIt` order: nodes newest first, so cells from the LAST down.
    for i in (0..n).rev() {
        let j = assign[i];
        if i == j {
            continue;
        }
        let nd = nodes[i];
        for &sg in &segs[i] {
            s.remove_cell_from_segment_pub(nd, sg);
        }
        s.erase_cell(nd);
        s.nodes[nd].left = pos[j].0;
        s.nodes[nd].bottom = pos[j].1;
        s.paint_in_grid(nd);
        for &sg in &segs[j] {
            s.add_cell_to_segment_util(nd, sg);
        }
        applied.push((nd, pos[i], segs[i].clone(), pos[j], segs[j].clone()));
    }
    if nodes.iter().any(|&nd| s.has_placement_violation(nd)) {
        // `journal.undo()` — in reverse.
        for (nd, orig, osegs, _new, nsegs) in applied.into_iter().rev() {
            s.erase_cell(nd);
            for &sg in &nsegs {
                s.remove_cell_from_segment_pub(nd, sg);
            }
            s.nodes[nd].left = orig.0;
            s.nodes[nd].bottom = orig.1;
            s.paint_in_grid_round_pub(nd);
            for &sg in &osegs {
                s.add_cell_to_segment_util(nd, sg);
            }
        }
    }
}

// ── reorder ──────────────────────────────────────────────────────────────────────────────────

/// `std::next_permutation`.
fn next_permutation(v: &mut [usize]) -> bool {
    let n = v.len();
    if n < 2 {
        return false;
    }
    let mut i = n - 1;
    while i > 0 && v[i - 1] >= v[i] {
        i -= 1;
    }
    if i == 0 {
        v.reverse();
        return false;
    }
    let mut j = n - 1;
    while v[j] <= v[i - 1] {
        j -= 1;
    }
    v.swap(i - 1, j);
    v[i..].reverse();
    true
}

/// `DetailedReorderer::run` — windows of 3 single-height cells, every permutation tried.
fn reorder(s: &mut Setup, args: &[&str]) {
    let (passes, tol) = passes_tol(args);
    let window = 3usize.clamp(2, 4); // `-w` absent: 3, then `min(4, max(2, w))`.
    s.resort_segments();
    let mut curr = s.total_hpwl_xy() as i64;
    let init = curr;
    if init == 0 {
        return;
    }
    for p in 1..=passes {
        let last = curr;
        reorder_pass(s, window);
        curr = s.total_hpwl_xy() as i64;
        s.log.push(format!("[INFO DPL-0304] Pass {p:3} of reordering; objective is {}.", sci(curr as f64)));
        if last == 0 || (curr - last).abs() as f64 / last as f64 <= tol {
            break;
        }
    }
    s.resort_segments();
    let imp = (init - curr) as f64 / init as f64 * 100.0;
    s.log.push(format!("[INFO DPL-0305] End of reordering; objective is {}, improvement is {imp:.2} percent.", sci(curr as f64)));
}

/// `reorder()` — per segment, over each run of single-height cells. ⛔ The window loop runs while
/// `i + window <= jstop`, so the run's LAST cell is never in a window — transcribed.
fn reorder_pass(s: &mut Setup, window: usize) {
    let mut mask = vec![0u64; s.rt.edges.len()];
    let mut traversal = 0u64;
    for sg in 0..s.segments.len() {
        if s.cells_in_seg[sg].len() < 2 {
            continue;
        }
        sort_cells_in_seg(s, sg, 0, s.cells_in_seg[sg].len());
        let n = s.cells_in_seg[sg].len() as i64;
        let mut j = 0i64;
        while j < n {
            while j < n && s.arch.height_in_rows(&s.nodes[s.cells_in_seg[sg][j as usize]]) != 1 {
                j += 1;
            }
            let jstrt = j;
            while j < n && s.arch.height_in_rows(&s.nodes[s.cells_in_seg[sg][j as usize]]) == 1 {
                j += 1;
            }
            let jstop = j - 1;
            let mut i = jstrt;
            while i + window as i64 <= jstop {
                let mut istrt = i;
                let istop = jstop.min(istrt + window as i64 - 1);
                if istop == jstop {
                    istrt = jstrt.max(istop - window as i64 + 1);
                }
                let nodes = &s.cells_in_seg[sg];
                let seg = &s.segments[sg];
                let mut right_limit = seg.max_x;
                if istop != n - 1 {
                    let next = nodes[(istop + 1) as usize];
                    right_limit = (s.nodes[next].left - s.rt.pads[next].0).min(right_limit);
                }
                let mut left_limit = seg.min_x;
                if istrt != 0 {
                    let prev = nodes[(istrt - 1) as usize];
                    left_limit = (s.nodes[prev].right() + s.rt.pads[prev].1).max(left_limit);
                }
                reorder_window(s, sg, istrt as usize, istop as usize, left_limit, right_limit, &mut mask, &mut traversal);
                i += 1;
            }
        }
    }
}

/// `sortCellsInSeg(seg, start, end)` — by centre (ties cannot occur between legally placed cells).
fn sort_cells_in_seg(s: &mut Setup, sg: usize, a: usize, b: usize) {
    let nodes = &s.nodes;
    s.cells_in_seg[sg][a..b].sort_by_key(|&c| nodes[c].center_x());
}

/// `DetailedReorderer::cost` — the X-extent of every net on the window's cells, each once; `MAX`
/// if any cell has a placement violation where it now sits.
fn reorder_cost(s: &Setup, cells: &[usize], mask: &mut [u64], traversal: &mut u64) -> f64 {
    *traversal += 1;
    let mut cost = 0.0;
    for &nd in cells {
        if s.has_placement_violation(nd) {
            return f64::MAX;
        }
        for &p in &s.rt.node_pins[nd] {
            let e = s.rt.pins[p].edge;
            let np = s.rt.edges[e].len();
            if np <= 1 || np >= SKIP_NETS_LARGER_THAN || mask[e] == *traversal {
                continue;
            }
            mask[e] = *traversal;
            let (mut x0, mut x1) = (i32::MAX, i32::MIN);
            for &q in &s.rt.edges[e] {
                let pin = &s.rt.pins[q];
                let n = &s.nodes[pin.node];
                let x = n.left + n.width / 2 + pin.ox;
                x0 = x0.min(x);
                x1 = x1.max(x);
            }
            cost += (x1 - x0) as f64;
        }
    }
    cost
}

/// Move `nd` to `x` in place: `eraseFromGrid`, `setLeft`, `paintInGrid`.
fn repaint_at(s: &mut Setup, nd: usize, x: i32) {
    s.erase_cell(nd);
    s.nodes[nd].left = x;
    s.paint_in_grid(nd);
}

/// `DetailedReorderer::reorder(nodes, jstrt, jstop, …)` — spread the window's cells over
/// `[left, right]`, try every order, keep the cheapest, re-align, and restore on any failure.
#[allow(clippy::too_many_arguments)]
fn reorder_window(s: &mut Setup, sg: usize, jstrt: usize, jstop: usize, left_limit: i32, right_limit: i32,
                  mask: &mut [u64], traversal: &mut u64) {
    let size = jstop + 1 - jstrt;
    let cells: Vec<usize> = s.cells_in_seg[sg][jstrt..=jstop].to_vec();
    let orig: Vec<i32> = cells.iter().map(|&c| s.nodes[c].left).collect();
    let (mut left, mut right, mut width) = (vec![0i32; size], vec![0i32; size], vec![0i32; size]);
    let (mut total_pad, mut total_w) = (0i32, 0i32);
    for i in 0..size {
        let c = cells[i];
        left[i] = s.rt.pads[c].0;
        right[i] = s.rt.pads[c].1;
        width[i] = s.nodes[c].width;
        total_pad += left[i] + right[i];
        total_w += width[i];
    }
    let span = right_limit - left_limit;
    if span < total_w + total_pad {
        return;
    }
    let space_per_cell = (span - (total_w + total_pad)) / size as i32;
    let site_w = s.arch.rows[0].site_width;
    let per_total = space_per_cell / site_w;
    let per_right = per_total >> 1;
    let per_left = per_total - per_right;
    for i in 0..size {
        if total_w + total_pad + per_right * site_w < span {
            total_pad += per_right * site_w;
            right[i] += per_right * site_w;
        }
        if total_w + total_pad + per_left * site_w < span {
            total_pad += per_left * site_w;
            left[i] += per_left * site_w;
        }
    }
    if span < total_w + total_pad {
        return;
    }
    let mut best_cost = reorder_cost(s, &cells, mask, traversal);
    let orig_cost = best_cost;
    let (mut best_pos, mut curr_pos) = (vec![0i32; size], vec![0i32; size]);
    let mut order: Vec<usize> = (0..size).collect();
    let mut found = false;
    loop {
        let mut disp_ok = true;
        let mut x = left_limit;
        for i in 0..size {
            let ix = order[i];
            let nd = cells[ix];
            x += left[ix];
            curr_pos[ix] = x;
            repaint_at(s, nd, curr_pos[ix]);
            x += width[ix];
            x += right[ix];
            if (s.nodes[nd].left - s.rt.orig[nd].0).abs() > s.rt.max_disp.0 {
                disp_ok = false;
            }
        }
        if disp_ok {
            let c = reorder_cost(s, &cells, mask, traversal);
            if c < best_cost {
                best_pos = curr_pos.clone();
                best_cost = c;
                found = true;
            }
        }
        if !next_permutation(&mut order) {
            break;
        }
    }
    if !found {
        for i in 0..size {
            repaint_at(s, cells[i], orig[i]);
        }
        return;
    }
    for i in 0..size {
        repaint_at(s, cells[i], best_pos[i]);
    }
    sort_cells_in_seg(s, sg, jstrt, jstop + 1);
    // Re-align, reading the window from the (re-sorted) segment list, as upstream's `nodes` alias.
    let (mut shifted, mut failed) = (false, false);
    let mut left_edge = left_limit;
    for i in 0..size {
        let nd = s.cells_in_seg[sg][jstrt + i];
        let x0 = s.nodes[nd].left;
        let Some(x) = s.align_pos_pub(nd, x0, left_edge, right_limit) else {
            failed = true;
            break;
        };
        if x != x0 {
            shifted = true;
        }
        repaint_at(s, nd, x);
        left_edge = s.nodes[nd].right();
        if (s.nodes[nd].left - s.rt.orig[nd].0).abs() > s.rt.max_disp.0 {
            failed = true;
            break;
        }
    }
    let window: Vec<usize> = s.cells_in_seg[sg][jstrt..=jstop].to_vec();
    if !failed && shifted && reorder_cost(s, &window, mask, traversal) >= orig_cost {
        failed = true;
    }
    if !failed && window.iter().any(|&nd| s.has_placement_violation(nd)) {
        failed = true;
    }
    if failed {
        // `origLeft[ndi]` — by cell, whatever the list order now is.
        for &nd in &window {
            let k = cells.iter().position(|&c| c == nd).unwrap();
            repaint_at(s, nd, orig[k]);
        }
        sort_cells_in_seg(s, sg, jstrt, jstop + 1);
    }
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
