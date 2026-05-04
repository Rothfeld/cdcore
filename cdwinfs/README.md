## cdwinfs

Windows WinFSP filesystem mount for Crimson Desert archives. See `../README.md` for full
usage, build instructions, and runtime requirements.

Depends on `cdcore`. Licensed GPL-3.0 (inherited from `winfsp-rs` bindings).
Linux counterpart: `cdfuse` (MIT).

**Keeping in sync with cdfuse:**

Core logic (`virtual_files.rs`, `tui.rs`, `SharedFs` decode/repack pipeline) is identical
between the two crates -- only the filesystem callback layer differs. Both must be updated
together when adding features or fixing bugs.
