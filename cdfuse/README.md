# cdfuse

Mounts Crimson Desert archives as a Linux filesystem (default `/mnt/cd`).
Browse them in your file manager, open them in your usual editor, save, and the change is repacked into the game.

![cdfuse demo](../.assets/demo.gif)

## Quick start

1. Install **fuse3**: `sudo apt install fuse3` (or your distro's equivalent).
2. Enable user mounts: ensure `user_allow_other` is uncommented in `/etc/fuse.conf`.
3. Make the binary executable and run it: `chmod +x cdfuse && ./cdfuse`.

A small terminal setup screen opens:

```
  Game directory:  /mnt/games/CrimsonDesert/packages   (detected)
  Mount point:     /mnt/cd                             [m] edit
```

The Crimson Desert install path is detected from common Steam paths where
possible. Press **Enter** once both fields are filled. Settings save to
`~/.config/cdfuse/cdfuse.cfg` so subsequent runs need no arguments.

## Using the mount

Browse `/mnt/cd` in your file manager or shell:

```
/mnt/cd/
  character/
    cd_phm_basic_00_00_roofclimb_base_std_lantern_b_7_ing_00.paa
  gamedata/
    localizationstring_eng.paloc
    actionpointinfo.pabgb
  object/
    cd_gimmick_statue_09_ball.pam
  ui/
    bitmap_bell.dds
```

You can copy files out, run them through tools, and so on -- all read
operations work like a normal filesystem.

### Editing in familiar formats

Some game formats (`.paloc`, `.dds`, `.pam`, `.pamlod`, `.pac`, `.wem`) are
binary and not directly editable. Hidden root folders expose them in
editable formats:

| Hidden folder | Format | Editor |
|---------------|--------|--------|
| `/mnt/cd/.paloc.jsonl/`  | JSON-lines text     | vim, VS Code, gedit |
| `/mnt/cd/.dds.png/`      | PNG image           | GIMP, Krita |
| `/mnt/cd/.pam.fbx/`      | static mesh, FBX    | Blender |
| `/mnt/cd/.pamlod.fbx/`   | LOD mesh, FBX       | Blender |
| `/mnt/cd/.pac.fbx/`      | skinned mesh, FBX   | Blender |
| `/mnt/cd/.wem.ogg/`      | OGG audio           | mpv, VLC |

So instead of `gamedata/localizationstring_eng.paloc` (binary), you open
`/mnt/cd/.paloc.jsonl/gamedata/localizationstring_eng.paloc.jsonl` (text).

(if they dont show, your file manager probably hides dotfiles -- toggle "show hidden", or use `ls -a`)

Most virtual views are read-only for now.
**Saving repacks back to the game.** When you save through `.paloc.jsonl/`
or `.dds.png/`, the file is converted back to binary and written into the
original archive.

Voice-over languages live in separate package groups, so `sound/` shows up
multiple times:

- `/mnt/cd/sound@0005/` -- Korean
- `/mnt/cd/sound@0006/` -- English
- `/mnt/cd/sound@0035/` -- Japanese

### While mounted

A small status display stays open showing mount state and any repack
events. Press **Esc** to unmount.

---
<details>
<summary><b>Advanced</b></summary>


### Verifying the download

Every release publishes a SLSA build provenance attestation. With the
GitHub CLI installed:

```bash
gh attestation verify cdfuse --owner Rothfeld
```

This proves the binary was built by GitHub Actions from a specific commit
on the source repo, not tampered with after upload.

### Command-line options

```
cdfuse <GAME_DIR> <MOUNT>           Skip the setup screen and mount directly
cdfuse --unmount <MOUNT>            Unmount a previously mounted cdfuse
cdfuse --readonly                   Mount read-only (no repacks)
cdfuse --preload                    Load every package group up front
cdfuse --groups 0000,0005           Load only the listed groups
cdfuse --no-auto-repack             Don't repack a file when it's closed
cdfuse --licenses                   Print third-party licenses and exit
```

Example:

```bash
cdfuse "/mnt/games/Crimson Desert" /mnt/cd
```

Logs land in `/tmp/cdfuse.log`.

</details>

---

## License

MIT. See the `LICENSE` file at the repository root.

## Source and issues

<https://github.com/Rothfeld/cdcore>
