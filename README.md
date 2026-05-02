## Crates

### `cdcore`

Rust library exposed to Python via [PyO3](https://pyo3.rs). Used as the VFS and
decoder backend for [CrimsonForge](https://github.com/hzeemr/crimsonforge). Add
one line at the top of `main.py` to activate:

```python
import cdcore  # monkeypatches core.vfs_manager and core.dds_reader
```


**Build and install:**
```bash
cd cdcore
./build.sh
```

---

### `cdfuse`
(Linux only)
FUSE filesystem that mounts Crimson Desert archives as a Linux directory tree.
Files are transparently decrypted and decompressed on access.
Supports read-write: drag-and-drop files in, edit files in place, changes
are repacked into the PAZ archives on unmount.

**Requirements:** `libfuse3`, `user_allow_other` in `/etc/fuse.conf`.

**Build:**
```bash
cd cdfuse
cargo build --release
```

**Mount (interactive TUI):**
```bash
cdfuse /path/to/crimson_desert_install_dir /mnt/cd
```

Starts a TUI showing pending writes.
- `[s]` -- repack pending writes to PAZ, keep mounted
- `[c]` -- repack and exit
- `[q]` -- exit without saving

**Mount (non-interactive / scripted):**
```bash
cdfuse /path/to/crimson_desert_install_dir /mnt/cd 2>>cdfuse.log &

# Repack and unmount when done
cdfuse --unmount /mnt/cd

# Mount read-only
cdfuse /path/to/crimson_desert_install_dir /mnt/cd --readonly
```


**Archive tree:**
```
/mnt/cd/
  character/
    cd_phm_basic_00_00_roofclimb_base_std_lantern_b_7_ing_00.paa
    cd_r0002_00_horse_hair_mane_00_0002_index05.prefab
  gamedata/
    localizationstring_eng.paloc
    actionpointinfo.pabgb
    actionpointinfo.pabgh
  object/
    cd_gimmick_statue_09_ball.pam
  ui/
    bitmap_bell.dds
  ...
```

**Virtual read-only views (non-binary formats):**

Hidden root directories expose binary files as human-readable text without
modifying the archives. Each mirrors the full tree and only contains
relevant files.

```
/mnt/cd/.paloc.jsonl/gamedata/localizationstring_eng.paloc.jsonl
/mnt/cd/.pabgb.jsonl/gamedata/actionpointinfo.pabgb.jsonl
/mnt/cd/.prefab.jsonl/character/cd_r0002_00_horse_hair_mane_00_0002_index05.prefab.jsonl
/mnt/cd/.nav.jsonl/leveldata/...nav.jsonl
/mnt/cd/.paa_metabin.jsonl/character/...paa_metabin.jsonl
/mnt/cd/.dds.png/ui/bitmap_bell.dds.png
```

`.paloc.jsonl/` and `.dds.png/` support write-back: saving a file converts
it back to the original binary format and queues it for repack.

```bash
# Edit German localisation
$EDITOR /mnt/cd/.paloc.jsonl/gamedata/localizationstring_ger.paloc.jsonl

# Edit a texture (opens as PNG, saves back as BC7/DDS on unmount)
krita /mnt/cd/.dds.png/ui/bitmap_bell.dds.png
```

**Write via file manager:**

Drag a file onto the mount to replace it. The new content is buffered
in memory and written to the PAZ archive when you commit (`[s]` or `[c]`).

---

### `ddsthumb`

Batch DDS-to-PNG thumbnail generator. Takes a `.dds` file or directory
(scanned recursively) and writes resized PNGs to an output directory,
preserving the relative path structure. Handles all formats supported
by `cdcore`: BC1-BC7, BC6H, RGBA, float variants.

```bash
# Single file
ddsthumb ui/bitmap_bell.dds /tmp/thumbs --size 128

# Entire tree
ddsthumb /mnt/cd/ui /tmp/thumbs --size 256
# Found 18355 DDS files -- generating 256px thumbnails ...
#   1000/18355  errors=0
#   ...
```


---

## Build requirements

- Rust 1.70+
- Python 3.10+ with `libpython3.x-dev` (`apt install libpython3-dev`) -- pyo3 links against libpython at compile time
- [maturin](https://github.com/PyO3/maturin) 1.0+ (`pip install maturin`) -- builds the cdcore wheel
- `libfuse3-dev` (`apt install libfuse3-dev`) -- cdfuse links against libfuse3 at compile time

## Runtime requirements

- `cdcore` wheel: Python 3.10+, no other native dependencies
- `cdfuse`: `libfuse3` (`apt install libfuse3`), `user_allow_other` in `/etc/fuse.conf`
- `ddsthumb`: none (statically linked)
