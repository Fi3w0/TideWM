use super::*;

type RawConfigLoad = (RawConfig, Vec<String>, Vec<waves::Entry>, Vec<PathBuf>);

/// Owns notify registrations for the exact Wave source graph. Direct parent
/// directories catch writes/removes/atomic renames; one existing ancestor
/// keeps a deleted or not-yet-created include directory recoverable. Both
/// lexical paths and canonical symlink targets are relevant.
pub struct ConfigWatcher {
    watcher: RecommendedWatcher,
    relevant_paths: Arc<RwLock<HashSet<PathBuf>>>,
    watched_dirs: HashSet<PathBuf>,
}

impl ConfigWatcher {
    /// Replaces the watched include graph after a successful transactional
    /// reload. New directories are registered before obsolete ones are
    /// removed, so an edit cannot fall into a handoff gap.
    pub(crate) fn update_paths(&mut self, paths: &[PathBuf]) {
        let relevant = expanded_watch_paths(paths);
        let directories = watch_directories(&relevant);

        let additions: Vec<_> = directories
            .difference(&self.watched_dirs)
            .cloned()
            .collect();
        for directory in additions {
            match self.watcher.watch(&directory, RecursiveMode::NonRecursive) {
                Ok(()) => {
                    self.watched_dirs.insert(directory);
                }
                Err(err) => tracing::warn!(
                    %err,
                    path = %directory.display(),
                    "Failed to watch Wave source directory"
                ),
            }
        }
        if let Ok(mut active) = self.relevant_paths.write() {
            *active = relevant;
        }

        let obsolete: Vec<_> = self
            .watched_dirs
            .difference(&directories)
            .cloned()
            .collect();
        for directory in obsolete {
            if let Err(err) = self.watcher.unwatch(&directory) {
                tracing::debug!(%err, path = %directory.display(), "Failed to retire Wave watch");
            }
            self.watched_dirs.remove(&directory);
        }
    }

    /// A creation event for a previously missing parent can make a closer
    /// watch anchor available before the debounced reload runs.
    pub(crate) fn refresh_anchors(&mut self) {
        let paths = self
            .relevant_paths
            .read()
            .map(|paths| paths.iter().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        self.update_paths(&paths);
    }
}

fn absolute_watch_path(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    }
}

fn expanded_watch_paths(paths: &[PathBuf]) -> HashSet<PathBuf> {
    let mut expanded = HashSet::new();
    for path in paths {
        let absolute = absolute_watch_path(path);
        expanded.insert(absolute.clone());
        if let Ok(canonical) = absolute.canonicalize() {
            expanded.insert(canonical);
        }
    }
    expanded
}

fn nearest_existing_directory(path: &Path) -> Option<PathBuf> {
    let mut candidate = path.parent();
    while let Some(directory) = candidate {
        if directory.is_dir() {
            return Some(directory.to_path_buf());
        }
        candidate = directory.parent();
    }
    None
}

pub(super) fn watch_directories(paths: &HashSet<PathBuf>) -> HashSet<PathBuf> {
    let mut directories = HashSet::new();
    for path in paths {
        let Some(anchor) = nearest_existing_directory(path) else {
            continue;
        };
        directories.insert(anchor.clone());
        // If the direct parent itself disappears, its parent reports that
        // removal/recreation. Avoid registering the filesystem root.
        if path.parent() == Some(anchor.as_path()) {
            if let Some(parent) = anchor.parent().filter(|parent| parent.parent().is_some()) {
                directories.insert(parent.to_path_buf());
            }
        }
    }
    directories
}

pub(super) fn event_affects_paths(event_path: &Path, paths: &HashSet<PathBuf>) -> bool {
    let absolute = absolute_watch_path(event_path);
    let canonical = absolute.canonicalize().ok();
    paths.iter().any(|tracked| {
        tracked == &absolute
            || canonical.as_ref().is_some_and(|path| tracked == path)
            || tracked.starts_with(&absolute)
            || canonical
                .as_ref()
                .is_some_and(|path| tracked.starts_with(path))
    })
}

/// Starts the event-driven watcher for the already-parsed source graph and
/// forwards a coalesced token into the returned calloop channel. Deletion is
/// as relevant as modify/create: removing an include must drop its old values.
pub fn spawn_watcher(
    source_paths: &[PathBuf],
) -> notify::Result<(ConfigWatcher, Channel<Arc<AtomicBool>>)> {
    let (tx, rx) = channel::channel();
    let event_pending = Arc::new(AtomicBool::new(false));
    let relevant_paths = Arc::new(RwLock::new(expanded_watch_paths(source_paths)));
    let callback_paths = relevant_paths.clone();
    let watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        let Ok(event) = res else { return };
        if !(event.kind.is_modify() || event.kind.is_create() || event.kind.is_remove()) {
            return;
        }
        let relevant = callback_paths.read().is_ok_and(|paths| {
            event
                .paths
                .iter()
                .any(|path| event_affects_paths(path, &paths))
        });
        if relevant
            && !event_pending.swap(true, Ordering::AcqRel)
            && tx.send(event_pending.clone()).is_err()
        {
            event_pending.store(false, Ordering::Release);
        }
    })?;
    let mut watcher = ConfigWatcher {
        watcher,
        relevant_paths,
        watched_dirs: HashSet::new(),
    };
    watcher.update_paths(source_paths);
    Ok((watcher, rx))
}

/// Reads `path` as config, resolving `include` statements first (see
/// `waves::resolve`), then lowers the merged entry list into a
/// `RawConfig`. The single entry point both `Config::load` and
/// `Config::reload` use so they stay consistent about how includes are
/// resolved.
///
/// The Wave engine is the only grammar. A parse error reports the Wave
/// parser's message with file/line detail.
#[cfg(test)]
pub(super) fn load_raw_config(path: &Path) -> Result<RawConfigLoad, String> {
    let lua = wave::new_lua().map_err(|e| format!("in file {}: {e}", path.display()))?;
    load_raw_config_in(&lua, &wave::TideInfo::default(), path)
}

/// [`load_raw_config`] on a caller-owned session Lua with live-compositor
/// facts: the runtime path, so hardware conditionals
/// (`if tide.backend == "udev" then ...`) see the real machine and the
/// resulting globals persist for `tidectl eval` and `on` handlers.
pub(super) fn load_raw_config_in(
    lua: &mlua::Lua,
    tide: &wave::TideInfo,
    path: &Path,
) -> Result<RawConfigLoad, String> {
    let (entries, warnings, source_paths) = wave::resolve_with_lua(lua, tide, path)?;

    let raw = lower_entries(&entries);
    Ok((raw, warnings, entries, source_paths))
}

static CONFIG_PATH_OVERRIDE: RwLock<Option<PathBuf>> = RwLock::new(None);

/// Overrides the config path (`-c`/`--config` at startup, see `main.rs`, and
/// `load-config-file` at runtime). `config_path()` reads it on every call,
/// including the hot-reload watcher's own path lookup, so one override
/// covers both. Returns the previous override so a failed runtime switch can
/// put it back.
pub fn set_config_path_override(path: PathBuf) -> Option<PathBuf> {
    replace_config_path_override(Some(path))
}

/// Sets or clears the override, returning what it was.
pub fn replace_config_path_override(path: Option<PathBuf>) -> Option<PathBuf> {
    match CONFIG_PATH_OVERRIDE.write() {
        Ok(mut guard) => std::mem::replace(&mut *guard, path),
        Err(_) => None,
    }
}

pub(super) fn config_path() -> PathBuf {
    if let Some(path) = CONFIG_PATH_OVERRIDE
        .read()
        .ok()
        .and_then(|guard| guard.clone())
    {
        return path;
    }
    if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME") {
        return PathBuf::from(dir).join("tidewm").join("config.wave");
    }
    let Some(home) = std::env::var_os("HOME") else {
        return match private_temp_config_path() {
            Ok(path) => {
                tracing::warn!(
                    path = %path.display(),
                    "Neither XDG_CONFIG_HOME nor HOME is set; using private temporary config storage"
                );
                path
            }
            Err(err) => {
                // `/dev/null` is a safe read-only empty config. This is the
                // fail-closed path for a stripped environment where neither
                // a user identity nor a private temporary directory can be
                // verified; never load executable Wave from a shared path.
                tracing::error!(%err, "No private fallback config directory is available; using in-memory defaults");
                PathBuf::from("/dev/null")
            }
        };
    };
    PathBuf::from(home)
        .join(".config")
        .join("tidewm")
        .join("config.wave")
}

fn private_temp_config_path() -> std::io::Result<PathBuf> {
    let uid = fs::metadata("/proc/self")?.uid();
    let dir = std::env::temp_dir().join(format!("tidewm-config-{uid}"));
    ensure_private_owned_dir(&dir, uid)?;
    Ok(dir.join("config.wave"))
}

pub(super) fn ensure_private_owned_dir(dir: &Path, uid: u32) -> std::io::Result<()> {
    match fs::symlink_metadata(dir) {
        Ok(metadata) => {
            if !metadata.file_type().is_dir() || metadata.uid() != uid {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    format!(
                        "temporary config directory {} is not an owned directory",
                        dir.display()
                    ),
                ));
            }
            if metadata.permissions().mode() & 0o077 != 0 {
                fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            fs::DirBuilder::new().mode(0o700).create(dir)?;
        }
        Err(err) => return Err(err),
    }

    let metadata = fs::symlink_metadata(dir)?;
    if !metadata.file_type().is_dir()
        || metadata.uid() != uid
        || metadata.permissions().mode() & 0o077 != 0
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "temporary config directory {} failed ownership/privacy verification",
                dir.display()
            ),
        ));
    }
    Ok(())
}
