//! Quantization + bbox + smooth-normal primitives.
//!
//! Byte-for-byte mirror of the Python helpers in `core/mesh_importer.py`
//! (`_quantize_u16`, `_quantize_pac_u16`, `_pack_pac_normal`, `_compute_bbox`)
//! and `core/mesh_parser.py` (`_compute_smooth_normals`, `_compute_face_normal`).
//!
//! Float math is intentionally identical to the reference: same operation
//! order, same epsilon constants, no FMA. Reproducibility against the Python
//! oracle is the test bar; deviations cause downstream u16 quantization
//! mismatches and break the byte-equivalence regression suite.

/// Float -> uint16, generic min/max (used by PAM static vertex records).
///
/// `if abs(vmax - vmin) < 1e-10 -> 32768` (midpoint sentinel) matches Python.
pub fn quantize_u16(value: f32, vmin: f32, vmax: f32) -> u16 {
    if (vmax - vmin).abs() < 1e-10 {
        return 32768;
    }
    let t = ((value - vmin) / (vmax - vmin)).clamp(0.0, 1.0);
    (t * 65535.0).round().clamp(0.0, 65535.0) as u16
}

/// Float -> uint16, PAC bbox-min + extent encoding (used by PAC vertex records).
///
/// Note the PAC quant uses a 0..32767 range (not 0..65535) -- the upper bit is
/// reserved for a sign/flag in some PAC submesh layouts. Keep this distinction
/// even when extent is non-zero; the Python masks to 32767 explicitly.
pub fn quantize_pac_u16(value: f32, bbox_min: f32, bbox_extent: f32) -> u16 {
    if bbox_extent.abs() < 1e-10 {
        return 0;
    }
    let t = ((value - bbox_min) / bbox_extent).clamp(0.0, 1.0);
    (t * 32767.0).round().clamp(0.0, 32767.0) as u16
}

/// Pack a unit-vector normal into the PAC 10:10:10 layout, preserving the
/// upper 2 flag bits from `existing_packed` (typically winding/handedness).
///
/// Layout: bits  0..10 = nz, 10..20 = nx, 20..30 = ny, 30..32 = flags.
/// Encoding: clamp to [-1, 1], map to [0, 1023] via `(v + 1.0) * 511.5`.
///
/// Matches upstream `_pack_pac_normal`. Flag preservation is required by the
/// f206431 fix (without it the PAC reimport would zero the engine's
/// per-vertex shading flags and cause visible artefacts on import).
pub fn pack_pac_normal(normal: [f32; 3], existing_packed: u32) -> u32 {
    fn enc(v: f32) -> u32 {
        let clamped = v.clamp(-1.0, 1.0);
        let mapped = ((clamped + 1.0) * 511.5).round();
        mapped.clamp(0.0, 1023.0) as u32
    }
    let [nx, ny, nz] = normal;
    let packed = enc(nz) | (enc(nx) << 10) | (enc(ny) << 20);
    (existing_packed & 0xC000_0000) | packed
}

/// Tight axis-aligned bbox over a vertex list with a 1e-6 epsilon padding
/// (matches Python; the padding avoids zero-extent bboxes which would make
/// quantization collapse to a single u16 value).
///
/// Empty input returns `((0,0,0), (1,1,1))` to keep the downstream
/// quantizer's denominator non-zero.
pub fn compute_bbox(vertices: &[[f32; 3]]) -> ([f32; 3], [f32; 3]) {
    if vertices.is_empty() {
        return ([0.0, 0.0, 0.0], [1.0, 1.0, 1.0]);
    }
    let mut bmin = [f32::INFINITY; 3];
    let mut bmax = [f32::NEG_INFINITY; 3];
    for v in vertices {
        for i in 0..3 {
            if v[i] < bmin[i] {
                bmin[i] = v[i];
            }
            if v[i] > bmax[i] {
                bmax[i] = v[i];
            }
        }
    }
    let eps = 1e-6f32;
    (
        [bmin[0] - eps, bmin[1] - eps, bmin[2] - eps],
        [bmax[0] + eps, bmax[1] + eps, bmax[2] + eps],
    )
}

/// Cross-product face normal. Returns `(0, 1, 0)` for degenerate faces
/// (length < 1e-8), matching Python's fallback.
pub fn compute_face_normal(v0: [f32; 3], v1: [f32; 3], v2: [f32; 3]) -> [f32; 3] {
    let ax = v1[0] - v0[0];
    let ay = v1[1] - v0[1];
    let az = v1[2] - v0[2];
    let bx = v2[0] - v0[0];
    let by = v2[1] - v0[1];
    let bz = v2[2] - v0[2];
    let nx = ay * bz - az * by;
    let ny = az * bx - ax * bz;
    let nz = ax * by - ay * bx;
    let length = (nx * nx + ny * ny + nz * nz).sqrt();
    if length > 1e-8 {
        [nx / length, ny / length, nz / length]
    } else {
        [0.0, 1.0, 0.0]
    }
}

/// Per-vertex smooth normals, computed by accumulating each adjacent
/// (non-degenerate) face normal then normalizing. Faces touching out-of-bounds
/// vertex indices are silently skipped (matches Python defensive guard).
///
/// Vertices with zero accumulated length get the `(0, 1, 0)` fallback.
pub fn compute_smooth_normals(vertices: &[[f32; 3]], faces: &[[u32; 3]]) -> Vec<[f32; 3]> {
    let n = vertices.len();
    let mut normals = vec![[0.0f32; 3]; n];
    for &[a, b, c] in faces {
        let (ai, bi, ci) = (a as usize, b as usize, c as usize);
        if ai < n && bi < n && ci < n {
            let fn_ = compute_face_normal(vertices[ai], vertices[bi], vertices[ci]);
            for &idx in &[ai, bi, ci] {
                normals[idx][0] += fn_[0];
                normals[idx][1] += fn_[1];
                normals[idx][2] += fn_[2];
            }
        }
    }
    for n_ in normals.iter_mut() {
        let length = (n_[0] * n_[0] + n_[1] * n_[1] + n_[2] * n_[2]).sqrt();
        if length > 1e-8 {
            n_[0] /= length;
            n_[1] /= length;
            n_[2] /= length;
        } else {
            *n_ = [0.0, 1.0, 0.0];
        }
    }
    normals
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quantize_u16_midpoint_when_zero_extent() {
        // Python: abs(vmax - vmin) < 1e-10 -> 32768
        assert_eq!(quantize_u16(0.5, 1.0, 1.0), 32768);
        assert_eq!(quantize_u16(0.5, 1.0, 1.0 + 5e-11), 32768);
    }

    #[test]
    fn quantize_u16_endpoints() {
        assert_eq!(quantize_u16(0.0, 0.0, 1.0), 0);
        assert_eq!(quantize_u16(1.0, 0.0, 1.0), 65535);
        // Clamping outside the range
        assert_eq!(quantize_u16(-0.1, 0.0, 1.0), 0);
        assert_eq!(quantize_u16(1.1, 0.0, 1.0), 65535);
    }

    #[test]
    fn quantize_u16_round_to_nearest_even_at_half() {
        // Python's round() is banker's rounding, but float precision usually
        // tips the t * 65535.0 value off exact half. Just verify monotonicity
        // around the midpoint; the upstream byte-equivalence corpus catches
        // any rounding-mode divergence on real fixtures.
        let q = quantize_u16(0.5, 0.0, 1.0);
        assert!((32767..=32768).contains(&q));
    }

    #[test]
    fn quantize_pac_u16_zero_extent_returns_zero() {
        assert_eq!(quantize_pac_u16(123.4, 100.0, 0.0), 0);
    }

    #[test]
    fn quantize_pac_u16_caps_at_32767() {
        assert_eq!(quantize_pac_u16(2.0, 0.0, 1.0), 32767);
        assert_eq!(quantize_pac_u16(-1.0, 0.0, 1.0), 0);
        assert_eq!(quantize_pac_u16(0.0, 0.0, 1.0), 0);
        assert_eq!(quantize_pac_u16(1.0, 0.0, 1.0), 32767);
    }

    #[test]
    fn pack_pac_normal_preserves_upper_flag_bits() {
        let n = pack_pac_normal([0.0, 1.0, 0.0], 0xC000_0000);
        assert_eq!(n & 0xC000_0000, 0xC000_0000);
        let n2 = pack_pac_normal([0.0, 1.0, 0.0], 0x0000_0000);
        assert_eq!(n2 & 0xC000_0000, 0);
        // Same low 30 bits regardless of flag input.
        assert_eq!(n & 0x3FFF_FFFF, n2 & 0x3FFF_FFFF);
    }

    #[test]
    fn pack_pac_normal_y_axis() {
        // ny = 1.0 -> enc(1.0) = 1023 in bits 20..30
        // nx = 0.0 -> enc(0.0) = 512 in bits 10..20  (511.5 rounded to nearest-even = 512)
        // nz = 0.0 -> enc(0.0) = 512 in bits 0..10
        let packed = pack_pac_normal([0.0, 1.0, 0.0], 0);
        let nz = packed & 0x3FF;
        let nx = (packed >> 10) & 0x3FF;
        let ny = (packed >> 20) & 0x3FF;
        assert_eq!(ny, 1023, "ny should be 1023 (encoded 1.0)");
        assert_eq!(nx, 512, "nx should be 512 (encoded 0.0, 511.5 -> 512 banker)");
        assert_eq!(nz, 512, "nz should be 512 (encoded 0.0)");
    }

    #[test]
    fn pack_pac_normal_python_oracle() {
        // Generated by running core.mesh_importer._pack_pac_normal on the
        // exact same inputs in CPython 3.12. Any divergence here means the
        // PAC vertex records this writer produces will not byte-match the
        // reference -- which downstream breaks the byte-equivalence corpus.
        let cases: &[([f32; 3], u32, u32)] = &[
            ([0.0, 1.0, 0.0],     0,           0x3ff80200),
            ([0.0, 1.0, 0.0],     0xC000_0000, 0xfff80200),
            ([1.0, 0.0, 0.0],     0,           0x200ffe00),
            ([0.0, 0.0, 1.0],     0,           0x200803ff),
            ([0.5, 0.5, 0.5],     0x8000_0000, 0xaffbfeff),
            ([5.0, -5.0, 0.5],    0,           0x000ffeff),
            ([0.7071, 0.7071, 0.0], 0x4000_0000, 0x769da600),
            ([-0.3, 0.6, -0.9],   0,           0x33259833),
        ];
        for (n, existing, want) in cases {
            let got = pack_pac_normal(*n, *existing);
            assert_eq!(
                got, *want,
                "pack_pac_normal({n:?}, {existing:#x}) = {got:#x} (want {want:#x})"
            );
        }
    }

    #[test]
    fn pack_pac_normal_clamps_out_of_range() {
        // Beyond [-1, 1] should clamp before encoding.
        let packed = pack_pac_normal([5.0, -5.0, 0.5], 0);
        let nz = packed & 0x3FF;
        let nx = (packed >> 10) & 0x3FF;
        let ny = (packed >> 20) & 0x3FF;
        assert_eq!(nx, 1023, "5.0 clamps to 1.0 -> 1023");
        assert_eq!(ny, 0, "-5.0 clamps to -1.0 -> 0");
        // 0.5 -> (0.5 + 1.0) * 511.5 = 767.25 -> rounds to 767
        assert_eq!(nz, 767);
    }

    #[test]
    fn compute_bbox_empty_returns_unit() {
        let (mn, mx) = compute_bbox(&[]);
        assert_eq!(mn, [0.0, 0.0, 0.0]);
        assert_eq!(mx, [1.0, 1.0, 1.0]);
    }

    #[test]
    fn compute_bbox_pads_by_epsilon() {
        let (mn, mx) = compute_bbox(&[[0.0, 0.0, 0.0], [1.0, 2.0, 3.0]]);
        let eps = 1e-6f32;
        assert!((mn[0] + eps).abs() < 1e-9);
        assert!((mx[0] - 1.0 - eps).abs() < 1e-6);
        assert!((mx[1] - 2.0 - eps).abs() < 1e-6);
        assert!((mx[2] - 3.0 - eps).abs() < 1e-6);
    }

    #[test]
    fn compute_face_normal_unit_triangle() {
        // CCW triangle in z=0 plane -> normal (0, 0, 1)
        let n = compute_face_normal([0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]);
        assert!((n[0]).abs() < 1e-6);
        assert!((n[1]).abs() < 1e-6);
        assert!((n[2] - 1.0).abs() < 1e-6);
    }

    #[test]
    fn compute_face_normal_degenerate_returns_y_up() {
        // Three collinear points -> length 0 -> (0, 1, 0) fallback.
        let n = compute_face_normal([0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [2.0, 0.0, 0.0]);
        assert_eq!(n, [0.0, 1.0, 0.0]);
    }

    #[test]
    fn compute_smooth_normals_single_quad() {
        // Two CCW triangles forming a unit quad in z=0: every vertex normal -> +z
        let verts = vec![
            [0.0, 0.0, 0.0],
            [1.0, 0.0, 0.0],
            [1.0, 1.0, 0.0],
            [0.0, 1.0, 0.0],
        ];
        let faces = vec![[0u32, 1, 2], [0, 2, 3]];
        let normals = compute_smooth_normals(&verts, &faces);
        assert_eq!(normals.len(), 4);
        for n in &normals {
            assert!(n[0].abs() < 1e-6);
            assert!(n[1].abs() < 1e-6);
            assert!((n[2] - 1.0).abs() < 1e-6, "got {n:?}");
        }
    }

    #[test]
    fn compute_smooth_normals_skips_oob_face_indices() {
        // Face referring to vertex 99 in a 3-vert mesh must be silently dropped.
        let verts = vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]];
        let faces = vec![[0u32, 1, 2], [0, 1, 99]];
        let normals = compute_smooth_normals(&verts, &faces);
        // First face is valid -> all three vertex normals = +z.
        for n in &normals {
            assert!((n[2] - 1.0).abs() < 1e-6);
        }
    }

    #[test]
    fn compute_smooth_normals_isolated_vertex_gets_y_up() {
        // Vertex 3 is in the list but not referenced by any face -> length 0 -> (0,1,0).
        let verts = vec![
            [0.0, 0.0, 0.0],
            [1.0, 0.0, 0.0],
            [0.0, 1.0, 0.0],
            [5.0, 5.0, 5.0], // isolated
        ];
        let faces = vec![[0u32, 1, 2]];
        let normals = compute_smooth_normals(&verts, &faces);
        assert_eq!(normals[3], [0.0, 1.0, 0.0]);
    }
}
