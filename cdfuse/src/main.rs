use clap::Parser;
use log::info;
use crimsonforge_core::VfsManager;

mod fs;
mod virtual_files;

#[derive(Parser)]
#[command(name = "cdfuse", about = "Mount Crimson Desert archives as a filesystem")]
struct Args {
    /// Unmount and flush pending writes: cdfuse --unmount <mountpoint>
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
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args = Args::parse();

    // cdfuse --unmount <mountpoint>: flush pending writes and unmount.
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
        let groups = vfs.list_groups().unwrap_or_default();
        info!("loading {} groups", groups.len());
        for g in &groups {
            vfs.load_group(g).unwrap_or_else(|e| log::warn!("{e}"));
        }
    }

    // Block SIGTERM before any thread is spawned so all threads inherit the
    // mask. Ctrl-C (SIGINT) keeps default behaviour: abort, no repack.
    unsafe {
        let mut mask: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut mask);
        libc::sigaddset(&mut mask, libc::SIGTERM);
        libc::pthread_sigmask(libc::SIG_BLOCK, &mask, std::ptr::null_mut());
    }

    let fs = fs::CdFs::new(vfs, args.readonly);

    // Graceful shutdown thread: SIGTERM → fusermount -u → destroy() → repack.
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
            info!("SIGTERM — graceful unmount, repacking pending writes");
            std::process::Command::new("fusermount")
                .args(["-u", &mp])
                .status()
                .ok();
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
    fuser::mount2(fs, mount, &options).unwrap_or_else(|e| {
        log::error!("mount failed: {e}");
        std::process::exit(1);
    });
}
