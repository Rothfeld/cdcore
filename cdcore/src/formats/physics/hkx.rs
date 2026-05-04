//! Havok TAG0 binary tagfile parser (Havok SDK 2024.2).
//!
//! Every section is an 8-byte header followed by its body:
//!   [0:4] u32 BE: top 4 bits = flags, low 28 bits = total section size
//!                 (size includes this 8-byte header)
//!                 flag 0x4 (bit 30) = leaf -- body is raw bytes, no children
//!   [4:8] char[4] ASCII tag (e.g. "TAG0", "SDKV", "DATA", "TYPE")
//!
//! Root section is always TAG0. Child sections follow immediately inside it.

use crate::error::{ParseError, Result};

const HEADER_SIZE: usize = 8;
const LEAF_FLAG: u32 = 0x4;

#[derive(Debug, Clone)]
pub struct HavokSection {
    pub tag: String,
    pub offset: usize,
    pub size: usize,
    pub flags: u32,
}

impl HavokSection {
    pub fn body_offset(&self) -> usize { self.offset + HEADER_SIZE }
    pub fn body_size(&self)   -> usize { self.size.saturating_sub(HEADER_SIZE) }
    pub fn is_leaf(&self)     -> bool  { self.flags & LEAF_FLAG != 0 }
}

#[derive(Debug, Default, Clone)]
pub struct ParsedHavok {
    pub path: String,
    pub sdk_version: String,
    pub total_size: u32,
    pub sections: Vec<HavokSection>,
    pub class_names: Vec<String>,
    pub has_skeleton: bool,
    pub has_animation: bool,
    pub has_physics: bool,
    pub has_ragdoll: bool,
    pub has_cloth: bool,
    pub has_softbody: bool,
    pub has_mesh_shape: bool,
    pub rigid_body_count: u32,
    pub shape_types: Vec<String>,
    pub binds_to_mesh_topology: bool,
}

pub fn parse(data: &[u8], filename: &str) -> Result<ParsedHavok> {
    if data.len() < HEADER_SIZE {
        return Err(ParseError::eof(0, HEADER_SIZE, data.len()));
    }

    let (root_tag, root_size, _root_flags) = decode_header(data, 0)?;
    if root_tag != "TAG0" {
        return Err(ParseError::magic(b"TAG0", root_tag.as_bytes(), 4));
    }

    let mut result = ParsedHavok {
        path: filename.to_string(),
        total_size: root_size as u32,
        ..Default::default()
    };

    // Walk direct children of the TAG0 root section.
    parse_children(data, HEADER_SIZE, HEADER_SIZE + root_size.min(data.len()), 0, &mut result);

    classify(&mut result);
    Ok(result)
}

fn parse_children(
    data: &[u8],
    start: usize,
    end:   usize,
    depth: usize,
    result: &mut ParsedHavok,
) {
    let end = end.min(data.len());
    let mut off = start;

    while off + HEADER_SIZE <= end {
        let (tag, size, flags) = match decode_header(data, off) {
            Ok(h) => h,
            Err(_) => break,
        };
        if size < HEADER_SIZE { break; }

        let section_end = (off + size).min(data.len());

        match tag.as_str() {
            "SDKV" => {
                let body = &data[off + HEADER_SIZE..section_end.min(off + HEADER_SIZE + 8)];
                result.sdk_version = std::str::from_utf8(body)
                    .unwrap_or("")
                    .trim_end_matches('\0')
                    .to_string();
            }
            "TSTR" | "TST1" | "FSTR" => {
                extract_strings(&data[off + HEADER_SIZE..section_end], result);
            }
            "TYPE" | "INDX" => {
                // Container -- recurse into children
                parse_children(data, off + HEADER_SIZE, section_end, depth + 1, result);
            }
            _ => {}
        }

        result.sections.push(HavokSection {
            tag,
            offset: off,
            size,
            flags,
        });

        off = section_end;
    }
}

fn decode_header(data: &[u8], offset: usize) -> Result<(String, usize, u32)> {
    if offset + HEADER_SIZE > data.len() {
        return Err(ParseError::eof(offset, HEADER_SIZE, data.len().saturating_sub(offset)));
    }
    let raw = u32::from_be_bytes(data[offset..offset + 4].try_into().unwrap());
    let flags = (raw >> 28) & 0xF;
    let size  = (raw & 0x0FFFFFFF) as usize;
    let tag   = std::str::from_utf8(&data[offset + 4..offset + 8])
        .unwrap_or("????")
        .to_string();
    Ok((tag, size, flags))
}

fn extract_strings(body: &[u8], result: &mut ParsedHavok) {
    let mut pos = 0;
    while pos < body.len() {
        let nul = body[pos..].iter().position(|&b| b == 0).unwrap_or(body.len() - pos);
        let s = std::str::from_utf8(&body[pos..pos + nul]).unwrap_or("");
        if !s.is_empty() && s.starts_with("hk") && s.len() > 2 {
            result.class_names.push(s.to_string());
        }
        pos += nul + 1;
    }
}

fn classify(r: &mut ParsedHavok) {
    let names = &r.class_names;
    r.has_skeleton   = names.iter().any(|c| c.contains("Skeleton") || c == "hkaSkeleton");
    r.has_animation  = names.iter().any(|c| c.contains("Animation"));
    r.has_physics    = names.iter().any(|c| c.contains("RigidBody") || c.contains("hkpShape"));
    r.has_ragdoll    = names.iter().any(|c| c.contains("Ragdoll"));
    r.has_cloth      = names.iter().any(|c| c.contains("Cloth") || c.contains("cloth"));
    r.has_softbody   = names.iter().any(|c| c.contains("SoftBody") || c.contains("NavMesh"));
    r.has_mesh_shape = names.iter().any(|c| c == "hkpMeshShape");
    r.shape_types    = names.iter().filter(|c| c.contains("Shape")).cloned().collect();
    r.binds_to_mesh_topology = r.has_mesh_shape;
}
