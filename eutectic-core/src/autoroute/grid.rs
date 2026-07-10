//! The routing grid: the [`Grid`] discretisation of the board over all copper
//! layers, and the [`routing_area`] choice that sizes it.

use crate::doc::{Nm, Point};
use crate::solve::Rect;
use std::collections::BTreeMap;

use super::ingest::Pad;

/// Choose the routing area: the board outline's bounding box if a `Board` is
/// declared, else the bounding box of every pad, padded by two grid pitches so edge
/// pins have room. The grid spans the bbox; the [`BoardMask`](super::obstacles::BoardMask)
/// then carves it to the real (non-rectangular, cutout-holed) outline.
pub(super) fn routing_area(
    doc: &crate::doc::Doc,
    net_pads: &BTreeMap<crate::id::NetId, Vec<Pad>>,
    pitch: Nm,
) -> Option<Rect> {
    if let Some(region) = crate::elaborate::board_region(&doc.source)
        && let Some((min, max)) = region.bbox()
    {
        return Some(Rect { min, max });
    }
    let mut it = net_pads.values().flatten().map(|p| p.at);
    let first = it.next()?;
    let (mut min, mut max) = (first, first);
    for p in net_pads.values().flatten().map(|p| p.at) {
        min.x = min.x.min(p.x);
        min.y = min.y.min(p.y);
        max.x = max.x.max(p.x);
        max.y = max.y.max(p.y);
    }
    let m = 2 * pitch;
    Some(Rect {
        min: Point {
            x: min.x - m,
            y: min.y - m,
        },
        max: Point {
            x: max.x + m,
            y: max.y + m,
        },
    })
}

pub(super) struct Grid {
    pub(super) origin: Point,
    pub(super) pitch: Nm,
    pub(super) cols: usize,
    pub(super) rows: usize,
    pub(super) layers: usize,
}

impl Grid {
    pub(super) fn new(area: Rect, pitch: Nm, layers: usize) -> Grid {
        let cols = ((area.max.x - area.min.x) / pitch).max(0) as usize + 1;
        let rows = ((area.max.y - area.min.y) / pitch).max(0) as usize + 1;
        Grid {
            origin: area.min,
            pitch,
            cols,
            rows,
            layers,
        }
    }
    pub(super) fn world(&self, i: usize, j: usize) -> Point {
        Point {
            x: self.origin.x + i as Nm * self.pitch,
            y: self.origin.y + j as Nm * self.pitch,
        }
    }
    pub(super) fn idx(&self, i: usize, j: usize) -> usize {
        j * self.cols + i
    }
    pub(super) fn cells(&self) -> usize {
        self.cols * self.rows
    }
    /// Flat index into a `cells * layers` array (layer-minor).
    pub(super) fn lidx(&self, i: usize, j: usize, l: usize) -> usize {
        self.idx(i, j) * self.layers + l
    }
    /// The inclusive cell index box covering world bbox `(lo, hi)` grown by `margin`,
    /// clamped to the grid — the scan window for stamping one obstacle.
    pub(super) fn bbox_range(
        &self,
        lo: Point,
        hi: Point,
        margin: Nm,
    ) -> (usize, usize, usize, usize) {
        let clampi =
            |v: Nm| ((v - self.origin.x) / self.pitch).clamp(0, self.cols as Nm - 1) as usize;
        let clampj =
            |v: Nm| ((v - self.origin.y) / self.pitch).clamp(0, self.rows as Nm - 1) as usize;
        // ±one cell of slop is fine — the exact distance test inside decides membership.
        (
            clampi(lo.x - margin - self.pitch),
            clampi(hi.x + margin + self.pitch),
            clampj(lo.y - margin - self.pitch),
            clampj(hi.y + margin + self.pitch),
        )
    }
}
