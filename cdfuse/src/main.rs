use clap::Parser;
use log::info;
use crimsonforge_core::VfsManager;

mod fs;
mod virtual_files;

#[derive(Parser)]
#[command(name = "cdfuse", about = "Mount Crimson Desert archives as a filesystem")]
struct Args {
    /// Path to the game install directory (contains 0000/, 0001/, meta/, ...)
    packages: String,

    /// Mount point
    mount: String,

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

    let vfs = VfsManager::new(&args.packages).unwrap_or_else(|e| {
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

    // Block SIGTERM before any thread is spawned (rayon pool, fuser session)
    // so all threads inherit the mask and sigwait below receives it exclusively.
    // Ctrl-C (SIGINT) keeps default behaviour: abort immediately, no repack.
    // SIGTERM: graceful — fusermount -u triggers destroy() which repacks.
    unsafe {
        let mut mask: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut mask);
        libc::sigaddset(&mut mask, libc::SIGTERM);
        libc::pthread_sigmask(libc::SIG_BLOCK, &mask, std::ptr::null_mut());
    }

    let fs = fs::CdFs::new(vfs, args.readonly);

    // Graceful shutdown thread: waits for SIGTERM, then unmounts cleanly.
    {
        let mount = args.mount.clone();
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
                .args(["-u", &mount])
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

    info!("mounting {} at {} ({})", args.packages, args.mount,
          if args.readonly { "ro" } else { "rw" });
    fuser::mount2(fs, &args.mount, &options).unwrap_or_else(|e| {
        log::error!("mount failed: {e}");
        std::process::exit(1);
    });
}
