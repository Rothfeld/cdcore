use std::io::IsTerminal;
use std::sync::Arc;

use clap::Parser;
use log::info;
use cdcore::VfsManager;

mod fs;
mod tui;
mod virtual_files;

#[derive(Parser)]
#[command(name = "cdwinfs", about = "Mount Crimson Desert archives as a Windows filesystem via WinFSP")]
struct Args {
    /// Path to the Crimson Desert install directory (contains 0000/, meta/, ...)
    #[arg(value_name = "GAME_DIR")]
    game_dir: String,

    /// Mount point — drive letter (X:) or empty directory path
    #[arg(value_name = "MOUNT")]
    mount: String,

    /// Mount read-only (no writes to PAZ archives)
    #[arg(long)]
    readonly: bool,

    /// Disable automatic repack-to-PAZ when a modified file is closed
    #[arg(long)]
    no_auto_repack: bool,

    /// Load all package groups at mount time (default: lazy per-group load)
    #[arg(long)]
    preload: bool,

    /// Comma-separated list of specific groups to load (e.g. 0000,0001)
    #[arg(long, value_delimiter = ',')]
    groups: Vec<String>,
}

fn main() {
    let args = Args::parse();

    // Log to file so the TUI can own the console.
    let f = std::fs::File::create("cdwinfs.log")
        .expect("cannot open cdwinfs.log");
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .target(env_logger::Target::Pipe(Box::new(f)))
        .init();

    // winfsp_init_or_die: tries local winfsp-x64.dll first, then falls back to
    // HKLM\SOFTWARE\WinFsp\InstallDir via the `system` feature.
    let _init = winfsp::winfsp_init_or_die();

    let vfs = VfsManager::new(&args.game_dir).unwrap_or_else(|e| {
        eprintln!("error: {e}");
        std::process::exit(1);
    });

    if args.preload {
        info!("preload: loading all groups");
        vfs.load_all_groups().unwrap_or_else(|e| log::warn!("{e}"));
    } else if !args.groups.is_empty() {
        for g in &args.groups {
            info!("loading group {g}");
            vfs.load_group(g).unwrap_or_else(|e| log::warn!("{e}"));
        }
    } else {
        let groups = vfs.list_groups().unwrap_or_else(|e| {
            log::warn!("list_groups failed: {e}");
            Vec::new()
        });
        info!("loading {} groups", groups.len());
        for g in &groups {
            vfs.load_group(g).unwrap_or_else(|e| log::warn!("{e}"));
        }
    }

    let cdfs   = fs::CdWinFs::new(vfs, args.readonly, !args.no_auto_repack);
    let shared = cdfs.shared();

    let mut volume_params = winfsp::host::VolumeParams::new();
    volume_params
        .sector_size(512)
        .sectors_per_allocation_unit(1)
        .max_component_length(255)
        .volume_creation_time(0)
        .volume_serial_number(0x00CDF00D)
        .file_info_timeout(1000)
        .irp_timeout(60_000)
        .case_sensitive_search(true)
        .case_preserved_names(true)
        .unicode_on_disk(true)
        .persistent_acls(false)
        .read_only_volume(args.readonly)
        .filesystem_name("cdwinfs");

    let mut host = winfsp::host::FileSystemHost::new(volume_params, cdfs)
        .unwrap_or_else(|e| { eprintln!("create host: {e}"); std::process::exit(1); });

    host.mount(&args.mount)
        .unwrap_or_else(|e| { eprintln!("mount {}: {e}", args.mount); std::process::exit(1); });

    host.start()
        .unwrap_or_else(|e| { eprintln!("start dispatcher: {e}"); std::process::exit(1); });

    info!("mounted {} at {} ({})", args.game_dir, args.mount,
          if args.readonly { "ro" } else { "rw" });

    if std::io::stdin().is_terminal() {
        match tui::run(&args.mount, Arc::clone(&shared)) {
            tui::Action::Commit => {
                drop(shared);
                eprintln!("Repacking...");
            }
            tui::Action::Abort => {
                shared.discard_pending();
                drop(shared);
            }
        }
    } else {
        // Non-interactive: WinFSP dispatcher runs on its own threads.
        // Block here until the process is killed (Ctrl+C, service stop, etc.).
        // Cleanup (unmount/stop) runs in FileSystemHost's Drop impl.
        loop { std::thread::sleep(std::time::Duration::from_secs(60)); }
    }

    // Drop host: stop() + unmount() called by Drop impl.
    drop(host);
}
