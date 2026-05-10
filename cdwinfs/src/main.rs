use std::io::IsTerminal;
use std::sync::Arc;

use clap::Parser;
use log::info;
use cdcore::VfsManager;

mod config;
mod fs;
mod prefab_view;
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

    /// Drive letter to mount on, single character (e.g. Y).
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

    /// Print third-party dependency licenses and exit.
    #[arg(long, exclusive = true)]
    licenses: bool,
}

const THIRD_PARTY_LICENSES_DEFLATE: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/licenses.deflate"));

fn main() {
    let args = Args::parse();

    if args.licenses {
        use std::io::Read;
        let mut out = String::new();
        flate2::read::DeflateDecoder::new(THIRD_PARTY_LICENSES_DEFLATE)
            .read_to_string(&mut out)
            .expect("decompress licenses");
        print!("{out}");
        return;
    }

    // Log to file so the TUI can own the console. Wrap in a flush-per-record
    // adapter so log entries land on disk immediately -- env_logger's default
    // Pipe target uses BufWriter, which means runtime errors (e.g. a failed
    // FBX import inside the close-spawned flush thread) only appear after
    // ~8KB of activity or a graceful shutdown that drains the buffer.
    struct FlushPerWrite(std::fs::File);
    impl std::io::Write for FlushPerWrite {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            let n = self.0.write(buf)?;
            self.0.flush()?;
            Ok(n)
        }
        fn flush(&mut self) -> std::io::Result<()> { self.0.flush() }
    }
    let f = std::fs::File::create("cdwinfs.log")
        .expect("cannot open cdwinfs.log");
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .target(env_logger::Target::Pipe(Box::new(FlushPerWrite(f))))
        .init();

    // Resolve game_dir + mount: CLI args -> saved config -> interactive setup.
    let (game_dir, mount) = match (args.game_dir.as_deref(), args.mount.as_deref()) {
        (Some(gd), Some(m)) => {
            let ch = m.chars().next().unwrap_or('\0');
            if m.len() != 1 || !ch.is_ascii_alphabetic() {
                eprintln!("error: drive must be a single letter, e.g. Y");
                std::process::exit(1);
            }
            let drive = ch.to_ascii_uppercase().to_string();
            let cfg = config::Config { game_dir: gd.to_string(), mount: drive.clone() };
            if let Err(e) = config::save(&cfg) {
                eprintln!("warning: could not save config: {e}");
            }
            (gd.to_string(), drive)
        }
        _ => {
            // No (or partial) CLI args -- try saved config first.
            let saved = config::load();
            let game_dir_hint = saved.as_ref()
                .map(|c| std::path::PathBuf::from(&c.game_dir))
                .or_else(|| setup::detect_game_dir());
            let drive_hint = saved.as_ref()
                .map(|c| c.mount.clone())
                .unwrap_or_else(|| setup::detect_free_drive().unwrap_or_else(|| "Y".to_string()));
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
    // winfsp_init only tries LoadLibraryW("winfsp-x64.dll"), which fails unless
    // the WinFSP bin dir is in PATH.  Add it from the registry ourselves.
    add_winfsp_bin_to_path();
    let _init = winfsp::winfsp_init().unwrap_or_else(|e| {
        // We deliberately do NOT use winfsp_init_or_die here -- that variant
        // calls process::exit with an opaque NTSTATUS and no message at all,
        // which is the worst possible UX for the most common first-run failure
        // (WinFSP not installed).
        eprintln!("error: failed to load WinFSP runtime ({e:?})");
        eprintln!();
        match winfsp_install_dir() {
            Some(dir) => {
                eprintln!("WinFSP appears to be installed at:");
                eprintln!("    {dir}");
                eprintln!();
                eprintln!("but `winfsp-x64.dll` could not be loaded.  Possible causes:");
                eprintln!("  - the install is for a different architecture (need x64)");
                eprintln!("  - the DLL or its dependencies are corrupted -- reinstall WinFSP");
                eprintln!("  - the WinFsp.Launcher service is disabled (services.msc)");
            }
            None => {
                eprintln!("WinFSP is not installed (no entry under");
                eprintln!("HKLM\\SOFTWARE\\WOW6432Node\\WinFsp in the registry).");
                eprintln!();
                eprintln!("Download and install WinFSP from:");
                eprintln!("    https://winfsp.dev/rel/");
                eprintln!();
                eprintln!("Then run cdwinfs again.");
            }
        }
        std::process::exit(1);
    });

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
        vfs.expose_multi_package_dirs();
    } else {
        let groups = vfs.list_groups().unwrap_or_else(|e| {
            log::warn!("list_groups failed: {e}");
            Vec::new()
        });
        info!("loading {} groups", groups.len());
        for g in &groups {
            vfs.load_group(g).unwrap_or_else(|e| log::warn!("{e}"));
        }
        vfs.expose_multi_package_dirs();
    }

    let cdfs = fs::CdWinFs::new(vfs, args.readonly, !args.no_auto_repack);
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

    let mount_point = format!("{}:", mount.trim_end_matches(':'));
    host.mount(&mount_point)
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

/// Read `HKLM\SOFTWARE\WOW6432Node\WinFsp\InstallDir` directly via the Win32
/// registry API.  Returns the install path with any trailing backslash
/// stripped, or `None` if WinFSP isn't installed (no registry entry).
///
/// In-process registry call (winreg crate) -- no `reg query` / `powershell`
/// subprocess.  Spawning either trips Sysmon process-creation telemetry that
/// matches over-broad Sigma rules in VirusTotal sandboxes.
fn winfsp_install_dir() -> Option<String> {
    use winreg::enums::{HKEY_LOCAL_MACHINE, KEY_READ};
    use winreg::RegKey;
    let key = RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey_with_flags(r"SOFTWARE\WOW6432Node\WinFsp", KEY_READ)
        .ok()?;
    let dir: String = key.get_value("InstallDir").ok()?;
    let dir = dir.trim_end_matches('\\').to_string();
    (!dir.is_empty()).then_some(dir)
}

/// Prepend the WinFSP bin directory to PATH so `winfsp_init` can find
/// winfsp-x64.dll via LoadLibraryW.  Silent no-op if WinFSP isn't installed --
/// the subsequent `winfsp_init` failure path surfaces a friendly error.
fn add_winfsp_bin_to_path() {
    let Some(dir) = winfsp_install_dir() else { return };
    let bin = format!("{dir}\\bin");
    let old_path = std::env::var("PATH").unwrap_or_default();
    // Prepend so the WinFSP DLL takes priority over any stale copy elsewhere.
    let _ = std::env::set_var("PATH", format!("{bin};{old_path}"));
}
