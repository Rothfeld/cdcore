//! User-created file management via a dedicated package group.
//!
//! Cdfuse exposes `create()` / `unlink()` for new files. Those files live in
//! their own package group (default `9000`) so they never collide with a
//! shipped PAZ slot. The game sees them once `0.papgt` references the group.
//!
//! On-disk layout under `<packages>/<group_id>/`:
//!   `0.pamt` - serialized index (header + 1 PAZ entry + flat node table + file records)
//!   `0.paz`  - encrypted+compressed blobs concatenated, 16-byte aligned
//!
//! We treat the whole virtual path as a single node name (parent = root).
//! Names cap at 255 bytes (PAMT name_len is u8); paths longer than that
//! aren't representable. Folder hierarchy isn't reconstructed -- the engine's
//! parser joins parent chains with `""`, so `"character/foo.pac"` as a single
//! name reads back identically.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::archive::pamt::{parse_pamt_bytes, PamtFileEntry, PazTableEntry};
use crate::archive::papgt::{parse_papgt, update_pamt_crc, update_self_crc as papgt_update_self_crc};
use crate::archive::paz;
use crate::compression::{self, COMP_LZ4};
use crate::crypto::{self, pa_checksum};
use crate::error::{ParseError, Result};

/// Maximum PAMT node name length (PAMT records `name_len` as `u8`).
pub const MAX_USER_PATH_LEN: usize = 255;

/// One file managed by the user group.
#[derive(Debug, Clone)]
pub struct UserFile {
    pub path: String,
    pub paz_offset: u64,
    pub comp_size: u32,
    pub orig_size: u32,
    pub compression_type: u8,
}

/// Open or bootstrap the user group on disk and keep its file list in sync.
pub struct UserGroup {
    pub group_id: String,
    pub packages_path: PathBuf,
    pub group_dir: PathBuf,
    pub pamt_path: PathBuf,
    pub paz_path: PathBuf,
    pub papgt_path: PathBuf,
    pub files: Vec<UserFile>,
}

impl UserGroup {
    /// Resolve all paths and load the existing user-group state. Bootstraps
    /// fresh PAMT + PAZ + PAPGT entry if the group dir is missing.
    pub fn open_or_create(packages_path: &Path, papgt_path: &Path, group_id: &str) -> Result<Self> {
        if !group_id.chars().all(|c| c.is_ascii_digit()) || group_id.len() != 4 {
            return Err(ParseError::Other(format!(
                "user group id must be a 4-digit number, got '{group_id}'"
            )));
        }
        let group_dir = packages_path.join(group_id);
        let pamt_path = group_dir.join("0.pamt");
        let paz_path = group_dir.join("0.paz");

        let mut ug = UserGroup {
            group_id: group_id.to_string(),
            packages_path: packages_path.to_path_buf(),
            group_dir: group_dir.clone(),
            pamt_path: pamt_path.clone(),
            paz_path: paz_path.clone(),
            papgt_path: papgt_path.to_path_buf(),
            files: Vec::new(),
        };

        if !group_dir.exists() {
            fs::create_dir_all(&group_dir)?;
        }
        if !paz_path.exists() {
            fs::write(&paz_path, &[] as &[u8])?;
        }
        if !pamt_path.exists() {
            // Fresh bootstrap: empty PAMT + matching PAPGT entry.
            ug.flush_pamt()?;
            ug.ensure_papgt_entry()?;
        } else {
            ug.load_existing()?;
            // The existing PAMT may pre-date our bootstrap helper; still
            // make sure PAPGT references it with the current PAMT CRC.
            ug.ensure_papgt_entry()?;
        }
        Ok(ug)
    }

    /// Whether `path` is currently managed by this user group.
    pub fn contains(&self, path: &str) -> bool {
        self.files.iter().any(|f| f.path == path)
    }

    /// Read the original (decrypted, decompressed) bytes for a managed path.
    pub fn read(&self, path: &str) -> Option<Vec<u8>> {
        let f = self.files.iter().find(|f| f.path == path)?;
        let stored_size = if f.compression_type == 0 {
            f.orig_size as usize
        } else {
            f.comp_size as usize
        };
        let mut bytes = paz::read_bytes(self.paz_path.to_str()?, f.paz_offset, stored_size).ok()?;
        if crypto::is_encrypted(&f.path) {
            let basename = Path::new(&f.path).file_name()?.to_str()?;
            crypto::decrypt_inplace(&mut bytes, basename);
        }
        if f.compression_type != 0 {
            bytes = compression::decompress(&bytes, f.orig_size as usize, f.compression_type).ok()?;
        }
        Some(bytes)
    }

    /// Add or replace a file. Encrypts/compresses by extension convention,
    /// appends to PAZ, rewrites PAMT, updates PAPGT.
    pub fn add(&mut self, path: &str, data: &[u8]) -> Result<()> {
        if path.as_bytes().len() > MAX_USER_PATH_LEN {
            return Err(ParseError::Other(format!(
                "user path too long ({} bytes; max {})",
                path.as_bytes().len(), MAX_USER_PATH_LEN,
            )));
        }
        let orig_size = data.len() as u32;
        let mut processed: Vec<u8> = data.to_vec();
        let comp_type: u8 = if should_compress(path) { COMP_LZ4 } else { 0 };
        if comp_type == COMP_LZ4 {
            processed = compression::compress_lz4(&processed);
        }
        if crypto::is_encrypted(path) {
            let basename = Path::new(path).file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(path);
            processed = crypto::encrypt(&processed, basename);
        }
        let comp_size = processed.len() as u32;
        let paz_offset = paz::append_entry(self.paz_path.to_str().unwrap(), &processed)?;

        // Drop any prior entry for the same path; the PAZ blob it pointed at
        // becomes orphaned (no compaction yet).
        self.files.retain(|f| f.path != path);
        self.files.push(UserFile {
            path: path.to_string(),
            paz_offset,
            comp_size,
            orig_size,
            compression_type: comp_type,
        });

        self.flush_pamt()?;
        self.update_papgt_crc()?;
        Ok(())
    }

    /// Drop a managed file. The PAZ data stays orphaned; only the PAMT entry
    /// disappears (and with it, the file's visibility).
    pub fn remove(&mut self, path: &str) -> Result<bool> {
        let before = self.files.len();
        self.files.retain(|f| f.path != path);
        if self.files.len() == before {
            return Ok(false);
        }
        self.flush_pamt()?;
        self.update_papgt_crc()?;
        Ok(true)
    }

    /// All managed file paths.
    pub fn paths(&self) -> Vec<String> {
        self.files.iter().map(|f| f.path.clone()).collect()
    }

    /// Convert a managed file into a [`PamtFileEntry`] suitable for the
    /// in-memory VFS index (so lookups + reads route through user group).
    pub fn entry_for(&self, path: &str) -> Option<PamtFileEntry> {
        let pamt_idx = self.files.iter().position(|f| f.path == path)?;
        // record_offset is computed deterministically by `flush_pamt`.
        let layout = pamt_layout(&self.files);
        let record_offset = layout.file_records_off + pamt_idx * 20;
        let f = &self.files[pamt_idx];
        Some(PamtFileEntry {
            path: f.path.clone(),
            paz_file: self.paz_path.to_string_lossy().into_owned(),
            offset: f.paz_offset,
            comp_size: f.comp_size,
            orig_size: f.orig_size,
            flags: ((f.compression_type as u32) & 0x0F) << 16,
            paz_index: 0,
            record_offset,
        })
    }

    /// All entries as PamtFileEntry (for VFS index injection).
    pub fn all_entries(&self) -> Vec<PamtFileEntry> {
        self.files
            .iter()
            .map(|f| {
                let layout = pamt_layout(&self.files);
                let pamt_idx = self.files.iter().position(|x| x.path == f.path).unwrap();
                let record_offset = layout.file_records_off + pamt_idx * 20;
                PamtFileEntry {
                    path: f.path.clone(),
                    paz_file: self.paz_path.to_string_lossy().into_owned(),
                    offset: f.paz_offset,
                    comp_size: f.comp_size,
                    orig_size: f.orig_size,
                    flags: ((f.compression_type as u32) & 0x0F) << 16,
                    paz_index: 0,
                    record_offset,
                }
            })
            .collect()
    }

    // ── internals ─────────────────────────────────────────────────────────

    fn load_existing(&mut self) -> Result<()> {
        let raw = fs::read(&self.pamt_path)?;
        let pamt = parse_pamt_bytes(
            &raw,
            self.pamt_path.to_str().unwrap(),
            self.group_dir.to_str().unwrap(),
            0, // pamt stem = 0 for "0.pamt"
        )?;
        self.files = pamt
            .file_entries
            .into_iter()
            .map(|e| UserFile {
                path: e.path,
                paz_offset: e.offset,
                comp_size: e.comp_size,
                orig_size: e.orig_size,
                compression_type: ((e.flags >> 16) & 0x0F) as u8,
            })
            .collect();
        Ok(())
    }

    fn flush_pamt(&self) -> Result<()> {
        let paz_size = paz::file_size(self.paz_path.to_str().unwrap()).unwrap_or(0) as u32;
        let paz_crc = match fs::read(&self.paz_path) {
            Ok(b) => pa_checksum(&b),
            Err(_) => 0,
        };
        let bytes = serialize_user_pamt(&self.files, paz_crc, paz_size);
        atomic_write(&self.pamt_path, &bytes)
    }

    fn ensure_papgt_entry(&self) -> Result<()> {
        // Two questions the game cares about:
        //   (a) Does PAPGT have an entry for our group? (slot lookup)
        //   (b) Does that entry's pamt_crc match the on-disk PAMT?
        //
        // PAPGT format is header (12B) + N entries (12B each) + trailer with
        // a `N*5` marker byte and the full list of group-folder names.
        // Growing the entry list shifts the trailer; without a verified
        // serializer for the trailer we don't grow it -- doing so risks
        // corrupting the game's group-name lookup table.
        //
        // Strategy: locate our group's slot if the original PAPGT already
        // has one (rare but possible after a previous bootstrap). If not,
        // skip the update entirely. Files still land in the on-disk 9000
        // PAMT/PAZ and stay visible to cdfuse / other PAMT readers; the
        // live game won't see them until a follow-up PAPGT extension lands.
        let papgt = parse_papgt(self.papgt_path.to_str().unwrap())?;
        let raw_entry_count = real_papgt_entry_count(&papgt.raw_data);
        let group_idx = sorted_group_index(&self.packages_path, &self.group_id)?;
        if group_idx >= raw_entry_count {
            log::warn!(
                "user_group {}: PAPGT has only {} real entries; not growing trailer to add slot {}. \
                 Game will not see user files until PAPGT extension is implemented.",
                self.group_id, raw_entry_count, group_idx,
            );
            return Ok(());
        }
        let mut raw = papgt.raw_data.clone();
        let crc_off = papgt.groups[group_idx].crc_offset;
        let pamt_crc = current_pamt_self_crc(&self.pamt_path)?;
        update_pamt_crc(&mut raw, crc_off, pamt_crc);
        papgt_update_self_crc(&mut raw);
        atomic_write(&self.papgt_path, &raw)
    }

    fn update_papgt_crc(&self) -> Result<()> {
        // After a PAMT rewrite, refresh its CRC in PAPGT (if the slot exists).
        self.ensure_papgt_entry()
    }
}

// ── PAMT serializer (flat-path layout) ────────────────────────────────────

struct PamtLayout {
    paz_table_off: usize,
    folder_size_off: usize,
    node_size_off: usize,
    nodes_off: usize,
    folder_count_off: usize,
    file_records_off: usize,
}

fn pamt_layout(files: &[UserFile]) -> PamtLayout {
    let paz_table_off = 16;
    let folder_size_off = paz_table_off + 8; // one PAZ entry, no separator
    let node_size_off = folder_size_off + 4; // folder section is empty
    let nodes_off = node_size_off + 4;
    let nodes_size: usize = files.iter().map(|f| 5 + f.path.as_bytes().len()).sum();
    let folder_count_off = nodes_off + nodes_size;
    // folder_count(4) + hash(4) + folder_count*16 (= 16 for our single-root)
    let file_records_off = folder_count_off + 4 + 4 + 16;
    PamtLayout {
        paz_table_off,
        folder_size_off,
        node_size_off,
        nodes_off,
        folder_count_off,
        file_records_off,
    }
}

/// Build a fresh PAMT from a flat file list.
pub fn serialize_user_pamt(files: &[UserFile], paz_crc: u32, paz_size: u32) -> Vec<u8> {
    let layout = pamt_layout(files);
    let nodes_size: usize = files.iter().map(|f| 5 + f.path.as_bytes().len()).sum();
    let total_size = layout.file_records_off + files.len() * 20;
    let mut buf = vec![0u8; total_size];

    // Header: self_crc[0..4], paz_count[4..8], 8 bytes of zero/hash region.
    buf[4..8].copy_from_slice(&1u32.to_le_bytes()); // paz_count = 1
    // Hash region [8..16]: shipping PAMTs put a small fingerprint here that
    // the parser ignores; zero is safe and matches what an empty rebuild
    // would emit.
    buf[8..16].copy_from_slice(&[0u8; 8]);

    // PAZ table: 1 entry, no trailing separator.
    buf[layout.paz_table_off..layout.paz_table_off + 4].copy_from_slice(&paz_crc.to_le_bytes());
    buf[layout.paz_table_off + 4..layout.paz_table_off + 8].copy_from_slice(&paz_size.to_le_bytes());

    // Folder section: empty.
    buf[layout.folder_size_off..layout.folder_size_off + 4].copy_from_slice(&0u32.to_le_bytes());

    // Node section: one node per file, parent=0xFFFFFFFF, name=full path.
    buf[layout.node_size_off..layout.node_size_off + 4]
        .copy_from_slice(&(nodes_size as u32).to_le_bytes());
    let mut cursor = layout.nodes_off;
    let mut node_offsets: Vec<usize> = Vec::with_capacity(files.len());
    for f in files {
        node_offsets.push(cursor - layout.nodes_off);
        let name = f.path.as_bytes();
        buf[cursor..cursor + 4].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        buf[cursor + 4] = name.len() as u8;
        buf[cursor + 5..cursor + 5 + name.len()].copy_from_slice(name);
        cursor += 5 + name.len();
    }

    // folder_count(4) + hash(4) + per-folder 16-byte block.
    buf[layout.folder_count_off..layout.folder_count_off + 4].copy_from_slice(&1u32.to_le_bytes());
    // Hash[4]: parser ignores; leave zero.
    // Per-folder 16-byte block: pattern from shipping single-folder PAMTs is
    // `ff ff ff ff 00 00 00 00 N 00 00 00 N 00 00 00` where N = file count.
    let pf_off = layout.folder_count_off + 4 + 4;
    buf[pf_off..pf_off + 4].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
    buf[pf_off + 4..pf_off + 8].copy_from_slice(&0u32.to_le_bytes());
    buf[pf_off + 8..pf_off + 12].copy_from_slice(&(files.len() as u32).to_le_bytes());
    buf[pf_off + 12..pf_off + 16].copy_from_slice(&(files.len() as u32).to_le_bytes());

    // File records.
    for (i, f) in files.iter().enumerate() {
        let rec_off = layout.file_records_off + i * 20;
        let node_ref = node_offsets[i] as u32;
        buf[rec_off..rec_off + 4].copy_from_slice(&node_ref.to_le_bytes());
        buf[rec_off + 4..rec_off + 8].copy_from_slice(&(f.paz_offset as u32).to_le_bytes());
        buf[rec_off + 8..rec_off + 12].copy_from_slice(&f.comp_size.to_le_bytes());
        buf[rec_off + 12..rec_off + 16].copy_from_slice(&f.orig_size.to_le_bytes());
        // flags: paz_index=0 in low byte; compression_type in (>>16)&0x0F.
        let flags: u32 = ((f.compression_type as u32) & 0x0F) << 16;
        buf[rec_off + 16..rec_off + 20].copy_from_slice(&flags.to_le_bytes());
    }

    // Self-CRC last (PaChecksum over data[12..]).
    let crc = pa_checksum(&buf[12..]);
    buf[..4].copy_from_slice(&crc.to_le_bytes());

    buf
}

// ── helpers ───────────────────────────────────────────────────────────────

/// `fs::write` via tmpfile + rename so partial writes never appear on disk.
fn atomic_write(path: &Path, data: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(data)?;
        f.flush()?;
    }
    fs::rename(&tmp, path)?;
    Ok(())
}

fn current_pamt_self_crc(pamt_path: &Path) -> Result<u32> {
    let raw = fs::read(pamt_path)?;
    if raw.len() < 4 {
        return Err(ParseError::Other("PAMT too short to read self-CRC".into()));
    }
    Ok(u32::from_le_bytes(raw[..4].try_into().unwrap()))
}

/// Detect the real entry count in a PAPGT by scanning for the trailer marker.
///
/// The PAPGT layout is header(12) + N entries(12) + trailer(`N*5` byte + 3
/// padding + N null-terminated group-name strings). The bare parser reads
/// every 12-byte block to EOF and includes the trailer as misparsed entries.
/// We recover the real `N` by walking the entry-shaped region until we hit
/// a byte that looks like the `N*5` marker followed by ASCII path names.
///
/// Falls back to `(file_size - 12) / 12` if no marker is found, which keeps
/// the function safe on hand-built test PAPGTs that don't have a trailer.
fn real_papgt_entry_count(raw: &[u8]) -> usize {
    if raw.len() < 12 {
        return 0;
    }
    let max = (raw.len() - 12) / 12;
    for n in (1..=max).rev() {
        let trailer_off = 12 + n * 12;
        if trailer_off + 5 > raw.len() {
            continue;
        }
        // Marker = N*5 modulo 256; trailer starts at marker byte then 3
        // bytes of padding then ASCII '0'-prefixed group names.
        if raw[trailer_off] == ((n * 5) & 0xFF) as u8
            && raw[trailer_off + 4] == b'0'
        {
            return n;
        }
    }
    max
}

/// Position of `group_id` in the sorted list of dirs that contain `0.pamt`.
fn sorted_group_index(packages_path: &Path, group_id: &str) -> Result<usize> {
    let mut dirs: Vec<String> = fs::read_dir(packages_path)?
        .filter_map(|e| e.ok())
        .filter(|e| {
            let p = e.path();
            p.is_dir() && p.join("0.pamt").exists()
        })
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    dirs.sort();
    dirs.iter()
        .position(|d| d == group_id)
        .ok_or_else(|| ParseError::Other(format!("group {group_id} not found in {packages_path:?}")))
}

/// Whether a virtual path should be LZ4-compressed when stored. Mirrors the
/// shipped convention: most data gets compressed; tiny/uncompressible
/// formats (audio, dds) typically aren't. For user files, default on.
fn should_compress(_path: &str) -> bool {
    true
}

fn _unused_pamt_table(_t: &PazTableEntry) {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static TMPDIR_LOCK: Mutex<()> = Mutex::new(());

    /// Round-trip serialize_user_pamt -> parse_pamt_bytes.
    #[test]
    fn user_pamt_roundtrips_through_parser() {
        let files = vec![
            UserFile {
                path: "modded/foo.bin".into(),
                paz_offset: 0,
                comp_size: 100,
                orig_size: 200,
                compression_type: COMP_LZ4,
            },
            UserFile {
                path: "modded/bar.txt".into(),
                paz_offset: 128,
                comp_size: 50,
                orig_size: 50,
                compression_type: 0,
            },
        ];
        let raw = serialize_user_pamt(&files, 0xDEAD_BEEF, 1024);
        let parsed = parse_pamt_bytes(&raw, "x", "/tmp", 0).expect("parse_pamt");
        assert_eq!(parsed.paz_count, 1);
        assert_eq!(parsed.paz_table.len(), 1);
        assert_eq!(parsed.paz_table[0].checksum, 0xDEAD_BEEF);
        assert_eq!(parsed.paz_table[0].size, 1024);
        assert_eq!(parsed.file_entries.len(), 2);
        assert_eq!(parsed.file_entries[0].path, "modded/foo.bin");
        assert_eq!(parsed.file_entries[0].comp_size, 100);
        assert_eq!(parsed.file_entries[0].orig_size, 200);
        assert_eq!(parsed.file_entries[0].compression_type(), COMP_LZ4);
        assert_eq!(parsed.file_entries[1].path, "modded/bar.txt");
        assert_eq!(parsed.file_entries[1].compression_type(), 0);

        // Self-CRC must verify.
        let stored = u32::from_le_bytes(raw[..4].try_into().unwrap());
        let computed = pa_checksum(&raw[12..]);
        assert_eq!(stored, computed);
    }

    #[test]
    fn user_pamt_empty_serializes_cleanly() {
        let raw = serialize_user_pamt(&[], 0, 0);
        let parsed = parse_pamt_bytes(&raw, "x", "/tmp", 0).expect("parse_pamt");
        assert_eq!(parsed.file_entries.len(), 0);
        let stored = u32::from_le_bytes(raw[..4].try_into().unwrap());
        let computed = pa_checksum(&raw[12..]);
        assert_eq!(stored, computed);
    }

    #[test]
    fn user_pamt_rejects_oversized_path() {
        // The PAMT format encodes name_len as u8, so paths > 255 bytes can't
        // round-trip. UserGroup::add enforces this; here we exercise the
        // validation directly.
        let _g = TMPDIR_LOCK.lock().unwrap();
        let tmp = std::env::temp_dir().join(format!("cdfuse_user_pamt_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        let papgt = tmp.join("dummy.papgt");
        // Touch a minimal PAPGT-like file: header + 0 entries + 1 trailing pad
        // to mirror the production layout's parity. The bootstrap path won't
        // touch this file beyond reading-and-rewriting.
        let mut hdr = vec![0u8; 12];
        hdr.extend_from_slice(&[0u8; 12]); // one slot to receive our group
        fs::write(&papgt, &hdr).unwrap();
        // Stub a 0000 dir so `sorted_group_index` resolves position 0 -> 9000 at 1.
        fs::create_dir_all(tmp.join("9000")).unwrap();
        let mut ug = UserGroup::open_or_create(&tmp, &papgt, "9000").unwrap();
        let long = "a".repeat(256);
        let err = ug.add(&long, b"x").unwrap_err();
        assert!(err.to_string().contains("too long"), "got: {err}");
    }
}
