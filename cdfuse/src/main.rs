use std::io::IsTerminal;
use std::sync::Arc;

use clap::Parser;
use log::info;
use crimsonforge_core::VfsManager;

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
    // In TUI mode log to file so output doesn't corrupt the display.
    // In non-interactive mode log to stderr as usual.
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

    // cdfuse --unmount <mountpoint>: commit pending writes and unmount.
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

    let fs = fs::CdFs::new(vfs, args.readonly);
    let shared = fs.shared();

    // SIGTERM: graceful unmount -> destroy() -> repack.
    {
        let mp = mount.to_string();
        std::thread::spawn(move || {
            unsafe {
                let mut mask: libc::sigset_t = std::mem::zeroed();
                libc::sigemptyset(&mut mask);
                libc::sigaddset(&mut mask, libc::SIGTERM);
                let mut sig: libc::c_int = 0;
                libc::sigwait(&mask, &mut sig);
            }
            info!("SIGTERM -- graceful unmount");
            match std::process::Command::new("fusermount").args(["-u", &mp]).status() {
                Ok(s) if s.success() => {}
                Ok(s)  => log::warn!("fusermount -u failed: exit {s}"),
                Err(e) => log::warn!("fusermount -u error: {e}"),
            }
        });
    }

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

    // Interactive mode: TUI in main thread, FUSE session in background.
    // Non-interactive (piped/scripted): block in main thread as before.
    if std::io::stdin().is_terminal() {
        let mount_str = mount.to_string();
        // Channel used only at startup to detect immediate mount failures
        // (e.g. stale mount point) before the TUI is shown.
        let (tx, rx) = std::sync::mpsc::channel::<std::io::Result<()>>();
        let fuse_thread = std::thread::spawn(move || {
            let r = fuser::mount2(fs, &mount_str, &options);
            tx.send(r).ok();
        });

        // Give mount2 up to 200ms to fail.  If it does, report the error and
        // exit before the TUI is opened so the message is readable.
        match rx.recv_timeout(std::time::Duration::from_millis(200)) {
            Ok(Err(e)) => {
                log::error!("mount failed: {e}");
                eprintln!("mount failed: {e}");
                fuse_thread.join().ok();
                std::process::exit(1);
            }
            // Ok(Ok(())) = mount succeeded AND returned already (unmounted immediately).
            // Err(Timeout) = still running after 200ms = normal startup.
            _ => {}
        }

        match tui::run(mount, Arc::clone(&shared)) {
            tui::Action::Commit => {
                drop(shared);
                eprintln!("Repacking...");
                match std::process::Command::new("fusermount").args(["-u", mount]).status() {
                    Ok(s) if s.success() => {}
                    Ok(s)  => eprintln!("fusermount -u failed: exit {s}"),
                    Err(e) => eprintln!("fusermount -u error: {e}"),
                }
                if let Err(e) = fuse_thread.join() {
                    eprintln!("FUSE session thread panicked: {e:?}");
                }
            }
            tui::Action::Abort => {
                drop(shared);
                std::process::exit(0);
            }
        }
    } else {
        fuser::mount2(fs, mount, &options).unwrap_or_else(|e| {
            log::error!("mount failed: {e}");
            std::process::exit(1);
        });
    }
}
