use std::io::IsTerminal;
use std::sync::{Arc, Mutex};

use clap::Parser;
use log::info;
use cdcore::VfsManager;

mod fs;
mod tui;
mod virtual_files;

#[derive(Parser)]
#[command(name = "cdfuse", about = "Mount Crimson Desert archives as a filesystem")]
struct Args {
    /// Commit pending writes and unmount: cdfuse --unmount <mountpoint>
    #[arg(long, value_name = "MOUNTPOINT", exclusive = true)]
    unmount: Option<String>,

    /// Path to the game install directory (contains 0000/, 0001/, meta/, ...)
    #[arg(required_unless_present = "unmount")]
    packages: Option<String>,

    /// Mount point
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
    if log_to_tui {
        let f = std::fs::File::create("/tmp/cdfuse.log")
            .expect("cannot open /tmp/cdfuse.log");
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
            .target(env_logger::Target::Pipe(Box::new(f)))
            .init();
    } else {
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    }

    // cdfuse --unmount <mountpoint>: signal the running cdfuse process to
    // repack and exit.  This is a cross-process call so an external mechanism
    // is unavoidable; fusermount is the standard FUSE utility for this.
    if let Some(ref mp) = args.unmount {
        let status = std::process::Command::new("fusermount")
            .args(["-u", mp])
            .status()
            .unwrap_or_else(|e| { eprintln!("fusermount: {e}"); std::process::exit(1); });
        std::process::exit(if status.success() { 0 } else { 1 });
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

    // Block SIGTERM before any threads are spawned so all inherit the mask.
    // Ctrl-C (SIGINT) aborts immediately with no repack.
    unsafe {
        let mut mask: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut mask);
        libc::sigaddset(&mut mask, libc::SIGTERM);
        libc::pthread_sigmask(libc::SIG_BLOCK, &mask, std::ptr::null_mut());
    }

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

    info!("mounting {} at {} ({})", packages, mount,
          if args.readonly { "ro" } else { "rw" });

    if std::io::stdin().is_terminal() {
        // Spawn the FUSE session in a background thread via spawn_mount2.
        // Dropping the BackgroundSession unmounts and blocks until destroy()
        // completes — no need to shell out to fusermount from our code.
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

        // SIGTERM: drop the session -> unmount -> destroy() -> repack -> exit.
        {
            let sess = Arc::clone(&session);
            std::thread::spawn(move || {
                unsafe {
                    let mut mask: libc::sigset_t = std::mem::zeroed();
                    libc::sigemptyset(&mut mask);
                    libc::sigaddset(&mut mask, libc::SIGTERM);
                    let mut sig: libc::c_int = 0;
                    libc::sigwait(&mask, &mut sig);
                }
                info!("SIGTERM -- graceful unmount");
                sess.lock().unwrap().take(); // blocks until destroy() finishes
                std::process::exit(0);
            });
        }

        match tui::run(mount, Arc::clone(&shared)) {
            tui::Action::Commit => {
                drop(shared);
                eprintln!("Repacking...");
                session.lock().unwrap().take(); // unmount + blocks until destroy()
            }
            tui::Action::Abort => {
                // Clear overlay so destroy() has nothing to repack, then drop
                // the session to unmount cleanly.  process::exit would bypass
                // BackgroundSession::drop and leave the mount broken.
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
