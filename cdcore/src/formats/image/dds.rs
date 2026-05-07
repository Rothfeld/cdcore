use crate::error::{ParseError, Result};

const DDS_MAGIC: &[u8; 4] = b"DDS ";

// Pixel format flags
const DDPF_FOURCC: u32     = 0x4;
const DDPF_RGB: u32        = 0x40;
const DDPF_LUMINANCE: u32  = 0x20000;

// DXGI formats (DX10 extended header)
const DXGI_BC1: &[u32]  = &[71, 72];
const DXGI_BC2: &[u32]  = &[74, 75];
const DXGI_BC3: &[u32]  = &[77, 78];
const DXGI_BC4: &[u32]  = &[80, 81];
const DXGI_BC5: &[u32]  = &[83, 84];
const DXGI_BC6H: &[u32] = &[95, 96];
const DXGI_BC7: &[u32]  = &[98, 99];
const DXGI_RGBA16F: &[u32] = &[10];       // R16G16B16A16_FLOAT
const DXGI_RGBA32F: &[u32] = &[2];        // R32G32B32A32_FLOAT
const DXGI_R16F: &[u32]    = &[54, 55];   // R16_FLOAT, R16_UNORM
const DXGI_R32F: &[u32]    = &[41, 43];   // R32_FLOAT, R32_UINT
const DXGI_BGRA8: &[u32]        = &[87, 88, 89, 90, 91]; // B8G8R8A8 variants
const DXGI_RGBA8: &[u32]        = &[28, 29, 30, 31];     // R8G8B8A8 variants
const DXGI_R8: &[u32]           = &[61, 62];             // R8_UNORM, R8_UINT
const DXGI_R10G10B10A2: &[u32]  = &[24, 25];             // R10G10B10A2_UNORM/UINT

#[derive(Debug, Clone, Copy, PartialEq)]
enum DdsFormat {
    Bc1,
    Bc2,
    Bc3,
    Bc4,
    Bc5,
    Bc6h,
    Bc7,
    Bgra32,
    Rgba32,         // R8G8B8A8 -- no channel swizzle needed
    Rgb24,
    R8,             // single channel grayscale
    R10G10B10A2,    // packed 10/10/10/2 bits
    Luminance8,
    Luminance16,
    Rgba16F,   // 4 x f16
    Rgba32F,   // 4 x f32
    R16F,      // 1 x f16 -> grayscale
    R32F,      // 1 x f32 -> grayscale
}

struct DdsHeader {
    width: u32,
    height: u32,
    format: DdsFormat,
    data_offset: usize,
}

fn read_u16_le(data: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([data[off], data[off + 1]])
}

fn read_u32_le(data: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]])
}

fn parse_header(data: &[u8]) -> Result<DdsHeader> {
    if data.len() < 128 || &data[..4] != DDS_MAGIC {
        return Err(ParseError::Other("not a DDS file".into()));
    }

    let height = read_u32_le(data, 12);
    let width  = read_u32_le(data, 16);
    let pf_flags = read_u32_le(data, 80);
    let fourcc   = &data[84..88];
    let bpp      = read_u32_le(data, 88);

    let (format, data_offset) = if pf_flags & DDPF_FOURCC != 0 {
        match fourcc {
            b"DXT1" => (DdsFormat::Bc1, 128),
            b"DXT3" => (DdsFormat::Bc2, 128),
            b"DXT5" => (DdsFormat::Bc3, 128),
            b"BC4U" | b"BC4S" => (DdsFormat::Bc4, 128),
            b"BC5U" | b"BC5S" => (DdsFormat::Bc5, 128),
            // D3D9 float / HDR formats (numeric FourCC values)
            b"o\0\0\0" => (DdsFormat::R16F,    128), // 111 = D3DFMT_R16F
            b"q\0\0\0" => (DdsFormat::Rgba16F, 128), // 113 = D3DFMT_A16B16G16R16F
            b"t\0\0\0" => (DdsFormat::Rgba32F, 128), // 116 = D3DFMT_A32B32G32R32F
            b"DX10" => {
                if data.len() < 148 {
                    return Err(ParseError::Other("DX10 header truncated".into()));
                }
                let dxgi = read_u32_le(data, 128);
                let fmt = if DXGI_BC1.contains(&dxgi)    { DdsFormat::Bc1 }
                     else if DXGI_BC2.contains(&dxgi)    { DdsFormat::Bc2 }
                     else if DXGI_BC3.contains(&dxgi)    { DdsFormat::Bc3 }
                     else if DXGI_BC4.contains(&dxgi)    { DdsFormat::Bc4 }
                     else if DXGI_BC5.contains(&dxgi)    { DdsFormat::Bc5 }
                     else if DXGI_BC6H.contains(&dxgi)   { DdsFormat::Bc6h }
                     else if DXGI_BC7.contains(&dxgi)    { DdsFormat::Bc7 }
                     else if DXGI_RGBA16F.contains(&dxgi){ DdsFormat::Rgba16F }
                     else if DXGI_RGBA32F.contains(&dxgi){ DdsFormat::Rgba32F }
                     else if DXGI_R16F.contains(&dxgi)   { DdsFormat::R16F }
                     else if DXGI_R32F.contains(&dxgi)   { DdsFormat::R32F }
                     else if DXGI_BGRA8.contains(&dxgi)       { DdsFormat::Bgra32 }
                     else if DXGI_RGBA8.contains(&dxgi)       { DdsFormat::Rgba32 }
                     else if DXGI_R8.contains(&dxgi)          { DdsFormat::R8 }
                     else if DXGI_R10G10B10A2.contains(&dxgi) { DdsFormat::R10G10B10A2 }
                     else {
                         return Err(ParseError::Other(
                             format!("unsupported DXGI format {dxgi}")
                         ));
                     };
                (fmt, 148)
            }
            _ => return Err(ParseError::Other(
                format!("unsupported FourCC {:?}", std::str::from_utf8(fourcc).unwrap_or("?"))
            )),
        }
    } else if pf_flags & DDPF_RGB != 0 {
        let fmt = if bpp == 32 { DdsFormat::Bgra32 }
                  else if bpp == 24 { DdsFormat::Rgb24 }
                  else {
                      return Err(ParseError::Other(format!("unsupported RGB bpp {bpp}")));
                  };
        (fmt, 128)
    } else if pf_flags & DDPF_LUMINANCE != 0 {
        let fmt = if bpp == 8 { DdsFormat::Luminance8 } else { DdsFormat::Luminance16 };
        (fmt, 128)
    } else {
        // Last-resort heuristic matching the Python fallback.
        let rmask = read_u32_le(data, 92);
        if bpp == 8 && rmask == 0xFF {
            (DdsFormat::Luminance8, 128)
        } else if bpp == 16 && rmask == 0xFFFF {
            (DdsFormat::Luminance16, 128)
        } else {
            return Err(ParseError::Other("unsupported DDS pixel format".into()));
        }
    };

    Ok(DdsHeader { width, height, format, data_offset })
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

#[inline]
fn rgb565(v: u16) -> (u8, u8, u8) {
    let r = ((v >> 11) & 0x1F) as u32 * 255 / 31;
    let g = ((v >>  5) & 0x3F) as u32 * 255 / 63;
    let b = ( v        & 0x1F) as u32 * 255 / 31;
    (r as u8, g as u8, b as u8)
}

#[inline]
fn lerp3(a: u8, b: u8, num: u32, denom: u32) -> u8 {
    ((a as u32 * (denom - num) + b as u32 * num) / denom) as u8
}

fn bc4_lut(a0: u8, a1: u8) -> [u8; 8] {
    let mut lut = [0u8; 8];
    lut[0] = a0;
    lut[1] = a1;
    if a0 > a1 {
        for i in 0..6u32 {
            lut[2 + i as usize] =
                ((6 - i) * a0 as u32 + (1 + i) * a1 as u32) as u8 / 7;
        }
    } else {
        for i in 0..4u32 {
            lut[2 + i as usize] =
                ((4 - i) * a0 as u32 + (1 + i) * a1 as u32) as u8 / 5;
        }
        lut[6] = 0;
        lut[7] = 255;
    }
    lut
}

// ---------------------------------------------------------------------------
// Block decoders
// ---------------------------------------------------------------------------

fn decode_bc1(src: &[u8], width: u32, height: u32) -> Vec<u8> {
    let mut rgba = vec![0u8; (width * height * 4) as usize];
    let bx = ((width  + 3) / 4).max(1);
    let by = ((height + 3) / 4).max(1);
    let mut off = 0usize;

    for by in 0..by {
        for bx in 0..bx {
            if off + 8 > src.len() { break; }
            let c0 = read_u16_le(src, off);
            let c1 = read_u16_le(src, off + 2);
            let bits = read_u32_le(src, off + 4);
            off += 8;

            let (r0, g0, b0) = rgb565(c0);
            let (r1, g1, b1) = rgb565(c1);

            // 4 color entries: [c0, c1, mix1, mix2/transparent]
            let colors: [(u8, u8, u8, u8); 4] = if c0 > c1 {
                [
                    (r0, g0, b0, 255),
                    (r1, g1, b1, 255),
                    (lerp3(r0, r1, 1, 3), lerp3(g0, g1, 1, 3), lerp3(b0, b1, 1, 3), 255),
                    (lerp3(r0, r1, 2, 3), lerp3(g0, g1, 2, 3), lerp3(b0, b1, 2, 3), 255),
                ]
            } else {
                [
                    (r0, g0, b0, 255),
                    (r1, g1, b1, 255),
                    (lerp3(r0, r1, 1, 2), lerp3(g0, g1, 1, 2), lerp3(b0, b1, 1, 2), 255),
                    (0, 0, 0, 0),
                ]
            };

            for py in 0..4u32 {
                for px in 0..4u32 {
                    let x = bx * 4 + px;
                    let y = by * 4 + py;
                    if x < width && y < height {
                        let idx = ((bits >> (2 * (py * 4 + px))) & 3) as usize;
                        let (r, g, b, a) = colors[idx];
                        let p = ((y * width + x) * 4) as usize;
                        rgba[p]     = r;
                        rgba[p + 1] = g;
                        rgba[p + 2] = b;
                        rgba[p + 3] = a;
                    }
                }
            }
        }
    }
    rgba
}

fn decode_bc2(src: &[u8], width: u32, height: u32) -> Vec<u8> {
    let mut rgba = vec![0u8; (width * height * 4) as usize];
    let bx = ((width  + 3) / 4).max(1);
    let by = ((height + 3) / 4).max(1);
    let mut off = 0usize;

    for by in 0..by {
        for bx in 0..bx {
            if off + 16 > src.len() { break; }
            let alpha_bits = u64::from_le_bytes(src[off..off+8].try_into().unwrap());
            off += 8;
            let c0 = read_u16_le(src, off);
            let c1 = read_u16_le(src, off + 2);
            let bits = read_u32_le(src, off + 4);
            off += 8;

            let (r0, g0, b0) = rgb565(c0);
            let (r1, g1, b1) = rgb565(c1);
            let colors = [
                (r0, g0, b0), (r1, g1, b1),
                (lerp3(r0, r1, 1, 3), lerp3(g0, g1, 1, 3), lerp3(b0, b1, 1, 3)),
                (lerp3(r0, r1, 2, 3), lerp3(g0, g1, 2, 3), lerp3(b0, b1, 2, 3)),
            ];

            for py in 0..4u32 {
                for px in 0..4u32 {
                    let x = bx * 4 + px;
                    let y = by * 4 + py;
                    if x < width && y < height {
                        let ci = ((bits >> (2 * (py * 4 + px))) & 3) as usize;
                        let ai = ((alpha_bits >> (4 * (py * 4 + px))) & 0xF) as u8;
                        let (r, g, b) = colors[ci];
                        let p = ((y * width + x) * 4) as usize;
                        rgba[p]     = r;
                        rgba[p + 1] = g;
                        rgba[p + 2] = b;
                        rgba[p + 3] = ai * 17;
                    }
                }
            }
        }
    }
    rgba
}

fn decode_bc3(src: &[u8], width: u32, height: u32) -> Vec<u8> {
    let mut rgba = vec![0u8; (width * height * 4) as usize];
    let bx = ((width  + 3) / 4).max(1);
    let by = ((height + 3) / 4).max(1);
    let mut off = 0usize;

    for by in 0..by {
        for bx in 0..bx {
            if off + 16 > src.len() { break; }

            let a0 = src[off];
            let a1 = src[off + 1];
            let alpha_bits = {
                let b = &src[off + 2..off + 8];
                (b[0] as u64)
                    | ((b[1] as u64) << 8)
                    | ((b[2] as u64) << 16)
                    | ((b[3] as u64) << 24)
                    | ((b[4] as u64) << 32)
                    | ((b[5] as u64) << 40)
            };
            off += 8;

            let alpha_lut = bc4_lut(a0, a1);

            let c0 = read_u16_le(src, off);
            let c1 = read_u16_le(src, off + 2);
            let bits = read_u32_le(src, off + 4);
            off += 8;

            let (r0, g0, b0) = rgb565(c0);
            let (r1, g1, b1) = rgb565(c1);
            let colors = [
                (r0, g0, b0), (r1, g1, b1),
                (lerp3(r0, r1, 1, 3), lerp3(g0, g1, 1, 3), lerp3(b0, b1, 1, 3)),
                (lerp3(r0, r1, 2, 3), lerp3(g0, g1, 2, 3), lerp3(b0, b1, 2, 3)),
            ];

            for py in 0..4u32 {
                for px in 0..4u32 {
                    let x = bx * 4 + px;
                    let y = by * 4 + py;
                    if x < width && y < height {
                        let ci = ((bits >> (2 * (py * 4 + px))) & 3) as usize;
                        let ai = ((alpha_bits >> (3 * (py * 4 + px))) & 7) as usize;
                        let (r, g, b) = colors[ci];
                        let p = ((y * width + x) * 4) as usize;
                        rgba[p]     = r;
                        rgba[p + 1] = g;
                        rgba[p + 2] = b;
                        rgba[p + 3] = alpha_lut[ai];
                    }
                }
            }
        }
    }
    rgba
}

fn decode_bc4(src: &[u8], width: u32, height: u32) -> Vec<u8> {
    let mut rgba = vec![0u8; (width * height * 4) as usize];
    let bx = ((width  + 3) / 4).max(1);
    let by = ((height + 3) / 4).max(1);
    let mut off = 0usize;

    for by in 0..by {
        for bx in 0..bx {
            if off + 8 > src.len() { break; }
            let r0 = src[off];
            let r1 = src[off + 1];
            let lut = bc4_lut(r0, r1);
            let bits = {
                let b = &src[off + 2..off + 8];
                (b[0] as u64)
                    | ((b[1] as u64) << 8)
                    | ((b[2] as u64) << 16)
                    | ((b[3] as u64) << 24)
                    | ((b[4] as u64) << 32)
                    | ((b[5] as u64) << 40)
            };
            off += 8;

            for py in 0..4u32 {
                for px in 0..4u32 {
                    let x = bx * 4 + px;
                    let y = by * 4 + py;
                    if x < width && y < height {
                        let idx = ((bits >> (3 * (py * 4 + px))) & 7) as usize;
                        let v = lut[idx];
                        let p = ((y * width + x) * 4) as usize;
                        rgba[p]     = v;
                        rgba[p + 1] = v;
                        rgba[p + 2] = v;
                        rgba[p + 3] = 255;
                    }
                }
            }
        }
    }
    rgba
}

fn decode_bc5(src: &[u8], width: u32, height: u32) -> Vec<u8> {
    let mut rgba = vec![0u8; (width * height * 4) as usize];
    let bx = ((width  + 3) / 4).max(1);
    let by = ((height + 3) / 4).max(1);
    let mut off = 0usize;

    for by in 0..by {
        for bx in 0..bx {
            if off + 16 > src.len() { break; }

            let read_channel = |src: &[u8], off: usize| -> (u8, [u8; 8], u64) {
                let a0 = src[off];
                let a1 = src[off + 1];
                let lut = bc4_lut(a0, a1);
                let b = &src[off + 2..off + 8];
                let bits = (b[0] as u64)
                    | ((b[1] as u64) << 8)
                    | ((b[2] as u64) << 16)
                    | ((b[3] as u64) << 24)
                    | ((b[4] as u64) << 32)
                    | ((b[5] as u64) << 40);
                (a0, lut, bits)
            };

            let (_, r_lut, r_bits) = read_channel(src, off);
            off += 8;
            let (_, g_lut, g_bits) = read_channel(src, off);
            off += 8;

            for py in 0..4u32 {
                for px in 0..4u32 {
                    let x = bx * 4 + px;
                    let y = by * 4 + py;
                    if x < width && y < height {
                        let ri = ((r_bits >> (3 * (py * 4 + px))) & 7) as usize;
                        let gi = ((g_bits >> (3 * (py * 4 + px))) & 7) as usize;
                        let rv = r_lut[ri];
                        let gv = g_lut[gi];
                        // Reconstruct Z from XY normal map
                        let nx = (rv as f32 / 255.0) * 2.0 - 1.0;
                        let ny = (gv as f32 / 255.0) * 2.0 - 1.0;
                        let nz = (1.0_f32 - nx * nx - ny * ny).max(0.0).sqrt();
                        let bv = ((nz * 0.5 + 0.5) * 255.0) as u8;
                        let p = ((y * width + x) * 4) as usize;
                        rgba[p]     = rv;
                        rgba[p + 1] = gv;
                        rgba[p + 2] = bv;
                        rgba[p + 3] = 255;
                    }
                }
            }
        }
    }
    rgba
}

fn decode_bc6h(src: &[u8], width: u32, height: u32) -> Vec<u8> {
    // Simplified: extract approximate endpoint colors per block.
    let mut rgba = vec![0u8; (width * height * 4) as usize];
    let bx = ((width  + 3) / 4).max(1);
    let by = ((height + 3) / 4).max(1);
    let mut off = 0usize;

    for by in 0..by {
        for bx in 0..bx {
            if off + 16 > src.len() { break; }
            let block = &src[off..off + 16];
            off += 16;
            let r = block[0];
            let g = block[2];
            let b = block[4];
            for py in 0..4u32 {
                for px in 0..4u32 {
                    let x = bx * 4 + px;
                    let y = by * 4 + py;
                    if x < width && y < height {
                        let p = ((y * width + x) * 4) as usize;
                        rgba[p]     = r;
                        rgba[p + 1] = g;
                        rgba[p + 2] = b;
                        rgba[p + 3] = 255;
                    }
                }
            }
        }
    }
    rgba
}

fn decode_bc7(src: &[u8], width: u32, height: u32) -> Vec<u8> {
    // Simplified: handle mode 6 (most common), fallback for others.
    let mut rgba = vec![0u8; (width * height * 4) as usize];
    let bx = ((width  + 3) / 4).max(1);
    let by = ((height + 3) / 4).max(1);
    let mut off = 0usize;

    for by in 0..by {
        for bx in 0..bx {
            if off + 16 > src.len() { break; }
            let block = &src[off..off + 16];
            off += 16;

            let mode = (0..8).find(|&m| block[0] & (1 << m) != 0).unwrap_or(0);
            let (r, g, b) = if mode == 6 {
                let r = ((block[1] >> 1) & 0x7F) as u32 * 255 / 127;
                let g = (((block[1] & 1) << 6) | ((block[2] >> 2) & 0x3F)) as u32 * 255 / 127;
                let b = (((block[2] & 3) << 5) | ((block[3] >> 3) & 0x1F)) as u32 * 255 / 127;
                (r as u8, g as u8, b as u8)
            } else {
                (block[1], block[2], block[3])
            };

            for py in 0..4u32 {
                for px in 0..4u32 {
                    let x = bx * 4 + px;
                    let y = by * 4 + py;
                    if x < width && y < height {
                        let p = ((y * width + x) * 4) as usize;
                        rgba[p]     = r;
                        rgba[p + 1] = g;
                        rgba[p + 2] = b;
                        rgba[p + 3] = 255;
                    }
                }
            }
        }
    }
    rgba
}

fn decode_bgra32(src: &[u8], width: u32, height: u32) -> Vec<u8> {
    let n = (width * height * 4) as usize;
    let mut rgba = vec![0u8; n];
    let src = &src[..src.len().min(n)];
    let chunks = src.len() / 4;
    for i in 0..chunks {
        let p = i * 4;
        rgba[p]     = src[p + 2]; // R
        rgba[p + 1] = src[p + 1]; // G
        rgba[p + 2] = src[p];     // B
        rgba[p + 3] = src[p + 3]; // A
    }
    rgba
}

fn decode_luminance8(src: &[u8], width: u32, height: u32) -> Vec<u8> {
    let n = (width * height) as usize;
    let mut rgba = vec![0u8; n * 4];
    for i in 0..n.min(src.len()) {
        let v = src[i];
        let p = i * 4;
        rgba[p]     = v;
        rgba[p + 1] = v;
        rgba[p + 2] = v;
        rgba[p + 3] = 255;
    }
    rgba
}

fn decode_luminance16(src: &[u8], width: u32, height: u32) -> Vec<u8> {
    let n = (width * height) as usize;
    let mut rgba = vec![0u8; n * 4];
    for i in 0..n.min(src.len() / 2) {
        let v = (read_u16_le(src, i * 2) >> 8) as u8;
        let p = i * 4;
        rgba[p]     = v;
        rgba[p + 1] = v;
        rgba[p + 2] = v;
        rgba[p + 3] = 255;
    }
    rgba
}

fn decode_rgba32(src: &[u8], width: u32, height: u32) -> Vec<u8> {
    // R8G8B8A8 -- already in RGBA order, copy directly.
    let n = (width * height * 4) as usize;
    src[..src.len().min(n)].to_vec()
}

fn decode_rgb24(src: &[u8], width: u32, height: u32) -> Vec<u8> {
    let n = (width * height) as usize;
    let mut rgba = vec![255u8; n * 4];
    for i in 0..n.min(src.len() / 3) {
        let s = i * 3;
        let d = i * 4;
        rgba[d]     = src[s + 2]; // R (stored as BGR in legacy DDS)
        rgba[d + 1] = src[s + 1]; // G
        rgba[d + 2] = src[s];     // B
    }
    rgba
}

#[inline]
fn f16_to_u8(bits: u16) -> u8 {
    // Decode IEEE-754 f16 and tone-map to 0-255 via simple clamp.
    let sign   = (bits >> 15) & 1;
    let exp    = (bits >> 10) & 0x1f;
    let frac   = bits & 0x3ff;
    let v: f32 = if exp == 0 {
        if frac == 0 { 0.0 } else { (frac as f32 / 1024.0) * (2.0_f32.powi(-14)) }
    } else if exp == 31 {
        if sign == 0 { f32::INFINITY } else { f32::NEG_INFINITY }
    } else {
        let s = if sign == 1 { -1.0f32 } else { 1.0f32 };
        s * (1.0 + frac as f32 / 1024.0) * (2.0_f32.powi(exp as i32 - 15))
    };
    (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8
}

fn decode_rgba16f(src: &[u8], width: u32, height: u32) -> Vec<u8> {
    let n = (width * height) as usize;
    let mut rgba = vec![255u8; n * 4];
    for i in 0..n.min(src.len() / 8) {
        let s = i * 8;
        let d = i * 4;
        rgba[d]     = f16_to_u8(u16::from_le_bytes([src[s],     src[s + 1]])); // R
        rgba[d + 1] = f16_to_u8(u16::from_le_bytes([src[s + 2], src[s + 3]])); // G
        rgba[d + 2] = f16_to_u8(u16::from_le_bytes([src[s + 4], src[s + 5]])); // B
        rgba[d + 3] = f16_to_u8(u16::from_le_bytes([src[s + 6], src[s + 7]])); // A
    }
    rgba
}

fn decode_rgba32f(src: &[u8], width: u32, height: u32) -> Vec<u8> {
    let n = (width * height) as usize;
    let mut rgba = vec![255u8; n * 4];
    for i in 0..n.min(src.len() / 16) {
        let s = i * 16;
        let d = i * 4;
        let r = f32::from_le_bytes([src[s],      src[s+1],  src[s+2],  src[s+3]]);
        let g = f32::from_le_bytes([src[s+4],    src[s+5],  src[s+6],  src[s+7]]);
        let b = f32::from_le_bytes([src[s+8],    src[s+9],  src[s+10], src[s+11]]);
        let a = f32::from_le_bytes([src[s+12],   src[s+13], src[s+14], src[s+15]]);
        // Reinhard tone-map per channel so HDR values compress gracefully.
        let tm = |v: f32| -> u8 { ((v / (v + 1.0)).clamp(0.0, 1.0) * 255.0 + 0.5) as u8 };
        rgba[d]     = tm(r);
        rgba[d + 1] = tm(g);
        rgba[d + 2] = tm(b);
        rgba[d + 3] = (a.clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
    }
    rgba
}

fn decode_r16f(src: &[u8], width: u32, height: u32) -> Vec<u8> {
    let n = (width * height) as usize;
    let mut rgba = vec![255u8; n * 4];
    for i in 0..n.min(src.len() / 2) {
        let v = f16_to_u8(u16::from_le_bytes([src[i * 2], src[i * 2 + 1]]));
        let d = i * 4;
        rgba[d]     = v;
        rgba[d + 1] = v;
        rgba[d + 2] = v;
    }
    rgba
}

fn decode_r8(src: &[u8], width: u32, height: u32) -> Vec<u8> {
    decode_luminance8(src, width, height)
}

fn decode_r10g10b10a2(src: &[u8], width: u32, height: u32) -> Vec<u8> {
    let n = (width * height) as usize;
    let mut rgba = vec![255u8; n * 4];
    for i in 0..n.min(src.len() / 4) {
        let v = u32::from_le_bytes(src[i*4..i*4+4].try_into().unwrap());
        rgba[i*4]     = ((v & 0x3FF) * 255 / 1023) as u8;
        rgba[i*4 + 1] = (((v >> 10) & 0x3FF) * 255 / 1023) as u8;
        rgba[i*4 + 2] = (((v >> 20) & 0x3FF) * 255 / 1023) as u8;
        rgba[i*4 + 3] = (((v >> 30) & 0x3) * 255 / 3) as u8;
    }
    rgba
}

fn decode_r32f(src: &[u8], width: u32, height: u32) -> Vec<u8> {
    let n = (width * height) as usize;
    let mut rgba = vec![255u8; n * 4];
    for i in 0..n.min(src.len() / 4) {
        let v = f32::from_le_bytes([src[i*4], src[i*4+1], src[i*4+2], src[i*4+3]]);
        let v8 = (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
        let d = i * 4;
        rgba[d]     = v8;
        rgba[d + 1] = v8;
        rgba[d + 2] = v8;
    }
    rgba
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Surface shape classification.  Used by the `.dds.png/` virtual view to
/// refuse formats whose round-trip through a single PNG would lose data
/// (cubemap faces, volume slices, array layers, mip chains).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DdsShape {
    Simple2d,
    Cubemap,
    Volume,
    Array,
    Mipmapped,
}

impl DdsShape {
    pub fn is_simple_2d(self) -> bool { matches!(self, DdsShape::Simple2d) }

    /// True for shapes where the top 2D surface fully represents the file's
    /// renderable content: Simple2d (one mip) or Mipmapped (top mip).
    /// A round-trip through one PNG drops lower mips for the latter, but the
    /// game tolerates the loss (samples at top mip when finer ones are absent).
    /// False for Cubemap/Volume/Array — those carry surfaces beyond the top
    /// 2D image and lose data unrecoverably.
    pub fn is_2d_round_trippable(self) -> bool {
        matches!(self, DdsShape::Simple2d | DdsShape::Mipmapped)
    }
}

/// Inspect a DDS header and report its surface shape.  Cheap: only reads the
/// 128-byte legacy header plus, if present, the 20-byte DX10 extension.
pub fn classify_dds(data: &[u8]) -> Result<DdsShape> {
    if data.len() < 128 || &data[..4] != DDS_MAGIC {
        return Err(ParseError::Other("not a DDS file".into()));
    }
    let mip_count = read_u32_le(data, 28);
    let caps2     = read_u32_le(data, 112);

    const DDSCAPS2_CUBEMAP: u32 = 0x0000_0200;
    const DDSCAPS2_VOLUME:  u32 = 0x0020_0000;

    if caps2 & DDSCAPS2_CUBEMAP != 0 { return Ok(DdsShape::Cubemap); }
    if caps2 & DDSCAPS2_VOLUME  != 0 { return Ok(DdsShape::Volume);  }

    // DX10 extended header (FourCC = "DX10") sits at offset 128, 20 bytes long.
    let pf_flags = read_u32_le(data, 80);
    let fourcc   = &data[84..88];
    if pf_flags & DDPF_FOURCC != 0 && fourcc == b"DX10" && data.len() >= 148 {
        let resource_dim = read_u32_le(data, 128 + 4);
        let misc_flag    = read_u32_le(data, 128 + 8);
        let array_size   = read_u32_le(data, 128 + 12);
        const D3D10_RESOURCE_DIMENSION_TEXTURE3D: u32 = 4;
        const D3D10_RESOURCE_MISC_TEXTURECUBE:    u32 = 0x4;
        if misc_flag & D3D10_RESOURCE_MISC_TEXTURECUBE != 0 { return Ok(DdsShape::Cubemap); }
        if resource_dim == D3D10_RESOURCE_DIMENSION_TEXTURE3D { return Ok(DdsShape::Volume); }
        if array_size > 1 { return Ok(DdsShape::Array); }
    }

    if mip_count > 1 { return Ok(DdsShape::Mipmapped); }
    Ok(DdsShape::Simple2d)
}

/// Decode the first mip level of a DDS file to raw RGBA bytes.
/// Returns (width, height, rgba_bytes).
pub fn decode_dds_to_rgba(data: &[u8]) -> Result<(u32, u32, Vec<u8>)> {
    let hdr = parse_header(data)?;
    let w = hdr.width;
    let h = hdr.height;
    let src = &data[hdr.data_offset..];

    let rgba = match hdr.format {
        DdsFormat::Bc1         => decode_bc1(src, w, h),
        DdsFormat::Bc2         => decode_bc2(src, w, h),
        DdsFormat::Bc3         => decode_bc3(src, w, h),
        DdsFormat::Bc4         => decode_bc4(src, w, h),
        DdsFormat::Bc5         => decode_bc5(src, w, h),
        DdsFormat::Bc6h        => decode_bc6h(src, w, h),
        DdsFormat::Bc7         => decode_bc7(src, w, h),
        DdsFormat::Bgra32      => decode_bgra32(src, w, h),
        DdsFormat::Rgba32      => decode_rgba32(src, w, h),
        DdsFormat::Rgb24       => decode_rgb24(src, w, h),
        DdsFormat::Rgba16F     => decode_rgba16f(src, w, h),
        DdsFormat::Rgba32F     => decode_rgba32f(src, w, h),
        DdsFormat::R16F        => decode_r16f(src, w, h),
        DdsFormat::R32F        => decode_r32f(src, w, h),
        DdsFormat::R8          => decode_r8(src, w, h),
        DdsFormat::R10G10B10A2 => decode_r10g10b10a2(src, w, h),
        DdsFormat::Luminance8  => decode_luminance8(src, w, h),
        DdsFormat::Luminance16 => decode_luminance16(src, w, h),
    };

    Ok((w, h, rgba))
}

// ---------------------------------------------------------------------------
// Encoding
// ---------------------------------------------------------------------------

/// Whether `encode_dds_matching` would accept a DDS of this format. True
/// only for BC1/BC3/BC4/BC5 today -- the four block formats with
/// implementations in this module. Used by the FS layer to decide whether
/// to expose a `.dds.png/` virtual entry: read-only previews of files we
/// can't write back are a trap (the user's PNG edit silently fails on save
/// because no encoder matches), so we hide HDR / BC6H / BC7 / float / etc.
/// from the listing entirely.
///
/// Cheap: only reads the 128-byte DDS header (plus 20-byte DX10 extension
/// when present). Returns false on parse error.
pub fn is_encodable_format(data: &[u8]) -> bool {
    match parse_header(data) {
        Ok(hdr) => matches!(
            hdr.format,
            DdsFormat::Bc1 | DdsFormat::Bc3 | DdsFormat::Bc4 | DdsFormat::Bc5,
        ),
        Err(_) => false,
    }
}

/// Re-encode RGBA bytes to a DDS file matching the format of `original_dds`.
///
/// Supported output formats: BC1/DXT1, BC3/DXT5, BC4U, BC5U.
/// Returns an error for unsupported formats (BC6H, BC7, uncompressed, etc.).
pub fn encode_dds_matching(rgba: &[u8], w: u32, h: u32, original_dds: &[u8]) -> Result<Vec<u8>> {
    let hdr = parse_header(original_dds)?;
    let blocks = match hdr.format {
        DdsFormat::Bc1 => enc_bc1(rgba, w, h),
        DdsFormat::Bc3 => enc_bc3(rgba, w, h),
        DdsFormat::Bc4 => enc_bc4(rgba, w, h, 0),
        DdsFormat::Bc5 => enc_bc5(rgba, w, h),
        _ => return Err(ParseError::Other(format!(
            "encode_dds_matching: unsupported format {:?}", hdr.format
        ))),
    };
    let fourcc: &[u8; 4] = match hdr.format {
        DdsFormat::Bc1 => b"DXT1",
        DdsFormat::Bc3 => b"DXT5",
        DdsFormat::Bc4 => b"BC4U",
        DdsFormat::Bc5 => b"BC5U",
        _ => unreachable!(),
    };
    Ok(make_dds(w, h, fourcc, &blocks))
}

fn make_dds(w: u32, h: u32, fourcc: &[u8; 4], blocks: &[u8]) -> Vec<u8> {
    let mut out = vec![0u8; 128 + blocks.len()];
    out[0..4].copy_from_slice(b"DDS ");
    out[4..8].copy_from_slice(&124u32.to_le_bytes());   // dwSize
    out[8..12].copy_from_slice(&0x0002_1007u32.to_le_bytes()); // CAPS|H|W|PF|LINEARSIZE
    out[12..16].copy_from_slice(&h.to_le_bytes());
    out[16..20].copy_from_slice(&w.to_le_bytes());
    out[20..24].copy_from_slice(&(blocks.len() as u32).to_le_bytes());
    out[28..32].copy_from_slice(&1u32.to_le_bytes()); // mipMapCount
    out[76..80].copy_from_slice(&32u32.to_le_bytes()); // ddspf.dwSize
    out[80..84].copy_from_slice(&4u32.to_le_bytes());  // DDPF_FOURCC
    out[84..88].copy_from_slice(fourcc);
    out[108..112].copy_from_slice(&0x1000u32.to_le_bytes()); // DDSCAPS_TEXTURE
    out[128..].copy_from_slice(blocks);
    out
}

fn enc_bc1(rgba: &[u8], w: u32, h: u32) -> Vec<u8> {
    use rayon::prelude::*;
    let bw = (w + 3) / 4;
    let mut out = vec![0u8; ((w+3)/4 * (h+3)/4 * 8) as usize];
    out.par_chunks_mut(8).enumerate().for_each(|(i, slot)| {
        let block = extract_4x4(rgba, w, h, (i % bw as usize) as u32 * 4, (i / bw as usize) as u32 * 4);
        slot.copy_from_slice(&bc1_block(&block));
    });
    out
}

fn enc_bc4(rgba: &[u8], w: u32, h: u32, ch: usize) -> Vec<u8> {
    use rayon::prelude::*;
    let bw = (w + 3) / 4;
    let mut out = vec![0u8; ((w+3)/4 * (h+3)/4 * 8) as usize];
    out.par_chunks_mut(8).enumerate().for_each(|(i, slot)| {
        let block = extract_4x4(rgba, w, h, (i % bw as usize) as u32 * 4, (i / bw as usize) as u32 * 4);
        slot.copy_from_slice(&bc4_block(&block, ch));
    });
    out
}

fn enc_bc5(rgba: &[u8], w: u32, h: u32) -> Vec<u8> {
    use rayon::prelude::*;
    let bw = (w + 3) / 4;
    let mut out = vec![0u8; ((w+3)/4 * (h+3)/4 * 16) as usize];
    out.par_chunks_mut(16).enumerate().for_each(|(i, slot)| {
        let block = extract_4x4(rgba, w, h, (i % bw as usize) as u32 * 4, (i / bw as usize) as u32 * 4);
        slot[..8].copy_from_slice(&bc4_block(&block, 0));
        slot[8..].copy_from_slice(&bc4_block(&block, 1));
    });
    out
}

fn enc_bc3(rgba: &[u8], w: u32, h: u32) -> Vec<u8> {
    use rayon::prelude::*;
    let bw = (w + 3) / 4;
    let mut out = vec![0u8; ((w+3)/4 * (h+3)/4 * 16) as usize];
    out.par_chunks_mut(16).enumerate().for_each(|(i, slot)| {
        let block = extract_4x4(rgba, w, h, (i % bw as usize) as u32 * 4, (i / bw as usize) as u32 * 4);
        slot[..8].copy_from_slice(&bc4_block(&block, 3));
        slot[8..].copy_from_slice(&bc1_block(&block));
    });
    out
}

fn extract_4x4(rgba: &[u8], w: u32, h: u32, bx: u32, by: u32) -> [u8; 64] {
    let mut block = [0u8; 64];
    for py in 0..4u32 {
        for px in 0..4u32 {
            let sx = (bx + px).min(w - 1);
            let sy = (by + py).min(h - 1);
            let src = ((sy * w + sx) * 4) as usize;
            let dst = ((py * 4 + px) * 4) as usize;
            block[dst..dst+4].copy_from_slice(&rgba[src..src+4]);
        }
    }
    block
}

fn bc1_block(block: &[u8; 64]) -> [u8; 8] {
    // PCA color axis: one power-iteration step from the covariance matrix.
    let mut mean = [0f32; 3];
    for i in 0..16 {
        mean[0] += block[i*4] as f32;
        mean[1] += block[i*4+1] as f32;
        mean[2] += block[i*4+2] as f32;
    }
    mean[0] /= 16.0; mean[1] /= 16.0; mean[2] /= 16.0;

    let mut cov = [0f32; 6];
    for i in 0..16 {
        let (dr, dg, db) = (block[i*4] as f32 - mean[0], block[i*4+1] as f32 - mean[1], block[i*4+2] as f32 - mean[2]);
        cov[0] += dr*dr; cov[1] += dg*dg; cov[2] += db*db;
        cov[3] += dr*dg; cov[4] += dr*db; cov[5] += dg*db;
    }
    let mut axis = [cov[0]+cov[3]+cov[4], cov[1]+cov[3]+cov[5], cov[2]+cov[4]+cov[5]];
    let len = (axis[0]*axis[0] + axis[1]*axis[1] + axis[2]*axis[2]).sqrt();
    if len < 1e-6 { axis = [1.0, 1.0, 1.0]; } else { axis[0] /= len; axis[1] /= len; axis[2] /= len; }

    let (mut lo, mut hi) = (f32::MAX, f32::MIN);
    let (mut lc, mut hc) = ([0u8;3], [0u8;3]);
    for i in 0..16 {
        let (r, g, b) = (block[i*4] as f32, block[i*4+1] as f32, block[i*4+2] as f32);
        let t = (r-mean[0])*axis[0] + (g-mean[1])*axis[1] + (b-mean[2])*axis[2];
        if t < lo { lo = t; lc = [r as u8, g as u8, b as u8]; }
        if t > hi { hi = t; hc = [r as u8, g as u8, b as u8]; }
    }

    let c0 = rgb_to_565(hc[0], hc[1], hc[2]);
    let c1 = rgb_to_565(lc[0], lc[1], lc[2]);
    let palette = if c0 >= c1 {
        [rgb565_to_888(c0), rgb565_to_888(c1),
         lerp3_rgb(rgb565_to_888(c0), rgb565_to_888(c1), 2, 1),
         lerp3_rgb(rgb565_to_888(c0), rgb565_to_888(c1), 1, 2)]
    } else {
        [rgb565_to_888(c0), rgb565_to_888(c1),
         lerp3_rgb(rgb565_to_888(c0), rgb565_to_888(c1), 1, 1),
         [0,0,0]]
    };
    let mut idx = 0u32;
    for i in 0..16 {
        let (r, g, b) = (block[i*4] as i32, block[i*4+1] as i32, block[i*4+2] as i32);
        let best = (0..4usize).min_by_key(|&j| {
            let dr = r - palette[j][0] as i32;
            let dg = g - palette[j][1] as i32;
            let db = b - palette[j][2] as i32;
            dr*dr + dg*dg + db*db
        }).unwrap();
        idx |= (best as u32) << (i * 2);
    }
    let mut out = [0u8; 8];
    out[0..2].copy_from_slice(&c0.to_le_bytes());
    out[2..4].copy_from_slice(&c1.to_le_bytes());
    out[4..8].copy_from_slice(&idx.to_le_bytes());
    out
}

fn bc4_block(block: &[u8; 64], ch: usize) -> [u8; 8] {
    let vals: [u8; 16] = std::array::from_fn(|i| block[i*4 + ch]);
    let r0 = *vals.iter().max().unwrap();
    let r1 = *vals.iter().min().unwrap();
    let refs: [u8; 8] = [r0, r1,
        ((6*r0 as u32 + 1*r1 as u32)/7) as u8, ((5*r0 as u32 + 2*r1 as u32)/7) as u8,
        ((4*r0 as u32 + 3*r1 as u32)/7) as u8, ((3*r0 as u32 + 4*r1 as u32)/7) as u8,
        ((2*r0 as u32 + 5*r1 as u32)/7) as u8, ((1*r0 as u32 + 6*r1 as u32)/7) as u8,
    ];
    let mut bits: u64 = 0;
    for i in 0..16 {
        let v = vals[i] as i32;
        let idx = (0..8usize).min_by_key(|&j| (v - refs[j] as i32).abs()).unwrap();
        bits |= (idx as u64) << (i * 3);
    }
    [r0, r1,
     (bits & 0xff) as u8, ((bits>>8) & 0xff) as u8, ((bits>>16) & 0xff) as u8,
     ((bits>>24) & 0xff) as u8, ((bits>>32) & 0xff) as u8, ((bits>>40) & 0xff) as u8]
}

fn rgb_to_565(r: u8, g: u8, b: u8) -> u16 {
    ((r as u16 >> 3) << 11) | ((g as u16 >> 2) << 5) | (b as u16 >> 3)
}

fn rgb565_to_888(v: u16) -> [u8; 3] {
    let r = ((v >> 11) & 0x1f) as u8;
    let g = ((v >> 5)  & 0x3f) as u8;
    let b = (v & 0x1f) as u8;
    [(r << 3)|(r >> 2), (g << 2)|(g >> 4), (b << 3)|(b >> 2)]
}

fn lerp3_rgb(a: [u8;3], b: [u8;3], wa: u32, wb: u32) -> [u8; 3] {
    let t = wa + wb;
    [((a[0] as u32*wa + b[0] as u32*wb)/t) as u8,
     ((a[1] as u32*wa + b[1] as u32*wb)/t) as u8,
     ((a[2] as u32*wa + b[2] as u32*wb)/t) as u8]
}
