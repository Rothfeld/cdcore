use clap::Parser;
use log::info;
use crimsonforge_core::VfsManager;

mod fs;
mod virtual_files;

#[derive(Parser)]
#[command(name = "cdfuse", about = "Mount Crimson Desert archives as a read-only filesystem")]
struct Args {
    /// Path to the game install directory (contains 0000/, 0001/, meta/, …)
    packages: String,

    /// Mount point
    mount: String,

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

    let mut vfs = VfsManager::new(&args.packages).unwrap_or_else(|e| {
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

    let fs = fs::CdFs::new(vfs);

    let options = vec![
        fuser::MountOption::RO,
        fuser::MountOption::FSName("cdfuse".to_string()),
        fuser::MountOption::Subtype("cdfuse".to_string()),
        fuser::MountOption::AutoUnmount,
    ];

    info!("mounting {} at {}", args.packages, args.mount);
    fuser::mount2(fs, &args.mount, &options).unwrap_or_else(|e| {
        log::error!("mount failed: {e}");
        std::process::exit(1);
    });
}
