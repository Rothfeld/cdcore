//! PABC mesh-skinning palette parser.
//!
//! The same PAR container is reused for per-mesh bone palettes that map
//! vertex slot indices to PAB skeleton bones. Without this palette the
//! bone slots in a PAC vertex are misinterpreted as direct PAB indices,
//! producing the spike-shatter artifact on skinned character meshes.
//!
//! Layout (verified Apr 2026):
//!   PAR header (20 bytes)
//!   [0x10..0x13] u32 LE record_count
//!   [0x14..]     record_count * 196-byte records
//!
//! Per-record (196 bytes):
//!   [+0]       u8 flag
//!   [+1..+3]   24-bit PAB bone hash (low 3 bytes of u32 LE at +1)
//!   [+4..+67]  bind_matrix    (16 fp32, column-major)
//!   [+68..+131] inv_bind_matrix
//!   [+132..+195] aux_matrix

const PAR_MAGIC: &[u8; 4] = b"PAR ";
const RECORD_SIZE: usize = 196;
const HEADER_SIZE: usize = 0x14;

#[derive(Debug, Clone)]
pub struct PabcSkinRecord {
    pub record_index: usize,
    pub bone_hash_24: u32,
    pub pab_bone_index: i32,   // -1 if hash not found in PAB
    pub flag_byte: u8,
    pub bind_matrix:     [[f32; 4]; 4],
    pub inv_bind_matrix: [[f32; 4]; 4],
    pub aux_matrix:      [[f32; 4]; 4],
}

#[derive(Debug, Clone)]
pub struct PabcSkinPalette {
    pub path: String,
    pub record_count: usize,
    pub records: Vec<PabcSkinRecord>,
}

impl PabcSkinPalette {
    pub fn slot_to_pab(&self, slot: usize) -> i32 {
        self.records.get(slot).map(|r| r.pab_bone_index).unwrap_or(-1)
    }
}

fn read_mat4(data: &[u8], off: usize) -> [[f32; 4]; 4] {
    let mut mat = [[0.0f32; 4]; 4];
    for row in 0..4 {
        for col in 0..4 {
            let i = off + (row * 4 + col) * 4;
            if i + 4 <= data.len() {
                mat[row][col] = f32::from_le_bytes(data[i..i+4].try_into().unwrap());
            }
        }
    }
    mat
}

pub fn parse_skin(data: &[u8], pab_hashes: &[u32], filename: &str) -> PabcSkinPalette {
    let mut palette = PabcSkinPalette {
        path: filename.to_string(),
        record_count: 0,
        records: vec![],
    };

    if data.len() < HEADER_SIZE + RECORD_SIZE {
        return palette;
    }
    if &data[0..4] != PAR_MAGIC {
        return palette;
    }

    let record_count = u32::from_le_bytes(data[0x10..0x14].try_into().unwrap()) as usize;
    if record_count == 0 || record_count > 100_000 {
        return palette;
    }

    let available = (data.len() - HEADER_SIZE) / RECORD_SIZE;
    let record_count = record_count.min(available);
    palette.record_count = record_count;

    let hash_to_idx: std::collections::HashMap<u32, usize> = pab_hashes.iter()
        .enumerate()
        .map(|(i, &h)| (h, i))
        .collect();

    for i in 0..record_count {
        let rec_off = HEADER_SIZE + i * RECORD_SIZE;
        let flag_byte = data[rec_off];
        let hash_u32 = u32::from_le_bytes(data[rec_off+1..rec_off+5].try_into().unwrap());
        let bone_hash_24 = hash_u32 & 0x00FF_FFFF;
        let pab_bone_index = hash_to_idx.get(&bone_hash_24).map(|&i| i as i32).unwrap_or(-1);

        let bind_matrix     = read_mat4(data, rec_off + 4);
        let inv_bind_matrix = read_mat4(data, rec_off + 4 + 64);
        let aux_matrix      = read_mat4(data, rec_off + 4 + 128);

        palette.records.push(PabcSkinRecord {
            record_index: i,
            bone_hash_24,
            pab_bone_index,
            flag_byte,
            bind_matrix,
            inv_bind_matrix,
            aux_matrix,
        });
    }

    palette
}
