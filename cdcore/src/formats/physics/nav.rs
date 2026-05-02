//! Navigation mesh parser.
//!
//! No header — starts directly with 16-byte cell records:
//!   [0:4]   cell_id (u32 LE)
//!   [4:8]   grid_ref (u32 LE, pattern 0xFEFFFFxx)
//!   [8:12]  flags (u32 LE)
//!   [12:16] neighbor (u32 LE)

use crate::error::{read_u32_le, Result};

#[derive(Debug, Clone)]
pub struct NavCell {
    pub cell_id:  u32,
    pub grid_ref: u32,
    pub flags:    u32,
    pub neighbor: u32,
    pub tile_x:   u8,
    pub tile_y:   u8,
}

#[derive(Debug, Default, Clone)]
pub struct ParsedNavmesh {
    pub path: String,
    pub cell_count: usize,
    pub cells: Vec<NavCell>,
    pub tile_min: [i32; 2],
    pub tile_max: [i32; 2],
    pub file_size: usize,
}

pub fn parse(data: &[u8], filename: &str) -> Result<ParsedNavmesh> {
    let n = data.len() / 16;
    let mut cells = Vec::with_capacity(n);

    for i in 0..n {
        let off = i * 16;
        let cell_id  = read_u32_le(data, off)?;
        let grid_ref = read_u32_le(data, off + 4)?;
        let flags    = read_u32_le(data, off + 8)?;
        let neighbor = read_u32_le(data, off + 12)?;
        let tile_idx = (grid_ref & 0xFF) as u8;
        cells.push(NavCell {
            cell_id, grid_ref, flags, neighbor,
            tile_x: tile_idx, tile_y: 0,
        });
    }

    let (tile_min, tile_max) = if cells.is_empty() {
        ([0, 0], [0, 0])
    } else {
        let min_tx = cells.iter().map(|c| c.tile_x as i32).min().unwrap();
        let max_tx = cells.iter().map(|c| c.tile_x as i32).max().unwrap();
        ([min_tx, 0], [max_tx, 0])
    };

    Ok(ParsedNavmesh {
        path: filename.to_string(),
        cell_count: cells.len(),
        cells,
        tile_min,
        tile_max,
        file_size: data.len(),
    })
}
