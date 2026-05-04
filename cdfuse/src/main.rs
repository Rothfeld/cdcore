use std::io::IsTerminal;
use std::sync::{Arc, Mutex};

use clap::Parser;
use log::info;
use cdcore::VfsManager;

mod config;
mod fs;
mod setup;
mod tui;
mod virtual_files;

#[derive(Parser)]
#[command(name = "cdfuse", about = "Mount Crimson Desert archives as a filesystem")]
struct Args {
    /// Commit pending writes and unmount: cdfuse --unmount <mountpoint>
    #[arg(long, value_name = "MOUNTPOINT", exclusive = true)]
    unmount: Option<String>,

    /// Path to the Crimson Desert install directory (contains 0000/, meta/, ...).
    /// Omit to load from saved config or run the interactive setup.
    #[arg(value_name = "GAME_DIR")]
    game_dir: Option<String>,

    /// Mount point (directory path, e.g. /mnt/cd).
    /// Omit to load from saved config or run the interactive setup.
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
    // Always log to /tmp/cdfuse.log — keeps both the TUI and daemon mode clean.
    let f = std::fs::File::create("/tmp/cdfuse.log")
        .expect("cannot open /tmp/cdfuse.log");
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .target(env_logger::Target::Pipe(Box::new(f)))
        .init();

    if args.unmount.is_some() {
        let mp = args.unmount.as_deref().unwrap();
        let status = std::process::Command::new("fusermount")
            .args(["-u", mp])
            .status()
            .unwrap_or_else(|e| { eprintln!("fusermount: {e}"); std::process::exit(1); });
        std::process::exit(if status.success() { 0 } else { 1 });
    }

    // Resolve game_dir + mount: CLI args → saved config → interactive TUI.
    let (game_dir, mount) = match (args.game_dir.as_deref(), args.mount.as_deref()) {
        (Some(gd), Some(m)) => {
            let cfg = config::Config { game_dir: gd.to_string(), mount: m.to_string() };
            if let Err(e) = config::save(&cfg) {
                eprintln!("warning: could not save config: {e}");
            }
            (gd.to_string(), m.to_string())
        }
        _ => {
            let saved = config::load();
            let game_dir_hint = saved.as_ref()
                .map(|c| std::path::PathBuf::from(&c.game_dir))
                .or_else(|| setup::detect_game_dir());
            let mount_hint = saved.as_ref()
                .map(|c| c.mount.clone())
                .unwrap_or_else(|| setup::detect_default_mount());

            match tui::select_paths(game_dir_hint, mount_hint) {
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

    let fs = fs::CdFs::new(vfs, args.readonly, !args.no_auto_repack);
    let shared = fs.shared();

    let mut options = vec![
        fuser::MountOption::FSName("cdfuse".to_string()),
        fuser::MountOption::Subtype("cdfuse".to_string()),
        fuser::MountOption::AutoUnmount,
    ];
    if args.readonly {
        options.push(fuser::MountOption::RO);
    }

    // Create the mount point directory if it doesn't exist.
    if let Err(e) = std::fs::create_dir_all(&mount) {
        eprintln!("warning: could not create mount point {mount}: {e}");
    }

    info!("mounting {game_dir} at {mount} ({})", if args.readonly { "ro" } else { "rw" });

    if std::io::stdin().is_terminal() {
        let session = match fuser::spawn_mount2(fs, &mount, &options) {
            Ok(s)  => s,
            Err(e) => {
                log::error!("mount failed: {e}");
                eprintln!("mount failed: {e}");
                std::process::exit(1);
            }
        };
        let session: Arc<Mutex<Option<fuser::BackgroundSession>>> =
            Arc::new(Mutex::new(Some(session)));

        match tui::run(&mount, Arc::clone(&shared)) {
            tui::Action::Abort => {
                shared.discard_pending();
                drop(shared);
                session.lock().unwrap().take();
            }
        }
    } else {
        fuser::mount2(fs, &mount, &options).unwrap_or_else(|e| {
            log::error!("mount failed: {e}");
            std::process::exit(1);
        });
    }
}
