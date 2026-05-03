use std::io::IsTerminal;
use std::sync::Arc;

use clap::Parser;
use log::info;
use cdcore::VfsManager;

mod config;
mod fs;
mod setup;
mod tui;
mod virtual_files;

#[derive(Parser)]
#[command(name = "cdwinfs", about = "Mount Crimson Desert archives as a Windows filesystem via WinFSP")]
struct Args {
    /// Path to the Crimson Desert install directory (contains 0000/, meta/, ...).
    /// Omit to load from saved config or run the interactive setup wizard.
    #[arg(value_name = "GAME_DIR")]
    game_dir: Option<String>,

    /// Mount point — drive letter (X:) or empty directory path.
    /// Omit to load from saved config or run the interactive setup wizard.
    #[arg(value_name = "MOUNT")]
    mount: Option<String>,

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

    // Resolve game_dir + mount: CLI args → saved config → interactive setup.
    let (game_dir, mount) = match (args.game_dir.as_deref(), args.mount.as_deref()) {
        (Some(gd), Some(m)) => {
            // Args provided — save them for next time, then use them.
            let cfg = config::Config { game_dir: gd.to_string(), mount: m.to_string() };
            if let Err(e) = config::save(&cfg) {
                eprintln!("warning: could not save config: {e}");
            }
            (gd.to_string(), m.to_string())
        }
        _ => {
            // No (or partial) CLI args — try saved config first.
            let saved = config::load();
            let game_dir_hint = saved.as_ref()
                .map(|c| std::path::PathBuf::from(&c.game_dir))
                .or_else(|| setup::detect_game_dir());
            let drive_hint = saved.as_ref()
                .map(|c| c.mount.clone())
                .unwrap_or_else(|| setup::detect_free_drive().unwrap_or_else(|| "Y:".to_string()));
            let drive_detected = saved.is_none();

            match tui::select_paths(game_dir_hint, drive_hint, drive_detected) {
                Some((gd, m)) => {
                    let cfg = config::Config { game_dir: gd.clone(), mount: m.clone() };
                    if let Err(e) = config::save(&cfg) {
                        eprintln!("warning: could not save config: {e}");
                    }
                    (gd, m)
                }
                None => std::process::exit(0),
            }
        }
    };

    // The winfsp crate no longer uses the `system` feature (which required WinFSP
    // installed at *build* time for bindgen to find headers).  Without it,
    // winfsp_init_or_die only tries LoadLibraryW("winfsp-x64.dll"), which fails
    // unless the WinFSP bin dir is in PATH.  Add it from the registry ourselves.
    add_winfsp_bin_to_path();
    let _init = winfsp::winfsp_init_or_die();

    let vfs = VfsManager::new(&game_dir).unwrap_or_else(|e| {
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

    let vgmstream = setup::detect_vgmstream();
    let ffmpeg    = setup::detect_ffmpeg();
    let cdfs   = fs::CdWinFs::new(vfs, args.readonly, !args.no_auto_repack, vgmstream, ffmpeg);
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

    host.mount(&mount)
        .unwrap_or_else(|e| { eprintln!("mount {mount}: {e}"); std::process::exit(1); });

    host.start()
        .unwrap_or_else(|e| { eprintln!("start dispatcher: {e}"); std::process::exit(1); });

    info!("mounted {game_dir} at {mount} ({})", if args.readonly { "ro" } else { "rw" });

    if std::io::stdin().is_terminal() {
        match tui::run(&mount, Arc::clone(&shared)) {
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

/// Add the WinFSP bin directory to PATH so that winfsp_init_or_die can find
/// winfsp-x64.dll via LoadLibraryW.  Reads HKLM\SOFTWARE\WOW6432Node\WinFsp\InstallDir
/// using `reg query` (no extra dependencies).  Silent no-op if not installed or
/// if the DLL is already findable (e.g. WinFSP bin is already in PATH).
fn add_winfsp_bin_to_path() {
    let output = std::process::Command::new("reg")
        .args(["query", r"HKLM\SOFTWARE\WOW6432Node\WinFsp", "/v", "InstallDir"])
        .output();

    let output = match output {
        Ok(o) if o.status.success() => o,
        _ => return,
    };

    let text = String::from_utf8_lossy(&output.stdout);
    for line in text.lines() {
        if let Some(pos) = line.find("REG_SZ") {
            let install_dir = line[pos + "REG_SZ".len()..].trim().trim_end_matches('\\');
            let bin = format!("{install_dir}\\bin");
            let old_path = std::env::var("PATH").unwrap_or_default();
            // Prepend so the WinFSP DLL takes priority over any stale copy elsewhere.
            let _ = std::env::set_var("PATH", format!("{bin};{old_path}"));
            return;
        }
    }
}
