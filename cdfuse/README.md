## cdfuse

Linux counterpart of `cdwinfs`. Both implement the same filesystem — transparent
read/write access to Crimson Desert PAZ archives via a mounted directory tree,
with virtual JSONL/PNG views over binary formats and TUI-driven write-back.

**Platform split:**

| Crate | Platform | Driver |
|-------|----------|--------|
| `cdfuse` | Linux | FUSE (`fuser`) |
| `cdwinfs` | Windows | WinFsp |

**Keeping them in sync:**

The two crates are maintained in parallel by AI agents. When adding features or
fixing bugs, both crates should be updated together. The core logic
(`virtual_files.rs`, `tui.rs`, the decode/repack pipeline in `SharedFs`) is
identical; only the filesystem callback layer differs.

See `../README.md` for usage, build instructions, and runtime requirements.
