//! Donor-vertex matching for the PAC rebuilder.
//!
//! When a user edits a PAC mesh in Blender and round-trips back through OBJ,
//! the OBJ file loses bone bindings, packed normals, and the engine's
//! per-vertex shading bytes. We need a *donor* original vertex for each new
//! vertex so we can clone its record before overwriting position/UV/normal.
//!
//! Two-tier match:
//!   1. Exact lookup by position rounded to 1e-5 (catches the common
//!      "user moved 5 verts out of 20k" case).
//!   2. Spatial hash + shell-expanding nearest-neighbor for the rest.
//!
//! Mirrors `_choose_pac_donor_indices` from `core/mesh_importer.py`.
//! Static (PAM) donor matching is a separate problem (sequence alignment
//! against the original vertex order, not spatial); it lives in TODO once
//! stage 4 (PAM builder) is wired.

use std::collections::HashMap;

use crate::repack::mesh::SubMesh;

/// Pick the original-submesh vertex index whose record each new-submesh
/// vertex should be cloned from.
///
/// Returns a vec of length `new_sm.vertices.len()`. If the original mesh is
/// empty, returns all zeros (caller still gets a slot to write into).
pub fn choose_pac_donor_indices(orig_sm: &SubMesh, new_sm: &SubMesh) -> Vec<usize> {
    let n_orig = orig_sm.vertices.len();
    let n_new = new_sm.vertices.len();
    if n_orig == 0 {
        return vec![0; n_new];
    }

    // Exact-match table keyed on positions rounded to 1e-5 metres. First
    // writer wins to mirror the Python `setdefault` semantics.
    let mut exact_map: HashMap<(i64, i64, i64), usize> = HashMap::with_capacity(n_orig);
    for (i, p) in orig_sm.vertices.iter().enumerate() {
        let key = quantize_key(*p);
        exact_map.entry(key).or_insert(i);
    }

    // Below ~64 verts the spatial-hash overhead is not worth the constant-factor
    // win, so the Python falls back to a linear scan. Mirror that exactly.
    if n_orig <= 64 {
        let mut donor_indices = Vec::with_capacity(n_new);
        for new_pos in &new_sm.vertices {
            let key = quantize_key(*new_pos);
            if let Some(&exact) = exact_map.get(&key) {
                donor_indices.push(exact);
                continue;
            }
            let mut best_idx = 0usize;
            let mut best_dist = f32::INFINITY;
            for (orig_idx, op) in orig_sm.vertices.iter().enumerate() {
                let dx = new_pos[0] - op[0];
                let dy = new_pos[1] - op[1];
                let dz = new_pos[2] - op[2];
                let d2 = dx * dx + dy * dy + dz * dz;
                if d2 < best_dist {
                    best_dist = d2;
                    best_idx = orig_idx;
                }
            }
            donor_indices.push(best_idx);
        }
        return donor_indices;
    }

    // Build a uniform spatial hash sized for ~8 verts per cell at equilibrium.
    let mut min = orig_sm.vertices[0];
    let mut max = orig_sm.vertices[0];
    for v in &orig_sm.vertices {
        for i in 0..3 {
            if v[i] < min[i] { min[i] = v[i]; }
            if v[i] > max[i] { max[i] = v[i]; }
        }
    }
    let extent = (max[0] - min[0])
        .max(max[1] - min[1])
        .max(max[2] - min[2])
        .max(1e-6);
    let target_cells_per_axis: i64 = ((n_orig as f64 / 8.0)
        .powf(1.0 / 3.0)
        .round_ties_even() as i64)
        .max(2);
    let mut cell_size = extent / (target_cells_per_axis as f32);
    if cell_size < 1e-6 {
        cell_size = 1e-6;
    }
    let inv_cell = 1.0 / cell_size;

    let cell_key = |x: f32, y: f32, z: f32| -> (i64, i64, i64) {
        // Mirror Python's `int(...)` (truncation toward zero). Since the
        // expression is non-negative for points inside the bbox the result
        // matches a floor; for points outside the bbox the truncation is
        // what the reference does, so keep it as-is.
        (
            ((x - min[0]) * inv_cell) as i64,
            ((y - min[1]) * inv_cell) as i64,
            ((z - min[2]) * inv_cell) as i64,
        )
    };

    let mut grid: HashMap<(i64, i64, i64), Vec<usize>> = HashMap::new();
    for (orig_idx, v) in orig_sm.vertices.iter().enumerate() {
        grid.entry(cell_key(v[0], v[1], v[2])).or_default().push(orig_idx);
    }

    let max_shell = target_cells_per_axis + 1;
    let mut donor_indices = Vec::with_capacity(n_new);

    for new_pos in &new_sm.vertices {
        let key = quantize_key(*new_pos);
        if let Some(&exact) = exact_map.get(&key) {
            donor_indices.push(exact);
            continue;
        }

        let (cx, cy, cz) = cell_key(new_pos[0], new_pos[1], new_pos[2]);
        let mut best_idx = 0usize;
        let mut best_dist = f32::INFINITY;
        let mut shell: i64 = 0;

        while shell <= max_shell {
            let lo = cx - shell;
            let hi = cx + shell;
            for ix in lo..=hi {
                for iy in (cy - shell)..=(cy + shell) {
                    for iz in (cz - shell)..=(cz + shell) {
                        // For shells > 0 only scan the cube surface; interior
                        // cells were covered by previous shells.
                        if shell > 0
                            && lo < ix && ix < hi
                            && (cy - shell) < iy && iy < (cy + shell)
                            && (cz - shell) < iz && iz < (cz + shell)
                        {
                            continue;
                        }
                        if let Some(bucket) = grid.get(&(ix, iy, iz)) {
                            for &orig_idx in bucket {
                                let op = orig_sm.vertices[orig_idx];
                                let dx = new_pos[0] - op[0];
                                let dy = new_pos[1] - op[1];
                                let dz = new_pos[2] - op[2];
                                let d2 = dx * dx + dy * dy + dz * dz;
                                if d2 < best_dist {
                                    best_dist = d2;
                                    best_idx = orig_idx;
                                }
                            }
                        }
                    }
                }
            }
            // Termination: if the best donor so far is closer than the nearest
            // possible point in the next shell, we are done.
            if best_dist.is_finite() {
                let shell_dist = (shell as f32) * cell_size;
                if shell_dist * shell_dist > best_dist {
                    break;
                }
            }
            shell += 1;
        }
        donor_indices.push(best_idx);
    }

    donor_indices
}

#[inline]
fn quantize_key(p: [f32; 3]) -> (i64, i64, i64) {
    // round-half-to-even; Python's `round()` is banker's rounding too.
    let f = |v: f32| (v * 100_000.0).round_ties_even() as i64;
    (f(p[0]), f(p[1]), f(p[2]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sm_from(positions: &[[f32; 3]]) -> SubMesh {
        SubMesh {
            vertices: positions.to_vec(),
            ..Default::default()
        }
    }

    #[test]
    fn empty_orig_returns_zeros() {
        let orig = sm_from(&[]);
        let new = sm_from(&[[0.0, 0.0, 0.0], [1.0, 1.0, 1.0]]);
        assert_eq!(choose_pac_donor_indices(&orig, &new), vec![0, 0]);
    }

    #[test]
    fn exact_match_via_quantized_key() {
        // Identical positions return identical donor indices regardless of size.
        let positions: Vec<[f32; 3]> = (0..100).map(|i| [i as f32, 0.0, 0.0]).collect();
        let orig = sm_from(&positions);
        let new = sm_from(&positions);
        let donors = choose_pac_donor_indices(&orig, &new);
        for (i, &d) in donors.iter().enumerate() {
            assert_eq!(d, i, "vertex {i} should be its own donor");
        }
    }

    #[test]
    fn small_mesh_uses_linear_scan() {
        // <= 64 verts triggers the small-mesh branch; correctness same as spatial.
        let orig = sm_from(&[[0.0, 0.0, 0.0], [10.0, 0.0, 0.0], [0.0, 10.0, 0.0]]);
        let new = sm_from(&[[0.1, 0.1, 0.0], [9.0, 0.1, 0.0], [0.1, 9.0, 0.1]]);
        let donors = choose_pac_donor_indices(&orig, &new);
        assert_eq!(donors, vec![0, 1, 2]);
    }

    #[test]
    fn large_mesh_uses_spatial_hash() {
        // 200 verts on a 1D line; query nearest for a few off-grid points.
        let positions: Vec<[f32; 3]> = (0..200).map(|i| [i as f32, 0.0, 0.0]).collect();
        let orig = sm_from(&positions);
        let new = sm_from(&[[0.4, 0.0, 0.0], [50.6, 0.0, 0.0], [199.0, 0.0, 0.0], [-100.0, 0.0, 0.0]]);
        let donors = choose_pac_donor_indices(&orig, &new);
        assert_eq!(donors[0], 0, "0.4 -> nearest is 0");
        assert_eq!(donors[1], 51, "50.6 -> nearest is 51");
        assert_eq!(donors[2], 199, "199.0 -> exact match");
        assert_eq!(donors[3], 0, "far query falls back through shells");
    }

    #[test]
    fn moved_vertex_picks_nearest_unmoved_neighbor() {
        // 100-vert orig grid; new mesh = orig with vert 50 displaced by epsilon.
        // Donor for the displaced vertex should still be 50 (exact-key falls
        // through to nearest, and nearest is itself before displacement).
        let mut positions: Vec<[f32; 3]> = (0..100)
            .map(|i| [(i % 10) as f32, (i / 10) as f32, 0.0])
            .collect();
        let orig = sm_from(&positions);
        positions[50] = [positions[50][0] + 0.01, positions[50][1] + 0.01, 0.0];
        let new = sm_from(&positions);
        let donors = choose_pac_donor_indices(&orig, &new);
        assert_eq!(donors[50], 50);
        // All others stay exact.
        for (i, &d) in donors.iter().enumerate() {
            if i != 50 {
                assert_eq!(d, i, "vertex {i} should be its own donor");
            }
        }
    }
}
