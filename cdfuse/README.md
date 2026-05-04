## cdfuse

Linux FUSE filesystem mount for Crimson Desert archives. See `../README.md` for full
usage, build instructions, and runtime requirements.

Depends on `cdcore`. Windows counterpart: `cdwinfs` (WinFSP, GPL-3.0).

**Keeping in sync with cdwinfs:**

Core logic (`virtual_files.rs`, `tui.rs`, `SharedFs` decode/repack pipeline) is identical
between the two crates -- only the filesystem callback layer differs. Both must be updated
together when adding features or fixing bugs.
