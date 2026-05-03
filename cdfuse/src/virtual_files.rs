//! Virtual file layer for the FUSE mount.
//!
//! Hidden root directories injected at the filesystem root, each mirroring
//! the full VFS directory tree and exposing matching files as JSONL:
//!
//!   .paloc.jsonl/        every .paloc localisation file
//!   .pabgb.jsonl/        every .pabgb game-data table (+ paired .pabgh)
//!   .prefab.jsonl/       every .prefab scene/character descriptor
//!   .paa_metabin.jsonl/  every .paa_metabin animation metadata file
//!   .nav.jsonl/          every .nav navigation mesh
//!
//! Example:
//!   real  gamedata/text/ui.paloc
//!   view  .paloc.jsonl/gamedata/text/ui.paloc
//!
//! Path taxonomy
//! -------------
//!   resolve(path)         -> Some if path is a readable virtual file
//!   resolve_virtual_dir() -> Some if path is a virtual directory
//!   virtual_root_dirs()   -> iterator over the top-level virtual dir names

use log::warn;
use cdcore::formats::dds::decode_dds_to_rgba;
use image_dds::{dds_from_image, dds_image_format, Mipmaps, Quality};
use cdcore::formats::data::{parse_paloc, serialize_paloc, PalocEntry, parse_pabgb, FieldValue};
use cdcore::formats::scene::parse_prefab;
use cdcore::formats::animation::parse_paa_metabin;
use cdcore::formats::physics::parse_nav;
use cdcore::formats::mesh::{parse_pam, parse_pamlod, parse_pac, submeshes_to_fbx};

// -- Constants -----------------------------------------------------------------

/// (virtual_root_name, source_ext, suffix)
///
/// suffix is appended to the full source filename in the virtual tree.
/// "" for all JSONL roots (filename unchanged).
/// ".png" for .dds.png/ so bitmap_bell.dds -> bitmap_bell.dds.png and
/// file managers pick the right MIME type without ambiguity.
static VIRTUAL_ROOTS: &[(&str, &str, &str)] = &[
    (".paloc.jsonl",       ".paloc",       ".jsonl"),
    // (".pabgb.jsonl",       ".pabgb",       ".jsonl"),     // disabled
    // (".prefab.jsonl",      ".prefab",      ".jsonl"),     // disabled
    // (".paa_metabin.jsonl", ".paa_metabin", ".jsonl"),     // disabled
    // (".nav.jsonl",         ".nav",         ".jsonl"),     // disabled
    (".dds.png",           ".dds",         ".png"),
    (".pam.fbx",           ".pam",         ".fbx"),
    (".pamlod.fbx",        ".pamlod",      ".fbx"),
    (".pac.fbx",           ".pac",         ".fbx"),
];

// -- Public types --------------------------------------------------------------

#[derive(Clone, Copy)]
pub enum VirtualKind {
    PalocJson,
    PabgbJson,
    PrefabJsonl,
    PaaMetabinJsonl,
    NavJsonl,
    DdsPng,
    PamFbx,
    PamlodFbx,
    PacFbx,
}

pub struct VirtualFile {
    pub kind:        VirtualKind,
    pub source_path: String,   // real VFS path to decode
}

pub struct VirtualDirInfo {
    pub real_path:  String,          // matching real directory in VFS (empty = VFS root)
    pub filter_ext: &'static str,    // source extension to match (.dds, .paloc, ...)
    pub suffix:     &'static str,    // appended to virtual filenames ("" or ".png")
}

// -- Routing -------------------------------------------------------------------

/// Iterator over the top-level virtual directory names.
pub fn virtual_root_dirs() -> impl Iterator<Item = &'static str> {
    VIRTUAL_ROOTS.iter().map(|(name, _, _)| *name)
}

/// Map a virtual file path to its source descriptor.
///
/// `.paloc.jsonl/gamedata/localizationstring_ger.paloc`
///     -> VirtualFile { PalocJson, "gamedata/localizationstring_ger.paloc" }
/// `.dds.png/ui/bitmap_bell.dds.png`
///     -> VirtualFile { DdsPng, "ui/bitmap_bell.dds" }
///
/// Returns `None` for virtual directory paths or unrecognised paths.
pub fn resolve(virtual_path: &str) -> Option<VirtualFile> {
    for &(vdir, src_ext, suffix) in VIRTUAL_ROOTS {
        let Some(rest) = virtual_path.strip_prefix(vdir).and_then(|s| s.strip_prefix('/'))
            else { continue };
        // Virtual filename = source_name + suffix.
        // Strip suffix to recover source_name, then verify it ends with src_ext.
        let source = if suffix.is_empty() {
            if !rest.ends_with(src_ext) { continue; }
            rest
        } else {
            match rest.strip_suffix(suffix) {
                Some(s) if s.ends_with(src_ext) => s,
                _ => continue,
            }
        };
        return Some(VirtualFile { kind: kind_for(src_ext), source_path: source.to_string() });
    }
    None
}

/// Map a virtual directory path to info about the real directory it mirrors.
///
/// `.paloc.jsonl`   -> VirtualDirInfo { real_path: "",   filter_ext: ".paloc", suffix: ""     }
/// `.dds.png/ui`    -> VirtualDirInfo { real_path: "ui", filter_ext: ".dds",   suffix: ".png" }
///
/// Returns `None` for virtual file paths or unrecognised paths.
pub fn resolve_virtual_dir(path: &str) -> Option<VirtualDirInfo> {
    for &(vdir, src_ext, suffix) in VIRTUAL_ROOTS {
        if path == vdir {
            return Some(VirtualDirInfo { real_path: String::new(), filter_ext: src_ext, suffix });
        }
        let Some(rest) = path.strip_prefix(vdir).and_then(|s| s.strip_prefix('/'))
            else { continue };
        // It is a directory unless the path looks like a virtual file
        // (ends with {src_ext}{suffix}).
        let is_file = if suffix.is_empty() {
            rest.ends_with(src_ext)
        } else {
            rest.strip_suffix(suffix).is_some_and(|s| s.ends_with(src_ext))
        };
        if !is_file {
            return Some(VirtualDirInfo { real_path: rest.to_string(), filter_ext: src_ext, suffix });
        }
    }
    None
}

fn kind_for(ext: &str) -> VirtualKind {
    match ext {
        ".paloc"       => VirtualKind::PalocJson,
        ".pabgb"       => VirtualKind::PabgbJson,
        ".prefab"      => VirtualKind::PrefabJsonl,
        ".paa_metabin" => VirtualKind::PaaMetabinJsonl,
        ".nav"         => VirtualKind::NavJsonl,
        ".dds"         => VirtualKind::DdsPng,
        ".pam"         => VirtualKind::PamFbx,
        ".pamlod"      => VirtualKind::PamlodFbx,
        ".pac"         => VirtualKind::PacFbx,
        _              => unreachable!("unknown virtual ext: {ext}"),
    }
}

// -- Renderers -----------------------------------------------------------------

/// Decode a PALOC binary and return UTF-8 JSON bytes.
pub fn render_paloc(data: &[u8], path: &str) -> Option<Vec<u8>> {
    let parsed = parse_paloc(data, path).map_err(|e| warn!("render_paloc {path}: {e}")).ok()?;
    let mut out = String::new();
    for entry in &parsed.entries {
        out.push_str("{\"key\": ");
        push_json_str(&mut out, &entry.key);
        out.push_str(", \"value\": ");
        push_json_str(&mut out, &entry.value);
        out.push_str("}\n");
    }
    Some(out.into_bytes())
}

/// Decode a PABGB binary pair and return UTF-8 JSON bytes.
pub fn render_pabgb(pabgh_data: &[u8], pabgb_data: &[u8], path: &str) -> Option<Vec<u8>> {
    let table = parse_pabgb(pabgh_data, pabgb_data, path).map_err(|e| warn!("render_pabgb {path}: {e}")).ok()?;
    let mut out = String::new();
    for row in &table.rows {
        out.push_str("{\"index\": ");
        out.push_str(&row.index.to_string());
        if row.row_hash != 0 {
            out.push_str(", \"hash\": \"0x");
            out.push_str(&format!("{:08X}", row.row_hash));
            out.push('"');
        }
        out.push_str(", \"name\": ");
        push_json_str(&mut out, &row.name);
        out.push_str(", \"fields\": [");
        let last_field = row.fields.len().saturating_sub(1);
        for (fi, field) in row.fields.iter().enumerate() {
            out.push_str("{\"offset\": ");
            out.push_str(&field.offset.to_string());
            out.push_str(", \"value\": ");
            push_json_field_value(&mut out, &field.value);
            out.push('}');
            if fi < last_field { out.push_str(", "); }
        }
        out.push_str("]}\n");
    }
    Some(out.into_bytes())
}

/// Decode a prefab and return UTF-8 JSONL -- one line per string entry.
pub fn render_prefab(data: &[u8], path: &str) -> Option<Vec<u8>> {
    let parsed = parse_prefab(data, path).map_err(|e| warn!("render_prefab {path}: {e}")).ok()?;
    let mut out = String::new();
    for s in &parsed.strings {
        let kind = match s.kind {
            cdcore::formats::scene::PrefabStringKind::FileRef      => "FileRef",
            cdcore::formats::scene::PrefabStringKind::EnumTag      => "EnumTag",
            cdcore::formats::scene::PrefabStringKind::PropertyName => "PropertyName",
            cdcore::formats::scene::PrefabStringKind::Unknown      => "Unknown",
        };
        out.push_str("{\"kind\": \"");
        out.push_str(kind);
        out.push_str("\", \"value\": ");
        push_json_str(&mut out, &s.value);
        out.push_str("}\n");
    }
    Some(out.into_bytes())
}

/// Decode a paa_metabin and return UTF-8 JSONL -- one line per record.
pub fn render_paa_metabin(data: &[u8], path: &str) -> Option<Vec<u8>> {
    let parsed = parse_paa_metabin(data, path).map_err(|e| warn!("render_paa_metabin {path}: {e}")).ok()?;
    let mut out = String::new();
    for r in &parsed.records {
        out.push_str("{\"offset\": ");
        out.push_str(&r.offset.to_string());
        out.push_str(", \"subtype\": ");
        out.push_str(&r.subtype.to_string());
        out.push_str(", \"tag\": ");
        out.push_str(&r.tag.to_string());
        out.push_str(", \"payload\": \"");
        for b in &r.payload { out.push_str(&format!("{b:02x}")); }
        out.push_str("\"}\n");
    }
    Some(out.into_bytes())
}

/// Decode a nav mesh and return UTF-8 JSONL -- one line per cell.
pub fn render_nav(data: &[u8], path: &str) -> Option<Vec<u8>> {
    let parsed = parse_nav(data, path).map_err(|e| warn!("render_nav {path}: {e}")).ok()?;
    let mut out = String::new();
    for c in &parsed.cells {
        out.push_str("{\"cell_id\": ");
        out.push_str(&c.cell_id.to_string());
        out.push_str(", \"grid_ref\": \"0x");
        out.push_str(&format!("{:08X}", c.grid_ref));
        out.push_str("\", \"flags\": \"0x");
        out.push_str(&format!("{:08X}", c.flags));
        out.push_str("\", \"neighbor\": ");
        out.push_str(&c.neighbor.to_string());
        out.push_str(", \"tile_x\": ");
        out.push_str(&c.tile_x.to_string());
        out.push_str("}\n");
    }
    Some(out.into_bytes())
}

/// Decode a DDS texture and return PNG bytes (RGBA8).
pub fn render_dds_png(data: &[u8], path: &str) -> Option<Vec<u8>> {
    let (width, height, rgba) = decode_dds_to_rgba(data)
        .map_err(|e| warn!("render_dds_png {path}: {e}")).ok()?;

    let mut buf = Vec::new();
    let mut enc = png::Encoder::new(&mut buf, width, height);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    let mut writer = enc.write_header()
        .map_err(|e| warn!("render_dds_png {path}: png header: {e}")).ok()?;
    writer.write_image_data(&rgba)
        .map_err(|e| warn!("render_dds_png {path}: png data: {e}")).ok()?;
    drop(writer);
    Some(buf)
}

/// Re-encode a PNG back to DDS, preserving the format of the original DDS.
///
/// Reads the DXGI/FourCC format from original_dds_data, decodes the PNG to
/// RGBA, then re-encodes using image-dds so the output has the same pixel
/// format (BC7, BC1, RGBA8, etc.) as the source file.
pub fn parse_png_to_dds(png_data: &[u8], original_dds_data: &[u8], path: &str) -> Option<Vec<u8>> {
    // Decode PNG to RgbaImage (image_dds expects this type).
    let img = image_dds::image::load_from_memory_with_format(
            png_data, image_dds::image::ImageFormat::Png)
        .map_err(|e| warn!("parse_png_to_dds {path}: PNG decode: {e}")).ok()?
        .into_rgba8();

    // Extract the target format from the original DDS header.
    let orig = image_dds::ddsfile::Dds::read(&mut std::io::Cursor::new(original_dds_data))
        .map_err(|e| warn!("parse_png_to_dds {path}: original DDS parse: {e}")).ok()?;
    let fmt = dds_image_format(&orig)
        .map_err(|e| warn!("parse_png_to_dds {path}: get format: {e:?}")).ok()?;

    // Re-encode. GeneratedAutomatic matches the mip chain games expect.
    // Arg order: (image, format, quality, mipmaps)
    let dds = dds_from_image(&img, fmt, Quality::Normal, Mipmaps::GeneratedAutomatic)
        .map_err(|e| warn!("parse_png_to_dds {path}: encode ({fmt:?}): {e}")).ok()?;

    let mut out = Vec::new();
    dds.write(&mut out)
        .map_err(|e| warn!("parse_png_to_dds {path}: write DDS: {e}")).ok()?;
    Some(out)
}

// -- Mesh → FBX renderers -------------------------------------------------------

pub fn render_pam_fbx(data: &[u8], path: &str) -> Option<Vec<u8>> {
    let mesh = parse_pam(data, path).map_err(|e| warn!("render_pam_fbx {path}: {e}")).ok()?;
    let refs: Vec<&_> = mesh.submeshes.iter().collect();
    let name = std::path::Path::new(path).file_stem()
        .and_then(|s| s.to_str()).unwrap_or(path);
    Some(submeshes_to_fbx(&refs, name))
}

pub fn render_pamlod_fbx(data: &[u8], path: &str) -> Option<Vec<u8>> {
    let mesh = parse_pamlod(data, path).map_err(|e| warn!("render_pamlod_fbx {path}: {e}")).ok()?;
    let refs: Vec<&_> = mesh.submeshes.iter().collect();
    let name = std::path::Path::new(path).file_stem()
        .and_then(|s| s.to_str()).unwrap_or(path);
    Some(submeshes_to_fbx(&refs, name))
}

pub fn render_pac_fbx(data: &[u8], path: &str) -> Option<Vec<u8>> {
    let mesh = parse_pac(data, path).map_err(|e| warn!("render_pac_fbx {path}: {e}")).ok()?;
    let refs: Vec<&_> = mesh.submeshes.iter().map(|s| &s.base).collect();
    let name = std::path::Path::new(path).file_stem()
        .and_then(|s| s.to_str()).unwrap_or(path);
    Some(submeshes_to_fbx(&refs, name))
}

// -- Write-back: JSONL -> binary -----------------------------------------------

/// Parse PALOC JSONL (one `{"key":...,"value":...}` per line) back to binary.
pub fn parse_paloc_jsonl(data: &[u8]) -> Option<Vec<u8>> {
    let text = std::str::from_utf8(data)
        .map_err(|e| warn!("parse_paloc_jsonl: invalid UTF-8: {e}")).ok()?;
    let mut entries = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() { continue; }
        let key   = extract_json_field(line, "\"key\"")
            .unwrap_or_else(|| { warn!("parse_paloc_jsonl: missing key in: {line}"); String::new() });
        let value = extract_json_field(line, "\"value\"")
            .unwrap_or_else(|| { warn!("parse_paloc_jsonl: missing value in: {line}"); String::new() });
        entries.push(PalocEntry { key, value, key_offset: 0, value_offset: 0 });
    }
    Some(serialize_paloc(&entries))
}

/// Extract the string value of a named field from a flat JSON object line.
fn extract_json_field(line: &str, field: &str) -> Option<String> {
    let pos    = line.find(field)?;
    let after  = &line[pos + field.len()..];
    let colon  = after.find(':')? + 1;
    parse_json_string(after[colon..].trim_start())
}

fn parse_json_string(s: &str) -> Option<String> {
    if !s.starts_with('"') { return None; }
    let mut out   = String::new();
    let mut chars = s[1..].chars();
    loop {
        match chars.next()? {
            '"'  => return Some(out),
            '\\' => match chars.next()? {
                '"'  => out.push('"'),
                '\\' => out.push('\\'),
                'n'  => out.push('\n'),
                'r'  => out.push('\r'),
                't'  => out.push('\t'),
                'u'  => {
                    let hex: String = (0..4).filter_map(|_| chars.next()).collect();
                    out.push(char::from_u32(u32::from_str_radix(&hex, 16).ok()?)?)
                }
                c    => out.push(c),
            },
            c => out.push(c),
        }
    }
}

// -- JSON helpers --------------------------------------------------------------

fn push_json_str(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"'  => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => { out.push_str(&format!("\\u{:04x}", c as u32)); }
            c    => out.push(c),
        }
    }
    out.push('"');
}

fn push_json_field_value(out: &mut String, v: &FieldValue) {
    match v {
        FieldValue::U32(n) => out.push_str(&n.to_string()),
        FieldValue::I32(n) => out.push_str(&n.to_string()),
        FieldValue::F32(f) => out.push_str(&format!("{f:.4}")),
        FieldValue::Str(s) => push_json_str(out, s),
        FieldValue::Blob(b) => {
            let hex: String = b.iter().take(20).map(|x| format!("{x:02x}")).collect();
            let disp = if b.len() > 20 { format!("{hex}...") } else { hex };
            push_json_str(out, &disp);
        }
    }
}
