## Crates

### `crimsonforge_core`

Rust library exposed to Python via [PyO3](https://pyo3.rs).

- **VFS** — unified read access to the 1.4M+ game files across all PAZ archives
- **Parsers** — PAM/PAC meshes, PAA animations, PAB skeletons, HKX physics, PALOC localization, PABGB game data tables, prefabs, navigation meshes
- **Crypto** — ChaCha20 (filename-based key derivation) + Bob Jenkins PaChecksum
- **Compression** — LZ4 block, zlib, Type-1 PAR per-section LZ4
- **Repack** — atomic 13-step pipeline: compress → encrypt → append PAZ → update PAMT → update PAPGT → verify checksum chain

**Build and install:**
```bash
cd crimsonforge_core
./build.sh
```

**Python usage:**
```python
import crimsonforge_core as cf

vfs = cf.VfsManager("/path/to/crimson_desert_install_dir")
vfs.load_all_groups()

entry = vfs.lookup("object/cd_gimmick_statue_09_ball.pam")
data  = vfs.read_entry(entry)          # decrypt + decompress

mesh  = cf.parse_pam(data, "cd_gimmick_statue_09_ball.pam")
print(mesh.total_vertices, mesh.total_faces)

localization = vfs.lookup("gamedata/localizationstring_eng.paloc")
paloc = cf.parse_paloc(vfs.read_entry(localization))
print(paloc.entry_count)           # 178864
print(paloc.lookup("262897"))      # 'Unavailable while mounted.'
```

---

### `cdfuse`

Read-only FUSE filesystem that mounts Crimson Desert archives as a Linux directory tree. Files are transparently decrypted and decompressed on access.

**Requirements:** `libfuse3`, `user_allow_other` in `/etc/fuse.conf`.

**Build:**
```bash
cd cdfuse
cargo build --release
```

**Mount:**
```bash
# Mount all groups
./target/release/cdfuse /path/to/crimson_desert_install_dir /mnt/cd

# Mount specific groups only (0000 contains the object/ tree)
./target/release/cdfuse /path/to/crimson_desert_install_dir /mnt/cd --groups 0000,0001

# Unmount
fusermount3 -u /mnt/cd
```

Once mounted the full archive tree is browsable:
```
/mnt/cd/
  character/
    cd_phm_basic_00_00_roofclimb_base_std_lantern_b_7_ing_00.paa
    cd_r0002_00_horse_hair_mane_00_0002_index05.prefab
    …
  object/
    cd_gimmick_statue_09_ball.pam
    03_cube.hkx
    …
  sound/
  texture/
  …
```

## Requirements

- Rust 1.70+
- Python 3.10+ with `libpython3.x-dev` (`apt install libpython3-dev`)
- [maturin](https://github.com/PyO3/maturin) 1.0+ (`pip install maturin`)
- libfuse3 (for `cdfuse`)
