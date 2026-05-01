## Crates

### `crimsonforge_core`

Rust library exposed to Python via [PyO3](https://pyo3.rs).

- **VFS** — unified read access to the 1.4M+ game files across all PAZ archives
- **Parsers** — PAM/PAC/PAMLOD meshes, PAA animations, PAB skeletons, PABC morph targets, PABC skin palettes, HKX physics, NAV navigation meshes, PALOC localization, PABGB game data tables, prefabs
- **DDS decode** — BC1–BC7, BGRA32, Luminance8/16, DX10 extended header
- **Crypto** — ChaCha20 (filename-based key derivation) + Bob Jenkins PaChecksum
- **Compression** — LZ4 block, zlib, Type-1 PAR per-section LZ4
- **Repack** — atomic 13-step pipeline: compress -> encrypt -> append PAZ -> update PAMT -> update PAPGT -> verify checksum chain

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

paloc = cf.parse_paloc(vfs.read_entry(vfs.lookup("gamedata/localizationstring_eng.paloc")))
print(paloc.lookup("262897"))      # 'Unavailable while mounted.'

# DDS decode
w, h, rgba = cf.decode_dds_to_rgba(dds_bytes)

# PABC morph targets
morph = cf.parse_pabc(pabc_bytes)
print(morph.count, morph.row_floats_hint)  # e.g. 178, 49

# PABC skin palette (per-mesh bone slot -> PAB bone index)
palette = cf.parse_skin_pabc(pabc_bytes, pab_hashes, "mesh.pabc")
bone_idx = palette.slot_to_pab(17)
```

**Transparent Python integration:**

When the wheel is installed and imported before `core.vfs_manager` or `core.dds_reader`,
it silently injects Rust-backed implementations into `sys.modules`. Existing Python code
requires no changes:

```python
import crimsonforge_core  # inject first

from core.vfs_manager import VfsManager  # -> _RustVfsManager
from core.dds_reader import decode_dds_to_rgba  # -> Rust decoder
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

# Mount specific groups only
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
    ...
  object/
    cd_gimmick_statue_09_ball.pam
    03_cube.hkx
    ...
  sound/
  texture/
  ...
```

## Requirements

- Rust 1.70+
- Python 3.10+ with `libpython3.x-dev` (`apt install libpython3-dev`)
- [maturin](https://github.com/PyO3/maturin) 1.0+ (`pip install maturin`)
- libfuse3 (for `cdfuse`)
