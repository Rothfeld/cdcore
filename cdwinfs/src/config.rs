use std::path::PathBuf;

pub struct Config {
    pub game_dir: String,
    pub mount:    String,
}

pub fn config_path() -> PathBuf {
    let base = std::env::var("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."));
    base.join("CrimsonForge").join("cdwinfs.cfg")
}

pub fn load() -> Option<Config> {
    let text = std::fs::read_to_string(config_path()).ok()?;
    let mut game_dir = None;
    let mut mount    = None;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('#') || line.is_empty() { continue; }
        if let Some(v) = line.strip_prefix("game_dir=") { game_dir = Some(v.to_string()); }
        if let Some(v) = line.strip_prefix("mount=")    { mount    = Some(v.to_string()); }
    }
    Some(Config { game_dir: game_dir?, mount: mount? })
}

pub fn save(cfg: &Config) -> std::io::Result<()> {
    let path = config_path();
    if let Some(parent) = path.parent() { std::fs::create_dir_all(parent)?; }
    std::fs::write(&path, format!("game_dir={}\nmount={}\n", cfg.game_dir, cfg.mount))
}
