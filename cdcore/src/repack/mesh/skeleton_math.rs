//! 4x4 matrix + quaternion + axis-conversion helpers for skeleton FBX export.
//!
//! Mirrors `core/mesh_exporter.py`:
//!   - `_yup_to_zup_vec3 / _quat / _mat4`
//!   - `_lcl_from_bind_matrix`
//!   - `_mat4_from_lcl_trs`
//!   - `_mat4_mul`
//!   - `_mat4_inverse`
//!
//! Layout: matrices are flat 16-float column-major (matching FBX-on-disk and
//! Python's tuple convention). For 4x4 column-major flat,
//! `m[col*4 + row]` = `M[row][col]`. PAB skeletons store bind matrices in the
//! same column-major flat layout, so the Rust skeleton's `[[f32; 4]; 4]` (which
//! is row-by-row over the original file bytes) maps to this flat form via
//! `iter().flatten().collect()` -- no transpose required.

/// 4x4 column-major flat matrix.
pub type Mat4 = [f64; 16];

/// 4x4 identity matrix.
pub const IDENTITY: Mat4 = [
    1.0, 0.0, 0.0, 0.0,
    0.0, 1.0, 0.0, 0.0,
    0.0, 0.0, 1.0, 0.0,
    0.0, 0.0, 0.0, 1.0,
];

/// Convert a 3-vector from Y-up to Z-up. (x, y, z) -> (x, -z, y).
pub fn yup_to_zup_vec3(v: [f64; 3]) -> [f64; 3] {
    [v[0], -v[2], v[1]]
}

/// Convert a quaternion (xyzw) from Y-up to Z-up frame.
///
/// Composes `q_zup = r * q_yup * r^-1` where `r` is the +90 deg X rotation
/// quaternion (x=sin(45 deg), y=0, z=0, w=cos(45 deg)).
pub fn yup_to_zup_quat(q: [f64; 4]) -> [f64; 4] {
    let s = 0.5f64.sqrt();
    let (rx, ry, rz, rw) = (s, 0.0, 0.0, s);
    let (qx, qy, qz, qw) = (q[0], q[1], q[2], q[3]);

    // r * q (Hamilton product)
    let ax = rw * qx + rx * qw + ry * qz - rz * qy;
    let ay = rw * qy - rx * qz + ry * qw + rz * qx;
    let az = rw * qz + rx * qy - ry * qx + rz * qw;
    let aw = rw * qw - rx * qx - ry * qy - rz * qz;

    // (r * q) * r^-1 where r^-1 = (-rx, -ry, -rz, rw) for unit r
    let bx = aw * (-rx) + ax * rw + ay * (-rz) - az * (-ry);
    let by = aw * (-ry) - ax * (-rz) + ay * rw + az * (-rx);
    let bz = aw * (-rz) + ax * (-ry) - ay * (-rx) + az * rw;
    let bw = aw * rw - ax * (-rx) - ay * (-ry) - az * (-rz);
    [bx, by, bz, bw]
}

/// Convert a column-major-flat 4x4 transformation matrix Y-up to Z-up.
///
/// Computes `R * M * R^-1` where `R` is the rotation +90 deg around X (the
/// Y-up to Z-up basis change).
pub fn yup_to_zup_mat4(m: &Mat4) -> Mat4 {
    const R: Mat4 = [
        1.0, 0.0, 0.0, 0.0,
        0.0, 0.0, 1.0, 0.0,
        0.0, -1.0, 0.0, 0.0,
        0.0, 0.0, 0.0, 1.0,
    ];
    const R_INV: Mat4 = [
        1.0, 0.0, 0.0, 0.0,
        0.0, 0.0, -1.0, 0.0,
        0.0, 1.0, 0.0, 0.0,
        0.0, 0.0, 0.0, 1.0,
    ];
    mat4_mul(&mat4_mul(&R, m), &R_INV)
}

/// `C = A * B` for two column-major flat 4x4 matrices.
/// `C[row][col] = sum_k A[row][k] * B[k][col]`.
pub fn mat4_mul(a: &Mat4, b: &Mat4) -> Mat4 {
    let mut out = [0.0f64; 16];
    for col in 0..4 {
        for row in 0..4 {
            let mut s = 0.0;
            for k in 0..4 {
                s += a[k * 4 + row] * b[col * 4 + k];
            }
            out[col * 4 + row] = s;
        }
    }
    out
}

/// Inverse of a 4x4 affine matrix (column-major flat). The bottom row is
/// assumed to be [0, 0, 0, 1]. Returns identity when the linear 3x3 block is
/// singular (caller's safety net), matching the Python reference.
pub fn mat4_inverse(m: &Mat4) -> Mat4 {
    let a = m;
    let (a00, a10, a20) = (a[0], a[1], a[2]);
    let (a01, a11, a21) = (a[4], a[5], a[6]);
    let (a02, a12, a22) = (a[8], a[9], a[10]);

    let c00 = a11 * a22 - a12 * a21;
    let c01 = -(a01 * a22 - a02 * a21);
    let c02 = a01 * a12 - a02 * a11;
    let c10 = -(a10 * a22 - a12 * a20);
    let c11 = a00 * a22 - a02 * a20;
    let c12 = -(a00 * a12 - a02 * a10);
    let c20 = a10 * a21 - a11 * a20;
    let c21 = -(a00 * a21 - a01 * a20);
    let c22 = a00 * a11 - a01 * a10;

    let det = a00 * c00 + a01 * c10 + a02 * c20;
    if det.abs() < 1e-12 {
        return IDENTITY;
    }
    let inv_det = 1.0 / det;

    let inv00 = c00 * inv_det;
    let inv01 = c01 * inv_det;
    let inv02 = c02 * inv_det;
    let inv10 = c10 * inv_det;
    let inv11 = c11 * inv_det;
    let inv12 = c12 * inv_det;
    let inv20 = c20 * inv_det;
    let inv21 = c21 * inv_det;
    let inv22 = c22 * inv_det;

    let (tx, ty, tz) = (a[12], a[13], a[14]);
    let inv_tx = -(inv00 * tx + inv01 * ty + inv02 * tz);
    let inv_ty = -(inv10 * tx + inv11 * ty + inv12 * tz);
    let inv_tz = -(inv20 * tx + inv21 * ty + inv22 * tz);

    [
        inv00, inv10, inv20, 0.0,
        inv01, inv11, inv21, 0.0,
        inv02, inv12, inv22, 0.0,
        inv_tx, inv_ty, inv_tz, 1.0,
    ]
}

/// Decomposed Lcl Translation/Rotation/Scaling. Rotation in degrees (Blender's
/// intrinsic XYZ convention -- matches `_lcl_from_bind_matrix`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LclTrs {
    pub tx: f64, pub ty: f64, pub tz: f64,
    pub rx: f64, pub ry: f64, pub rz: f64,
    pub sx: f64, pub sy: f64, pub sz: f64,
}

/// Decompose an FBX bind matrix into Lcl TRS using Blender's intrinsic XYZ
/// Euler convention (= extrinsic ZYX, R = Rz * Ry * Rx for column vectors).
///
/// See `core/mesh_exporter.py::_lcl_from_bind_matrix` for the derivation
/// and the gimbal-lock sign-aware fix that keeps the Bip01 Pelvis bone
/// from drifting when its bind matrix sits exactly at +/- 90 deg pitch.
pub fn lcl_from_bind_matrix(m: &Mat4, scale: f64) -> LclTrs {
    let tx = m[12] * scale;
    let ty = m[13] * scale;
    let tz = m[14] * scale;

    // 3x3 rotation block, column-vector convention: R[row][col] = m[col*4+row].
    let mut r = [[0.0f64; 3]; 3];
    for col in 0..3 {
        for row in 0..3 {
            r[row][col] = m[col * 4 + row];
        }
    }

    // Per-axis scale = column lengths.
    let sx = (r[0][0].powi(2) + r[1][0].powi(2) + r[2][0].powi(2)).sqrt();
    let sx = if sx == 0.0 { 1.0 } else { sx };
    let sy = (r[0][1].powi(2) + r[1][1].powi(2) + r[2][1].powi(2)).sqrt();
    let sy = if sy == 0.0 { 1.0 } else { sy };
    let sz = (r[0][2].powi(2) + r[1][2].powi(2) + r[2][2].powi(2)).sqrt();
    let sz = if sz == 0.0 { 1.0 } else { sz };

    r[0][0] /= sx; r[1][0] /= sx; r[2][0] /= sx;
    r[0][1] /= sy; r[1][1] /= sy; r[2][1] /= sy;
    r[0][2] /= sz; r[1][2] /= sz; r[2][2] /= sz;

    let neg_sin_b = r[2][0].clamp(-1.0, 1.0);
    let sin_b = -neg_sin_b;

    const GIMBAL_THRESHOLD: f64 = 0.999999;

    let (alpha, beta, gamma);
    if sin_b.abs() < GIMBAL_THRESHOLD {
        beta = sin_b.asin();
        alpha = r[2][1].atan2(r[2][2]);
        gamma = r[1][0].atan2(r[0][0]);
    } else {
        beta = std::f64::consts::FRAC_PI_2.copysign(sin_b);
        alpha = (sin_b * r[0][1]).atan2(r[1][1]);
        gamma = 0.0;
    }

    LclTrs {
        tx, ty, tz,
        rx: alpha.to_degrees(),
        ry: beta.to_degrees(),
        rz: gamma.to_degrees(),
        sx, sy, sz,
    }
}

/// Recompose a 4x4 column-major flat matrix from Lcl TRS values. Inverse of
/// [`lcl_from_bind_matrix`]. `R = Rz(g) * Ry(b) * Rx(a)`, then `M = T * R * S`.
pub fn mat4_from_lcl_trs(trs: &LclTrs) -> Mat4 {
    let a = trs.rx.to_radians();
    let b = trs.ry.to_radians();
    let c = trs.rz.to_radians();
    let (ca, sa) = (a.cos(), a.sin());
    let (cb, sb) = (b.cos(), b.sin());
    let (cc, sc) = (c.cos(), c.sin());

    let r00 = cc * cb;
    let r01 = -sc * ca + cc * sb * sa;
    let r02 = sc * sa + cc * sb * ca;
    let r10 = sc * cb;
    let r11 = cc * ca + sc * sb * sa;
    let r12 = -cc * sa + sc * sb * ca;
    let r20 = -sb;
    let r21 = cb * sa;
    let r22 = cb * ca;

    [
        r00 * trs.sx, r10 * trs.sx, r20 * trs.sx, 0.0,
        r01 * trs.sy, r11 * trs.sy, r21 * trs.sy, 0.0,
        r02 * trs.sz, r12 * trs.sz, r22 * trs.sz, 0.0,
        trs.tx,       trs.ty,       trs.tz,       1.0,
    ]
}

/// Convenience: convert a row-major-stored Rust `[[f32;4];4]` (which is what
/// `formats/animation/pab.rs` returns -- the file bytes laid out into 4-row
/// chunks) into the flat column-major form expected by these helpers.
///
/// PAB stores bind matrices column-major flat. The Rust 2D array stores those
/// 16 floats by row-of-array (`mat[i][j]` = file byte `(i*4+j)*4`). Flattening
/// in row order recovers the file order, which IS column-major flat -- so no
/// transpose is needed, just a flatten + widen-to-f64.
pub fn flatten_pab_bind(bind: &[[f32; 4]; 4]) -> Mat4 {
    let mut out = [0.0f64; 16];
    for (i, row) in bind.iter().enumerate() {
        for (j, &v) in row.iter().enumerate() {
            out[i * 4 + j] = v as f64;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f64, b: f64, eps: f64) -> bool {
        (a - b).abs() <= eps
    }

    fn mat_max_diff(a: &Mat4, b: &Mat4) -> f64 {
        let mut m = 0.0;
        for i in 0..16 {
            let d = (a[i] - b[i]).abs();
            if d > m { m = d; }
        }
        m
    }

    #[test]
    fn yup_to_zup_vec3_swaps_y_negz() {
        assert_eq!(yup_to_zup_vec3([1.0, 2.0, 3.0]), [1.0, -3.0, 2.0]);
        assert_eq!(yup_to_zup_vec3([0.0, 1.0, 0.0]), [0.0, 0.0, 1.0]);
    }

    #[test]
    fn mat4_mul_identity() {
        let r = mat4_mul(&IDENTITY, &IDENTITY);
        assert!(mat_max_diff(&r, &IDENTITY) < 1e-12);
    }

    #[test]
    fn mat4_mul_translation_compose() {
        let mut t1 = IDENTITY;
        t1[12] = 1.0; t1[13] = 2.0; t1[14] = 3.0;
        let mut t2 = IDENTITY;
        t2[12] = 4.0; t2[13] = 5.0; t2[14] = 6.0;
        let r = mat4_mul(&t1, &t2);
        // T1 * T2 translates by (5, 7, 9).
        assert!(approx(r[12], 5.0, 1e-12));
        assert!(approx(r[13], 7.0, 1e-12));
        assert!(approx(r[14], 9.0, 1e-12));
    }

    #[test]
    fn mat4_inverse_identity() {
        let r = mat4_inverse(&IDENTITY);
        assert!(mat_max_diff(&r, &IDENTITY) < 1e-12);
    }

    #[test]
    fn mat4_inverse_round_trip_on_translation() {
        let mut t = IDENTITY;
        t[12] = 1.5; t[13] = -2.5; t[14] = 7.25;
        let inv = mat4_inverse(&t);
        let p = mat4_mul(&t, &inv);
        assert!(mat_max_diff(&p, &IDENTITY) < 1e-9);
    }

    #[test]
    fn mat4_inverse_singular_returns_identity() {
        // Zero linear block -> det = 0 -> identity fallback.
        let m: Mat4 = [0.0; 16];
        let inv = mat4_inverse(&m);
        assert!(mat_max_diff(&inv, &IDENTITY) < 1e-12);
    }

    #[test]
    fn yup_to_zup_quat_y_axis_rotation_round_trip() {
        // Y-up (0, sin(t/2), 0, cos(t/2)) is a rotation around Y.
        // After Y-up->Z-up the same rotation should be around Z (since Y maps to Z).
        let t = std::f64::consts::FRAC_PI_4;
        let q = [0.0, (t / 2.0).sin(), 0.0, (t / 2.0).cos()];
        let q_zup = yup_to_zup_quat(q);
        // Expected: rotation around Z axis with same angle.
        let exp = [0.0, 0.0, (t / 2.0).sin(), (t / 2.0).cos()];
        for i in 0..4 {
            assert!(approx(q_zup[i], exp[i], 1e-9), "axis {i}: {} vs {}", q_zup[i], exp[i]);
        }
    }

    #[test]
    fn lcl_decompose_recompose_round_trip() {
        // Pure translation
        let m: Mat4 = {
            let mut m = IDENTITY;
            m[12] = 1.0; m[13] = 2.0; m[14] = 3.0;
            m
        };
        let trs = lcl_from_bind_matrix(&m, 1.0);
        assert!(approx(trs.tx, 1.0, 1e-9));
        assert!(approx(trs.ty, 2.0, 1e-9));
        assert!(approx(trs.tz, 3.0, 1e-9));
        let r = mat4_from_lcl_trs(&trs);
        assert!(mat_max_diff(&r, &m) < 1e-9);
    }

    #[test]
    fn lcl_decompose_recompose_round_trip_rotation() {
        // 30 deg around X, 45 deg around Y, 60 deg around Z (intrinsic XYZ).
        let trs = LclTrs {
            tx: 0.0, ty: 0.0, tz: 0.0,
            rx: 30.0, ry: 45.0, rz: 60.0,
            sx: 1.0, sy: 1.0, sz: 1.0,
        };
        let m = mat4_from_lcl_trs(&trs);
        let trs2 = lcl_from_bind_matrix(&m, 1.0);
        // Recompose and compare matrices (Euler representation can differ but
        // matrices should be identical).
        let m2 = mat4_from_lcl_trs(&trs2);
        assert!(mat_max_diff(&m, &m2) < 1e-9);
    }

    #[test]
    fn lcl_decompose_handles_pi_over_2_gimbal() {
        // Bip01 Pelvis-style case: ry = -90 deg. The Python reference
        // explicitly tests this and uses sign-aware atan2 on R[0][1].
        let trs = LclTrs {
            tx: 0.0, ty: 0.0, tz: 0.0,
            rx: 17.5, ry: -90.0, rz: 0.0,
            sx: 1.0, sy: 1.0, sz: 1.0,
        };
        let m = mat4_from_lcl_trs(&trs);
        let trs2 = lcl_from_bind_matrix(&m, 1.0);
        let m2 = mat4_from_lcl_trs(&trs2);
        // Round-trip through the gimbal-lock branch must reproduce the matrix.
        assert!(mat_max_diff(&m, &m2) < 1e-9, "diff: {}", mat_max_diff(&m, &m2));
    }

    #[test]
    fn yup_to_zup_mat4_translation_swap() {
        // Pure translation (0, 1, 0) in Y-up should land at (0, 0, 1) in Z-up.
        let mut m = IDENTITY;
        m[12] = 0.0; m[13] = 1.0; m[14] = 0.0;
        let m_zup = yup_to_zup_mat4(&m);
        assert!(approx(m_zup[12], 0.0, 1e-9));
        assert!(approx(m_zup[13], 0.0, 1e-9));
        assert!(approx(m_zup[14], 1.0, 1e-9));
    }

    #[test]
    fn flatten_pab_bind_preserves_file_order() {
        let bind = [
            [0.0f32, 1.0, 2.0, 3.0],
            [4.0,    5.0, 6.0, 7.0],
            [8.0,    9.0,10.0,11.0],
            [12.0,  13.0,14.0,15.0],
        ];
        let f = flatten_pab_bind(&bind);
        for i in 0..16 {
            assert!(approx(f[i], i as f64, 1e-12));
        }
    }
}
