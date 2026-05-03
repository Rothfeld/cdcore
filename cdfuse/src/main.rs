use std::io::IsTerminal;
use std::sync::Arc;
#[cfg(unix)]
use std::sync::Mutex;

use clap::Parser;
use log::info;
use cdcore::VfsManager;

mod fs;
mod tui;
mod virtual_files;
#[cfg(windows)]
mod fs_win;

#[derive(Parser)]
#[command(name = "cdfuse", about = "Mount Crimson Desert archives as a filesystem")]
struct Args {
    /// Commit pending writes and unmount: cdfuse --unmount <mountpoint>  [Linux only]
    #[arg(long, value_name = "MOUNTPOINT", exclusive = true)]
    unmount: Option<String>,

    /// Path to the game install directory (contains 0000/, 0001/, meta/, ...)
    #[arg(required_unless_present = "unmount")]
    packages: Option<String>,

    /// Mount point (Linux: directory path; Windows: drive letter, e.g. Z:)
    #[arg(required_unless_present = "unmount")]
    mount: Option<String>,

    /// Mount read-only (no writes to PAZ archives)
    #[arg(long)]
    readonly: bool,

    /// Load all package groups at mount time (default: lazy per-group load)
    #[arg(long)]
    preload: bool,

    /// Comma-separated list of specific groups to load (e.g. 0000,0001)
    #[arg(long, value_delimiter = ',')]
    groups: Vec<String>,
}

fn main() {
    let args = Args::parse();
    let log_to_tui = args.unmount.is_none() && std::io::stdin().is_terminal();

    #[cfg(unix)]
    let log_path = "/tmp/cdfuse.log";
    #[cfg(windows)]
    let log_path = {
        let mut p = std::env::temp_dir();
        p.push("cdfuse.log");
        p.to_string_lossy().to_string()
    };

    if log_to_tui {
        let f = std::fs::File::create(&log_path)
            .unwrap_or_else(|e| { eprintln!("cannot open {log_path}: {e}"); std::process::exit(1); });
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
            .target(env_logger::Target::Pipe(Box::new(f)))
            .init();
    } else {
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    }

    // --unmount: signal the running cdfuse process to repack and exit.
    // Uses fusermount on Linux; not supported on Windows (just kill the process).
    if args.unmount.is_some() {
        #[cfg(unix)]
        {
            let mp = args.unmount.as_deref().unwrap();
            let status = std::process::Command::new("fusermount")
                .args(["-u", mp])
                .status()
                .unwrap_or_else(|e| { eprintln!("fusermount: {e}"); std::process::exit(1); });
            std::process::exit(if status.success() { 0 } else { 1 });
        }
        #[cfg(windows)]
        {
            eprintln!("--unmount not supported on Windows; terminate the cdfuse process instead");
            std::process::exit(1);
        }
    }

    let packages = args.packages.as_deref().unwrap();
    let mount    = args.mount.as_deref().unwrap();

    let vfs = VfsManager::new(packages).unwrap_or_else(|e| {
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

    info!("mounting {} at {} ({})", packages, mount,
          if args.readonly { "ro" } else { "rw" });

    #[cfg(unix)]
    run_unix(vfs, mount, &args);

    #[cfg(windows)]
    run_windows(vfs, mount, &args);
}

#[cfg(unix)]
fn run_unix(vfs: VfsManager, mount: &str, args: &Args) {
    let fs     = fs::CdFs::new(vfs, args.readonly);
    let shared = fs.shared();

    let mut options = vec![
        fuser::MountOption::FSName("cdfuse".to_string()),
        fuser::MountOption::Subtype("cdfuse".to_string()),
        fuser::MountOption::AutoUnmount,
    ];
    if args.readonly {
        options.push(fuser::MountOption::RO);
    }

    if std::io::stdin().is_terminal() {
        let session = match fuser::spawn_mount2(fs, mount, &options) {
            Ok(s)  => s,
            Err(e) => {
                log::error!("mount failed: {e}");
                eprintln!("mount failed: {e}");
                std::process::exit(1);
            }
        };
        let session: Arc<Mutex<Option<fuser::BackgroundSession>>> =
            Arc::new(Mutex::new(Some(session)));

        match tui::run(mount, Arc::clone(&shared)) {
            tui::Action::Commit => {
                drop(shared);
                eprintln!("Repacking...");
                session.lock().unwrap().take();
            }
            tui::Action::Abort => {
                shared.discard_pending();
                drop(shared);
                session.lock().unwrap().take();
            }
        }
    } else {
        fuser::mount2(fs, mount, &options).unwrap_or_else(|e| {
            log::error!("mount failed: {e}");
            std::process::exit(1);
        });
    }
}

#[cfg(windows)]
fn run_windows(vfs: VfsManager, mount: &str, args: &Args) {
    use std::ffi::{OsStr, OsString};
    use winfsp::host::{FileSystemHost, FileSystemParams, MountPoint, VolumeParams};

    let fs_win = fs_win::CdFsWin::new(vfs, args.readonly);
    let shared = fs_win.shared();

    let mut volume_params = VolumeParams::new();
    volume_params
        .sector_size(512)
        .sectors_per_allocation_unit(1)
        .max_component_length(255)
        .case_sensitive_search(false)
        .case_preserved_names(true)
        .unicode_on_disk(true)
        .read_only_volume(args.readonly)
        .filesystem_name("cdfuse");

    let params = FileSystemParams::default_params(volume_params);
    let mut host = FileSystemHost::new_with_options(params, fs_win)
        .unwrap_or_else(|e| {
            log::error!("WinFsp init failed: {e}");
            eprintln!("WinFsp init failed: {e}");
            eprintln!("Is WinFsp installed? See https://winfsp.dev/rel/");
            std::process::exit(1);
        });

    // MountPoint requires 'static; leak the drive-letter string.
    // This is a single-mount CLI tool — the string lives for the whole process.
    let mount_static: &'static OsStr =
        Box::leak(OsString::from(mount).into_boxed_os_str());
    host.mount(MountPoint::MountPoint(mount_static))
        .unwrap_or_else(|e| {
            log::error!("mount failed: {e}");
            eprintln!("mount failed: {e}");
            std::process::exit(1);
        });

    host.start()
        .unwrap_or_else(|e| {
            log::error!("start failed: {e}");
            eprintln!("start failed: {e}");
            std::process::exit(1);
        });

    info!("mounted at {mount}");

    if std::io::stdin().is_terminal() {
        match tui::run(mount, Arc::clone(&shared)) {
            tui::Action::Commit => {
                eprintln!("Repacking...");
                host.stop();
            }
            tui::Action::Abort => {
                shared.discard_pending();
                host.stop();
            }
        }
    } else {
        // Non-interactive: run until process is terminated.
        // WinFsp dispatcher calls dispatcher_stopped() on shutdown.
        loop { std::thread::sleep(std::time::Duration::from_secs(3600)); }
    }
}
