//! Uniform spatial hash for nearest-vertex donor lookup.
//!
//! Mirrors `_spatial_cell_key`, `_build_spatial_hash`, `_nearest_point_index`,
//! `_nearby_point_indices`, `_percentile` from `core/mesh_importer.py`.
//!
//! Cell size derivation: `extent / max(round(N^(1/3)), 1)`, lower-bound 1e-5.
//! This produces ~one vertex per cell on average for uniformly-distributed
//! meshes, which is what makes the radius-expanding search O(1) amortized.
//! Identical math as the reference is required so donor-index lists match
//! the Python output downstream.

use std::collections::HashMap;

pub type CellKey = (i64, i64, i64);
pub type Grid = HashMap<CellKey, Vec<usize>>;

#[inline]
pub fn cell_key(point: [f32; 3], cell_size: f32) -> CellKey {
    (
        (point[0] / cell_size).floor() as i64,
        (point[1] / cell_size).floor() as i64,
        (point[2] / cell_size).floor() as i64,
    )
}

/// Build a spatial hash. Returns the chosen cell size and the grid.
///
/// For an empty input list the cell size defaults to 1.0 (matches Python).
pub fn build_spatial_hash(points: &[[f32; 3]]) -> (f32, Grid) {
    if points.is_empty() {
        return (1.0, Grid::new());
    }
    let mut min = [f32::INFINITY; 3];
    let mut max = [f32::NEG_INFINITY; 3];
    for p in points {
        for i in 0..3 {
            if p[i] < min[i] { min[i] = p[i]; }
            if p[i] > max[i] { max[i] = p[i]; }
        }
    }
    let extent = (max[0] - min[0])
        .max(max[1] - min[1])
        .max(max[2] - min[2])
        .max(1e-5);
    // round-half-to-even; on tied values f64::round_ties_even matches Python's round()
    let cube_root = (points.len() as f64).powf(1.0 / 3.0).round_ties_even() as f32;
    let denom = cube_root.max(1.0);
    let cell_size = (extent / denom).max(1e-5);

    let mut grid: Grid = HashMap::new();
    for (idx, p) in points.iter().enumerate() {
        grid.entry(cell_key(*p, cell_size)).or_default().push(idx);
    }
    (cell_size, grid)
}

/// Nearest-point search. Expands the cell radius shell-by-shell up to 7;
/// if nothing found, falls back to a brute scan over `source_points`.
///
/// Panics on empty `source_points` (same contract as the Python ValueError).
pub fn nearest_point_index(
    point: [f32; 3],
    source_points: &[[f32; 3]],
    cell_size: f32,
    grid: &Grid,
) -> usize {
    assert!(!source_points.is_empty(), "Cannot transfer displacement from an empty source mesh.");

    let base = cell_key(point, cell_size);
    let mut best_idx: Option<usize> = None;
    let mut best_d2 = f32::INFINITY;

    for radius in 0..8 {
        let mut found_any = false;
        for dx in -radius..=radius {
            for dy in -radius..=radius {
                for dz in -radius..=radius {
                    let cell = (base.0 + dx, base.1 + dy, base.2 + dz);
                    if let Some(bucket) = grid.get(&cell) {
                        for &idx in bucket {
                            found_any = true;
                            let s = source_points[idx];
                            let d2 = (s[0] - point[0]).powi(2)
                                   + (s[1] - point[1]).powi(2)
                                   + (s[2] - point[2]).powi(2);
                            if d2 < best_d2 {
                                best_d2 = d2;
                                best_idx = Some(idx);
                            }
                        }
                    }
                }
            }
        }
        if found_any && best_idx.is_some() {
            return best_idx.unwrap();
        }
    }

    // Fallback brute scan -- happens when grid bounds don't reach `point`
    // within 7 cells (e.g. translated geometry).
    for (idx, s) in source_points.iter().enumerate() {
        let d2 = (s[0] - point[0]).powi(2)
               + (s[1] - point[1]).powi(2)
               + (s[2] - point[2]).powi(2);
        if d2 < best_d2 {
            best_d2 = d2;
            best_idx = Some(idx);
        }
    }
    best_idx.expect("source_points was non-empty so brute scan must hit")
}

/// Indices of every source point within `radius` Euclidean distance of `point`.
///
/// Empty source returns empty result (does not panic).
pub fn nearby_point_indices(
    point: [f32; 3],
    source_points: &[[f32; 3]],
    cell_size: f32,
    grid: &Grid,
    radius: f32,
) -> Vec<usize> {
    if source_points.is_empty() {
        return vec![];
    }
    let base = cell_key(point, cell_size);
    let cell_radius = ((radius / cell_size.max(1e-6)).ceil() as i64).max(1);
    let radius_sq = radius * radius;
    let mut candidates = Vec::new();
    for dx in -cell_radius..=cell_radius {
        for dy in -cell_radius..=cell_radius {
            for dz in -cell_radius..=cell_radius {
                let cell = (base.0 + dx, base.1 + dy, base.2 + dz);
                if let Some(bucket) = grid.get(&cell) {
                    for &idx in bucket {
                        let s = source_points[idx];
                        let d2 = (s[0] - point[0]).powi(2)
                               + (s[1] - point[1]).powi(2)
                               + (s[2] - point[2]).powi(2);
                        if d2 <= radius_sq {
                            candidates.push(idx);
                        }
                    }
                }
            }
        }
    }
    candidates
}

/// Simple sorted-array percentile. Empty input returns 0.0 (matches Python).
/// `pct` is clamped to [0, 1].
pub fn percentile(values: &[f32], pct: f32) -> f32 {
    if values.is_empty() {
        return 0.0;
    }
    let clamped = pct.clamp(0.0, 1.0);
    let mut ordered = values.to_vec();
    ordered.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let idx = (((ordered.len() - 1) as f32) * clamped).round_ties_even() as usize;
    ordered[idx]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input_returns_unit_cell() {
        let (cs, g) = build_spatial_hash(&[]);
        assert_eq!(cs, 1.0);
        assert!(g.is_empty());
    }

    #[test]
    fn cell_size_scales_with_extent() {
        let pts: Vec<[f32; 3]> = (0..125).map(|i| [i as f32, 0.0, 0.0]).collect();
        let (cs, _g) = build_spatial_hash(&pts);
        // extent = 124, cube_root(125) = 5, cs = 124/5 = 24.8
        assert!((cs - 24.8).abs() < 1e-4, "got {cs}");
    }

    #[test]
    fn nearest_point_finds_self() {
        let pts = vec![[0.0, 0.0, 0.0], [10.0, 0.0, 0.0], [0.0, 10.0, 0.0]];
        let (cs, g) = build_spatial_hash(&pts);
        for (i, p) in pts.iter().enumerate() {
            assert_eq!(nearest_point_index(*p, &pts, cs, &g), i);
        }
    }

    #[test]
    fn nearest_point_handles_far_query() {
        // Query far outside the grid forces fallback brute scan.
        let pts = vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [2.0, 0.0, 0.0]];
        let (cs, g) = build_spatial_hash(&pts);
        let nearest = nearest_point_index([1000.0, 0.0, 0.0], &pts, cs, &g);
        assert_eq!(nearest, 2);
    }

    #[test]
    #[should_panic(expected = "empty source mesh")]
    fn nearest_point_panics_on_empty_source() {
        let g = Grid::new();
        nearest_point_index([0.0, 0.0, 0.0], &[], 1.0, &g);
    }

    #[test]
    fn nearby_finds_within_radius() {
        let pts = vec![
            [0.0, 0.0, 0.0],
            [0.5, 0.0, 0.0],
            [2.0, 0.0, 0.0],
            [0.0, 0.0, 0.7],
        ];
        let (cs, g) = build_spatial_hash(&pts);
        let mut got = nearby_point_indices([0.0, 0.0, 0.0], &pts, cs, &g, 0.6);
        got.sort();
        assert_eq!(got, vec![0, 1]);
        let mut got = nearby_point_indices([0.0, 0.0, 0.0], &pts, cs, &g, 1.0);
        got.sort();
        assert_eq!(got, vec![0, 1, 3]);
    }

    #[test]
    fn percentile_basic() {
        assert_eq!(percentile(&[], 0.5), 0.0);
        assert_eq!(percentile(&[1.0, 2.0, 3.0, 4.0, 5.0], 0.0), 1.0);
        assert_eq!(percentile(&[1.0, 2.0, 3.0, 4.0, 5.0], 1.0), 5.0);
        // 0.5 * (5-1) = 2 -> index 2 -> value 3.0
        assert_eq!(percentile(&[1.0, 2.0, 3.0, 4.0, 5.0], 0.5), 3.0);
    }

    #[test]
    fn percentile_clamps_pct() {
        assert_eq!(percentile(&[1.0, 2.0, 3.0], -0.5), 1.0);
        assert_eq!(percentile(&[1.0, 2.0, 3.0], 1.5), 3.0);
    }
}
