// SPDX-License-Identifier: Apache-2.0
//! The power-via-metal half of `checkBlockedLayers`, shared by every caller: `check_placement`, the
//! negotiation legalizer, `improve_placement` and `optimize_mirroring`.
//!
//! Upstream: `761784900` (via stacks and DRCFILL patches outside the stripes block cells with a pin
//! on that layer), `f4798386d5` (pin layers from every signal terminal, connected or not) and
//! `fb9d98e5fe` (the per-square mask becomes a coarse filter; the exact test places each pin shape
//! at the candidate position and orientation and asks whether it touches the grown metal).
//!
//! The rules themselves are pure functions in [`crate::drc`]; this is the per-master model and the
//! one place that binds them to a [`Grid`].

use std::collections::HashMap;
use std::rc::Rc;

use vyges_opendb::Db;

use crate::drc::EdgeBox;
use crate::grid::Grid;

/// One master's pin shapes per routing level (master coordinates), its placement boundary (what
/// `transformEdgeRect` rotates to find the offset) and the `pin_layers` mask they imply.
pub struct PinModel {
    /// Per level: each shape and the index into [`PinModel::terms`] of its terminal.
    pub shapes: Vec<Vec<(EdgeBox, usize)>>,
    pub boundary: EdgeBox,
    pub pin_layers: u32,
    /// The master's terminals, in `master_mterms` order.
    pub terms: Vec<String>,
}

impl PinModel {
    /// The exact test for one level: a pin shape, placed with the boundary's lower-left at
    /// `(x, y)` (core-relative DBU) in `orient`, touching the grid's blockage on that level.
    ///
    /// 🔑 `nets[k]` is the instance's net on terminal `k` (`""` for none): "a pin may touch power
    /// metal of the net it is connected to" (upstream `102fa081cbd`, at `4155ac3`).
    #[allow(clippy::too_many_arguments)]
    pub fn hits(&self, grid: &Grid, level: usize, orient: &str, x: i32, y: i32, nets: &[String]) -> bool {
        self.shapes[level].iter().any(|&(s, k)| {
            let net = nets.get(k).map_or("", String::as_str);
            grid.overlaps_pin_blockage(level, crate::drc::transform_edge_rect(s, self.boundary, orient, x, y), net)
        })
    }

    /// An instance's net per terminal of this model, `""` where it has none.
    pub fn nets_of(&self, db: &Db, inst: &str) -> Vec<String> {
        let by_term: HashMap<String, String> = db.inst_iterm_info(inst).unwrap_or_default()
            .into_iter().map(|(t, _, n)| (t, n)).collect();
        self.terms.iter().map(|t| by_term.get(t).cloned().unwrap_or_default()).collect()
    }

    /// `PlacementDRC::checkBlockedLayers(cell, x, y, orient)` — both stages, at grid square `(gx, gy)`
    /// over `[gx, gx_end) × [gy, gy_end)`, the cell's `used_layers` given.
    #[allow(clippy::too_many_arguments)]
    pub fn check(&self, grid: &Grid, used_layers: u32, gx: i32, gx_end: i32, gy: i32, gy_end: i32, orient: &str,
                 nets: &[String]) -> bool {
        let (x, y) = (gx * grid.site_width, grid.row_y.get(gy as usize).copied().unwrap_or(0));
        crate::drc::check_blocked_layers(gx, gx_end, gy, gy_end, used_layers, self.pin_layers,
            &|px, py| grid.pixel(px as i64, py as i64).map(|p| (p.blocked_layers, p.blocked_pin_layers)),
            &|l| self.hits(grid, l, orient, x, y, nets))
    }
}

/// The models, built once per master (`Master::getPinShapes` is collected once per master).
pub struct PinModels<'a> {
    levels: &'a HashMap<String, i32>,
    cache: HashMap<String, Rc<PinModel>>,
}

impl<'a> PinModels<'a> {
    pub fn new(levels: &'a HashMap<String, i32>) -> Self {
        PinModels { levels, cache: HashMap::new() }
    }

    pub fn of(&mut self, db: &Db, master: &str) -> Rc<PinModel> {
        let levels = self.levels;
        Rc::clone(self.cache.entry(master.to_string()).or_insert_with(|| {
            let terms = db.master_mterms(master).unwrap_or_default();
            let (shapes, pin_layers) = crate::drc::master_pin_shapes(&terms, &|term| {
                db.mterm_pin_boxes(master, term).unwrap_or_default().into_iter()
                    .filter_map(|(ln, x0, y0, x1, y1)| levels.get(&db.layer_name_by_number(ln)).map(|&l| (l, (x0, y0, x1, y1))))
                    .collect()
            });
            let b = db.master_placement_boundary(master).unwrap_or_default();
            let boundary = if b.len() == 4 { (b[0], b[1], b[2], b[3]) } else { (0, 0, 0, 0) };
            Rc::new(PinModel { shapes, boundary, pin_layers, terms: terms.into_iter().map(|(t, _)| t).collect() })
        }))
    }
}
