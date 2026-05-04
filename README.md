## Crates

These crates are unaffiliated companion tooling for the excellent
[CrimsonForge](https://github.com/hzeemr/crimsonforge) modding studio.
The archive formats, crypto, compression, mesh parsers, FBX export logic,
and virtual file designs are all derived directly from CrimsonForge's Python
implementation.

---

### `cdcore`

Rust library exposed to Python via [PyO3](https://pyo3.rs).
Used as a faster VFS and decoder backend for CrimsonForge. Add one line at
the top of `main.py` to activate:

```python
import cdcore  # monkeypatches vfs, dds reader and mesh reader with native implementations
```

**Build and install:**
```bash
cd cdcore
./build.sh
```

---

### `cdfuse` (Linux) / `cdwinfs` (Windows)

![cdfuse demo](.assets/demo.gif "when i was younger, modding counter strike was as simple as dragging a file into a directory. why should it be harder?")

<details>
<summary>Architecture diagram</summary>

![architecture](.assets/diagram.svg "boy, now that i made a diagram it sure looks a lot more professional than it felt building it")

</details>

Filesystem that mounts Crimson Desert archives as a browsable directory tree.
Files are transparently decrypted and decompressed on access.
Supports read-write: edit files in place or drag-and-drop replacements.
By default, closing a modified file immediately repacks it into the PAZ archives
in the background. Pass `--no-auto-repack` to disable this and use `[s]` to
flush manually instead.

| Crate | Platform | Driver | Requirement |
|-------|----------|--------|-------------|
| `cdfuse` | Linux | FUSE via `fuser` | `libfuse3`, `user_allow_other` in `/etc/fuse.conf` |
| `cdwinfs` | Windows | [WinFsp](https://winfsp.dev/rel/) | WinFsp 2.x installed |

**Build:**
```bash
cd cdfuse       # Linux
cargo build --release

cd cdwinfs      # Windows
cargo build --release
```

**First launch — interactive setup:**

Both tools save their configuration (`~/.config/cdfuse/cdfuse.cfg` on Linux,
`%APPDATA%\CrimsonForge\cdwinfs.cfg` on Windows) so they can be launched without
arguments on subsequent runs.

On first launch (no saved config, no CLI args) a configuration screen appears:

```
  Game directory:  /cd              (detected)    [g] browse
  Mount point:     /media/max/cd                  [m] edit
```

The game directory is detected automatically from the registry (Windows) or common
install paths. Missing fields are shown in red. Press `Enter` to mount once both
are filled.

**CLI usage:**
```bash
cdfuse [GAME_DIR] [MOUNT]          # Linux — args override saved config
cdwinfs.exe [GAME_DIR] [DRIVE]     # Windows — DRIVE is a single letter, e.g. Y
```

**TUI while mounted:**
```
  [s] flush pending writes to PAZ    Esc quit [without saving]
```
- `(ro)` shown in yellow if mounted read-only
- Events panel appears below when repacks complete or fail

**Archive tree:**
```
/media/max/cd/     (Linux)    Y:\    (Windows)
  character/
    cd_phm_basic_00_00_roofclimb_base_std_lantern_b_7_ing_00.paa
    cd_r0002_00_horse_hair_mane_00_0002_index05.prefab
  gamedata/
    localizationstring_eng.paloc
    actionpointinfo.pabgb
  object/
    cd_gimmick_statue_09_ball.pam
  ui/
    bitmap_bell.dds
```

**Virtual read-only views:**

Hidden root directories expose binary files in more usable formats without
modifying the archives:

```
.paloc.jsonl/gamedata/localizationstring_eng.paloc.jsonl   (localisation text)
.dds.png/ui/bitmap_bell.dds.png                            (textures as PNG)
.pam.fbx/object/cd_gimmick_statue_09_ball.pam.fbx          (static mesh as FBX)
.pamlod.fbx/character/cd_phm_basic_body.pamlod.fbx         (LOD mesh as FBX)
.pac.fbx/character/cd_phm_basic_body.pac.fbx               (skinned mesh as FBX)
.wem.ogg/audio/vo_en_main_001.wem.ogg                      (audio as OGG)
```

`.paloc.jsonl/` and `.dds.png/` support write-back: saving a file converts it
back to the original binary format and repacks it automatically.

The FBX exporter is a Rust port of CrimsonForge's `mesh_exporter.py`, producing
binary FBX 7.4 files compatible with Blender, Maya, and Unreal. Geometry only
for now; skeleton support is planned.

`.wem.ogg/` requires [vgmstream-cli](https://github.com/vgmstream/vgmstream) to
be installed (ISC licence). The TUI header shows `vgm` and `ffmpeg` in green
when the tools are found, red when absent. The audio root is hidden entirely if
vgmstream is not installed.

```bash
# Edit German localisation
$EDITOR /media/max/cd/.paloc.jsonl/gamedata/localizationstring_ger.paloc.jsonl

# Edit a texture (save as PNG; repacked to original DDS format automatically)
krita /media/max/cd/.dds.png/ui/bitmap_bell.dds.png

# Open a mesh in Blender
blender /media/max/cd/.pam.fbx/object/cd_gimmick_statue_09_ball.pam.fbx

# Play a voice line
mpv /media/max/cd/.wem.ogg/audio/vo_en_main_001.wem.ogg
```

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
ddsthumb /media/max/cd/ui /tmp/thumbs --size 256
# Found 18355 DDS files -- generating 256px thumbnails ...
#   1000/18355  errors=0
```

---

## Release artifacts

Each tagged release on GitHub attaches:

| File | Description |
|------|-------------|
| `cdcore-X.Y.Z-cp312-cp312-manylinux_2_17_x86_64.manylinux2014_x86_64.whl` | cdcore wheel — Linux x86-64 |
| `cdcore-X.Y.Z-cp312-cp312-win_amd64.whl` | cdcore wheel — Windows x86-64 |
| `cdfuse` | cdfuse binary — Linux |
| `cdwinfs.exe` | cdwinfs binary — Windows |

The Linux wheel carries both the `manylinux_2_17_x86_64` (PEP 600) and
`manylinux2014_x86_64` (legacy) platform tags in its filename.  Both refer to
the same file; pip selects it automatically on any supported Linux distribution.

Install the cdcore wheel:
```bash
pip install cdcore-*.whl
```

## Build requirements

- Rust 1.70+
- Python 3.10+ with `libpython3.x-dev` (`apt install libpython3-dev`) — pyo3 links against libpython at compile time
- [maturin](https://github.com/PyO3/maturin) 1.0+ (`pip install maturin`) — builds the cdcore wheel
- **Linux:** `libfuse3-dev` (`apt install libfuse3-dev`) — cdfuse links against libfuse3 at compile time
- **Windows:** LLVM/clang for `winfsp-sys` bindgen — pre-installed on `windows-latest` CI runners; locally install from [llvm.org](https://releases.llvm.org/)

## Runtime requirements

- `cdcore` wheel: Python 3.10+, no other native dependencies
- `cdfuse` (Linux): `libfuse3` (`apt install libfuse3`), `user_allow_other` in `/etc/fuse.conf`
- `cdwinfs` (Windows): [WinFsp 2.x](https://winfsp.dev/rel/) installed — the installer registers the DLL path; `cdwinfs.exe` finds it automatically
- `ddsthumb`: none (statically linked)
- **Audio virtual view** (optional): [vgmstream-cli](https://github.com/vgmstream/vgmstream) for `.wem.ogg/`; [ffmpeg](https://ffmpeg.org) for future write-back. Both are detected at startup from `PATH` or `~/.crimsonforge/tools/` (Linux) / `%APPDATA%\CrimsonForge\tools\` (Windows).
