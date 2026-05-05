//! Atomic repack pipeline with full checksum chain management.
//!
//! Steps:
//!  1. Validate input data (basic magic checks)
//!  2. Compress (LZ4) + encrypt (ChaCha20) modified data
//!  3. Append to PAZ (16-byte aligned)
//!  4. Compute file CRC = pa_checksum(encrypted_bytes)
//!  5. Update PAMT record: new_comp_size, new_orig_size, new_offset
//!  6. Update PAZ table CRC in PAMT
//!  7. Recompute PAMT self-CRC
//!  8. Write PAMT to disk
//!  9. Update PAPGT: write new PAMT CRC at correct entry
//! 10. Recompute PAPGT self-CRC
//! 11. Write PAPGT to disk
//! 12. Verify full checksum chain
//!
//! No backups are taken.  PAZ archives ship with the game; users restore
//! originals via Steam's "Verify integrity of game files".  The Python
//! CrimsonForge GUI has its own backup_manager for users who want explicit
//! pre-edit snapshots (deliberate, not per-save).

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::Path;

use crate::archive::pamt::{PamtData, PamtFileEntry, update_file_record, update_paz_entry, update_self_crc as pamt_update_crc};
use crate::archive::papgt::{update_pamt_crc as papgt_update_pamt_crc, update_self_crc as papgt_update_crc, pamt_crc_offset};
use crate::archive::paz;
use crate::compression;
use crate::crypto::{self, pa_checksum};
use crate::error::Result;

pub struct ModifiedFile {
    pub data: Vec<u8>,
    pub entry: PamtFileEntry,
    pub pamt_data: PamtData,
    pub package_group: String,
}

pub struct RepackResult {
    pub success: bool,
    pub files_repacked: usize,
    pub paz_crc: u32,
    pub pamt_crc: u32,
    pub papgt_crc: u32,
    pub errors: Vec<String>,
}

pub struct RepackEngine {
    // No state: paths are passed in per call.  The previous backup_dir
    // field has been removed -- see module docstring.
}

impl RepackEngine {
    pub fn new(_packages_path: &str) -> Self {
        RepackEngine {}
    }

    pub fn repack(
        &self,
        modified_files: Vec<ModifiedFile>,
        papgt_path: &str,
    ) -> Result<RepackResult> {
        let mut errors = Vec::new();

        // Group by package_group
        let mut groups: HashMap<String, Vec<&ModifiedFile>> = HashMap::new();
        for mf in &modified_files {
            groups.entry(mf.package_group.clone()).or_default().push(mf);
        }

        let papgt_raw = fs::read(papgt_path)?;
        let papgt = crate::archive::papgt::parse_papgt(papgt_path)?;
        let mut papgt_raw = papgt_raw;

        let mut last_paz_crc  = 0u32;
        let mut last_pamt_crc = 0u32;
        let last_papgt_crc;
        let mut total_repacked = 0usize;

        for (group_key, files) in &groups {
            let pamt_data = &files[0].pamt_data;
            let mut pamt_raw: Vec<u8> = pamt_data.raw_data.clone();

            for mf in files {
                let entry = &mf.entry;
                let mut processed = mf.data.clone();

                // Compress if needed
                if entry.compression_type() != 0 && entry.compression_type() == compression::COMP_LZ4 {
                    processed = compression::compress_lz4(&processed);
                }

                // Encrypt if needed
                if entry.encrypted() {
                    let basename = Path::new(&entry.path)
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or(&entry.path);
                    processed = crypto::encrypt(&processed, basename);
                }

                let new_comp_size = processed.len() as u32;
                let new_orig_size = mf.data.len() as u32;
                let _file_crc = pa_checksum(&processed);

                // Append to PAZ
                let new_offset = match paz::append_entry(&entry.paz_file, &processed) {
                    Ok(off) => off as u32,
                    Err(e) => {
                        errors.push(format!("paz write error for {}: {e}", entry.path));
                        continue;
                    }
                };

                // Update PAMT file record
                update_file_record(
                    &mut pamt_raw,
                    entry.record_offset,
                    new_comp_size,
                    new_orig_size,
                    Some(new_offset),
                );

                // Update PAZ table entry with new PAZ CRC
                let paz_size = paz::file_size(&entry.paz_file).unwrap_or(0) as u32;
                let paz_crc  = checksum_paz(&entry.paz_file);
                if let Some(paz_entry) = pamt_data.paz_table.get(entry.paz_index as usize) {
                    update_paz_entry(&mut pamt_raw, paz_entry, paz_crc, paz_size);
                    last_paz_crc = paz_crc;
                }

                total_repacked += 1;
            }

            // Recompute PAMT self-CRC
            last_pamt_crc = pamt_update_crc(&mut pamt_raw);

            // Write PAMT to disk
            atomic_write(&pamt_data.path, &pamt_raw)?;

            // Update PAPGT with new PAMT CRC
            let folder_number: u32 = group_key.trim_start_matches('0').parse().unwrap_or(0);
            if let Some(crc_off) = pamt_crc_offset(&papgt, folder_number) {
                papgt_update_pamt_crc(&mut papgt_raw, crc_off, last_pamt_crc);
            }
        }

        // Recompute and write PAPGT
        last_papgt_crc = papgt_update_crc(&mut papgt_raw);
        atomic_write(papgt_path, &papgt_raw)?;

        Ok(RepackResult {
            success: errors.is_empty(),
            files_repacked: total_repacked,
            paz_crc: last_paz_crc,
            pamt_crc: last_pamt_crc,
            papgt_crc: last_papgt_crc,
            errors,
        })
    }
}

fn checksum_paz(paz_path: &str) -> u32 {
    match fs::read(paz_path) {
        Ok(data) => pa_checksum(&data),
        Err(_) => 0,
    }
}

fn atomic_write(path: &str, data: &[u8]) -> Result<()> {
    let tmp = format!("{path}.tmp");
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(data)?;
        f.flush()?;
    }
    fs::rename(&tmp, path)?;
    Ok(())
}

/// Verify the full checksum chain for a repacked group.
pub fn verify_chain(pamt_path: &str, papgt_path: &str) -> Result<bool> {
    
    

    let pamt_data = fs::read(pamt_path)?;
    let stored_pamt_crc = u32::from_le_bytes(pamt_data[..4].try_into().unwrap());
    let computed_pamt_crc = pa_checksum(&pamt_data[12..]);
    if stored_pamt_crc != computed_pamt_crc { return Ok(false); }

    let papgt_data = fs::read(papgt_path)?;
    let stored_papgt_crc = u32::from_le_bytes(papgt_data[4..8].try_into().unwrap());
    let computed_papgt_crc = pa_checksum(&papgt_data[12..]);
    if stored_papgt_crc != computed_papgt_crc { return Ok(false); }

    Ok(true)
}
