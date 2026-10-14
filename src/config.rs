//! Daemon settings: built-in defaults, then the optional config file
//! ($XDG_CONFIG_HOME/lince/config.json), then command-line flags.
//!
//! ```json
//! { "roots": ["~", "/mnt/data"], "exclude": [".git", "node_modules"],
//!   "socket": "/run/user/1000/lince/lince.sock", "watch": true,
//!   "max_watches": 100000 }
//! ```

use std::path::{Path, PathBuf};

use serde::Deserialize;

/// Excluded everywhere unless the config says otherwise: version control,
/// package caches and virtual environments, which are large and rarely what
/// anyone searches for by name.
pub const DEFAULT_EXCLUDES: &[&str] = &[
    ".git",
    "node_modules",
    ".cache",
    "__pycache__",
    ".npm",
    ".nvm",
    ".cargo",
    ".rustup",
    ".gradle",
    ".local/share/Trash",
    ".local/share/flatpak",
    ".venv",
    "venv",
];

#[derive(Deserialize, Default)]
struct File {
    roots: Option<Vec<String>>,
    exclude: Option<Vec<String>>,
    socket: Option<String>,
    snapshot: Option<String>,
    watch: Option<bool>,
    max_watches: Option<usize>,
}

pub struct Config {
    pub roots: Vec<PathBuf>,
    pub excludes: Vec<String>,
    pub socket: PathBuf,
    pub snapshot: PathBuf,
    pub watch: bool,
    /// None: 90% of fs.inotify.max_user_watches.
    pub max_watches: Option<usize>,
    pub exit_with_parent: bool,
}

fn env_dir(var: &str, fallback: &str) -> PathBuf {
    std::env::var_os(var)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(fallback))
}

fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))
}

pub fn expand(path: &str) -> PathBuf {
    match path.strip_prefix('~') {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => home().join(rest.trim_start_matches('/')),
        _ => PathBuf::from(path),
    }
}

pub fn default_socket() -> Result<PathBuf, String> {
    match std::env::var_os("XDG_RUNTIME_DIR").filter(|v| !v.is_empty()) {
        Some(dir) => Ok(PathBuf::from(dir).join("lince/lince.sock")),
        None => Ok(private_temp_dir()?.join("lince.sock")),
    }
}

/// Without XDG_RUNTIME_DIR the socket goes to a per-user directory in the
/// shared temp dir. It must be a real directory that only we can enter:
/// anyone could have created it first and put their own socket there.
fn private_temp_dir() -> Result<PathBuf, String> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};
    let uid = unsafe { libc::getuid() };
    let dir = std::env::temp_dir().join(format!("lince-{uid}"));
    match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(format!("{}: {e}", dir.display())),
    }
    let meta = std::fs::symlink_metadata(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    if !meta.is_dir() || meta.uid() != uid || meta.mode() & 0o077 != 0 {
        return Err(format!("{}: not a private directory owned by this user", dir.display()));
    }
    Ok(dir)
}

pub fn config_path() -> PathBuf {
    env_dir("XDG_CONFIG_HOME", ".config").join("lince/config.json")
}

/// Snapshot file for this exact set of roots and excludes, so differently
/// configured daemons never fight over one file.
fn snapshot_for(roots: &[PathBuf], excludes: &[String]) -> PathBuf {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for part in roots.iter().map(|r| r.to_string_lossy().into_owned()).chain(excludes.iter().cloned()) {
        for b in part.bytes().chain([0]) {
            h = (h ^ b as u64).wrapping_mul(0x100_0000_01b3);
        }
    }
    env_dir("XDG_CACHE_HOME", ".cache").join(format!("lince/index-{h:016x}.lnc"))
}

pub struct Overrides {
    pub config: Option<PathBuf>,
    pub roots: Vec<String>,
    pub excludes: Option<Vec<String>>,
    pub socket: Option<String>,
    pub snapshot: Option<String>,
    pub no_watch: bool,
    pub max_watches: Option<usize>,
    pub exit_with_parent: bool,
}

pub fn load(o: Overrides) -> Result<Config, String> {
    let path = o.config.clone().unwrap_or_else(config_path);
    let file: File = match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?,
        Err(_) if o.config.is_none() => File::default(),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    let roots: Vec<PathBuf> = if !o.roots.is_empty() {
        o.roots.iter().map(|r| expand(r)).collect()
    } else {
        file.roots.unwrap_or_else(|| vec!["~".into()]).iter().map(|r| expand(r)).collect()
    };
    let excludes = o
        .excludes
        .or(file.exclude)
        .unwrap_or_else(|| DEFAULT_EXCLUDES.iter().map(|s| s.to_string()).collect());
    let snapshot = o
        .snapshot
        .or(file.snapshot)
        .map(|s| expand(&s))
        .unwrap_or_else(|| snapshot_for(&roots, &excludes));
    Ok(Config {
        socket: match o.socket.or(file.socket) {
            Some(s) => expand(&s),
            None => default_socket()?,
        },
        watch: !o.no_watch && file.watch.unwrap_or(true),
        max_watches: o.max_watches.or(file.max_watches),
        exit_with_parent: o.exit_with_parent,
        roots,
        excludes,
        snapshot,
    })
}

impl Config {
    pub fn describe(&self) -> String {
        let roots: Vec<String> = self.roots.iter().map(|r| r.display().to_string()).collect();
        format!("roots {}, {} excludes, socket {}", roots.join(":"), self.excludes.len(), display(&self.socket))
    }
}

fn display(p: &Path) -> String {
    p.display().to_string()
}
