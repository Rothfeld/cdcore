//! Integration tests: parse known mesh samples from the live game data and
//! verify vertex counts match the Python reference implementation.
//!
//! Requires the game data to be mounted at /cd.  Tests are skipped when
//! /cd/0000 is absent so CI (without game data) stays green.

use cdcore::vfs::manager::VfsManager;
use cdcore::formats::mesh::{parse_pam, parse_pamlod};
use cdcore::formats::mesh::pac::{find_pac_descriptors, parse_par_sections};

fn vfs() -> Option<VfsManager> {
    if !std::path::Path::new("/cd/0000").exists() {
        return None;
    }
    let vfs = VfsManager::new("/cd").ok()?;
    vfs.load_group("0000").ok()?;
    Some(vfs)
}

/// Like `vfs()` but loads every package group; needed for character PACs
/// which live outside group 0000.
fn vfs_all() -> Option<VfsManager> {
    if !std::path::Path::new("/cd/0000").exists() {
        return None;
    }
    let vfs = VfsManager::new("/cd").ok()?;
    vfs.load_all_groups().ok()?;
    Some(vfs)
}

fn read(vfs: &VfsManager, path: &str) -> Vec<u8> {
    let entry = vfs.lookup(path).unwrap_or_else(|| panic!("entry not found: {path}"));
    vfs.read_entry(&entry).unwrap_or_else(|e| panic!("read failed for {path}: {e}"))
}

// ---- PAM --------------------------------------------------???------------------------------------------------------------------------

#[test]
fn pam_altarmarble() {
    let Some(vfs) = vfs() else { return };
    let data = read(&vfs, "object/cd_ancient_altarmarble_01.pam");
    let mesh = parse_pam(&data, "cd_ancient_altarmarble_01.pam").unwrap();
    assert_eq!(mesh.total_vertices, 6388, "altarmarble vertex count");
}

#[test]
fn pam_statue_breakable_large_mesh() {
    // total_verts=85593 (>65535) -- exercises the algebraic stride path
    let Some(vfs) = vfs() else { return };
    let data = read(&vfs, "object/cd_ancient_puzzle_statue_02_breakable.pam");
    let mesh = parse_pam(&data, "cd_ancient_puzzle_statue_02_breakable.pam").unwrap();
    assert_eq!(mesh.total_vertices, 85593, "statue vertex count");
}

// ---- PAMLOD ----------------------------------------------------------------------------------------------------------------------

#[test]
fn pamlod_sphere_uncompressed() {
    let Some(vfs) = vfs() else { return };
    let data = read(&vfs, "object/03_sphere.pamlod");
    let mesh = parse_pamlod(&data, "03_sphere.pamlod").unwrap();
    assert_eq!(mesh.total_vertices, 149, "sphere LOD0 vertex count");
}

#[test]
fn pamlod_barricade_format_a() {
    // Format A: LOD0 LZ4-compressed, entry at table index 0
    let Some(vfs) = vfs() else { return };
    let data = read(&vfs, "object/cd_barricade_gaurd_02.pamlod");
    let mesh = parse_pamlod(&data, "cd_barricade_gaurd_02.pamlod").unwrap();
    assert_eq!(mesh.total_vertices, 6379, "barricade LOD0 vertex count");
}

#[test]
fn pamlod_egg_inverted_lod_order() {
    // LOD0 has fewer vertices than LOD1; sort-by-nv would pick the wrong group.
    // The Format D table gives the authoritative order; chunk-matching uses it.
    let Some(vfs) = vfs() else { return };
    let data = read(&vfs, "object/cd_gimmick_middle_puzzle_egg_01.pamlod");
    let mesh = parse_pamlod(&data, "cd_gimmick_middle_puzzle_egg_01.pamlod").unwrap();
    assert_eq!(mesh.total_vertices, 732, "egg LOD0 vertex count");
}

#[test]
fn pamlod_ship_sorted_lod0() {
    // Large composite object: DDS entries not in LOD0-first order; requires
    // sort-by-nv and algebraic-stride to pick the correct LOD0 (145 047 verts).
    let Some(vfs) = vfs() else { return };
    let data = read(&vfs, "object/cd_gimmick_ship_orient_01_broken_02.pamlod");
    let mesh = parse_pamlod(&data, "cd_gimmick_ship_orient_01_broken_02.pamlod").unwrap();
    assert_eq!(mesh.total_vertices, 145047, "ship LOD0 vertex count");
}

#[test]
fn pamlod_roof_format_d() {
    // Format D: entries[k]=[lz4_prev, start, decomp], LOD0-2 LZ4-compressed
    let Some(vfs) = vfs() else { return };
    let data = read(&vfs, "object/cd_aka_house_module_b_roof_0002.pamlod");
    let mesh = parse_pamlod(&data, "cd_aka_house_module_b_roof_0002.pamlod").unwrap();
    assert_eq!(mesh.total_vertices, 20104, "roof LOD0 vertex count");
}

#[test]
fn pamlod_north_puzzle_format_b() {
    // Format B: end-offset table layout, LOD0+1 LZ4-compressed
    let Some(vfs) = vfs() else { return };
    let data = read(&vfs, "object/cd_puzzle_anamorphic_north_01.pamlod");
    let mesh = parse_pamlod(&data, "cd_puzzle_anamorphic_north_01.pamlod").unwrap();
    assert_eq!(mesh.total_vertices, 22254, "north puzzle LOD0 vertex count");
}

#[test]
fn pamlod_stairs_format_c() {
    // Format C: zero placeholder at table index 0, LOD0 LZ4-compressed
    let Some(vfs) = vfs() else { return };
    let data = read(&vfs, "object/cd_spot_tower_10_stairs_01.pamlod");
    let mesh = parse_pamlod(&data, "cd_spot_tower_10_stairs_01.pamlod").unwrap();
    assert_eq!(mesh.total_vertices, 9310, "stairs LOD0 vertex count");
}

// ---- PAC descriptor recovery ---------------------------------------------------------------------

#[test]
fn pac_descriptor_recovery_doorstatue() {
    // 4-LOD descriptor pattern (04 00 01 02 03). Single submesh.
    let Some(vfs) = vfs_all() else { return };
    let data = read(&vfs, "character/cd_0081_doorstatue.pac");
    let sections = parse_par_sections(&data);
    let sec0 = sections.iter().find(|s| s.index == 0).expect("section 0");
    assert!(sec0.size >= 5, "section 0 too small");
    let n_lods = data[sec0.offset + 4] as usize;
    assert_eq!(n_lods, 4, "doorstatue has 4 LODs");
    let descriptors = find_pac_descriptors(&data, sec0.offset, sec0.size, n_lods);
    assert_eq!(descriptors.len(), 1);
    assert_eq!(descriptors[0].stored_lod_count, 4);
    assert!(descriptors[0].vertex_counts[0] > 0);
}

#[test]
fn pac_descriptor_recovery_eye_3lod() {
    // 3-LOD descriptor pattern. Eye meshes use the alternate Macduff-style
    // 03 00 01 01 02 layout.
    let Some(vfs) = vfs_all() else { return };
    let data = read(&vfs, "character/cd_m0001_00_ancientpeople_eyeleft_0001.pac");
    let sections = parse_par_sections(&data);
    let sec0 = sections.iter().find(|s| s.index == 0).expect("section 0");
    let n_lods = data[sec0.offset + 4] as usize;
    assert_eq!(n_lods, 3, "eye mesh has 3 LODs");
    let descriptors = find_pac_descriptors(&data, sec0.offset, sec0.size, n_lods);
    assert_eq!(descriptors.len(), 1);
    assert_eq!(descriptors[0].stored_lod_count, 3);
}

#[test]
fn pac_descriptor_recovery_giant_multi_submesh() {
    // 23-submesh ancient giant character. Sanity check on multi-submesh recovery.
    let Some(vfs) = vfs_all() else { return };
    let data = read(&vfs, "character/cd_m0001_00_ancientgiant_nude_0001.pac");
    let sections = parse_par_sections(&data);
    let sec0 = sections.iter().find(|s| s.index == 0).expect("section 0");
    let n_lods = data[sec0.offset + 4] as usize;
    let descriptors = find_pac_descriptors(&data, sec0.offset, sec0.size, n_lods);
    assert_eq!(descriptors.len(), 23);
    // Descriptors are sorted by file offset; they should all sit inside section 0.
    for (i, d) in descriptors.iter().enumerate() {
        assert!(d.descriptor_offset >= sec0.offset, "desc {i} before sec0");
        assert!(d.descriptor_offset < sec0.offset + sec0.size, "desc {i} past sec0");
    }
}
