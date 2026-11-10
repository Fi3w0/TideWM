//! Explicit user-requested config setup. Never changes an existing ebb.wave.
use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};
const DEFAULT_EBB: &str = include_str!("../../../share/defaults/ebb.wave");

pub fn init(config: &Path) -> io::Result<serde_json::Value> {
    let original = fs::read_to_string(config)?;
    let target = config.canonicalize()?; // preserve the requested symlink itself
    let permissions = fs::metadata(&target)?.permissions();
    let ebb = config.parent().unwrap_or(Path::new(".")).join("ebb.wave");
    let included = original.lines().any(|line| {
        matches!(
            line.split('#').next().unwrap_or("").trim(),
            "include \"ebb.wave\"" | "include 'ebb.wave'" | "include ebb.wave"
        )
    });
    if !included
        && original.lines().any(|line| {
            let line = line.trim();
            ["ebb", "ebb_advanced"].iter().any(|name| {
                line.strip_prefix(name)
                    .is_some_and(|rest| rest.trim_start().starts_with('{'))
            })
        })
    {
        return Err(io::Error::other("main config already has inline Ebb settings; move them to ebb.wave and include it, so generated defaults cannot shadow your settings"));
    }
    let created = match OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&ebb)
    {
        Ok(mut file) => {
            file.write_all(DEFAULT_EBB.as_bytes())?;
            file.sync_all()?;
            true
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            if !fs::metadata(&ebb)?.is_file() {
                return Err(io::Error::other("existing ebb.wave is not a regular file"));
            }
            false
        }
        Err(error) => return Err(error),
    };
    if included {
        return Ok(serde_json::json!({"file":ebb,"created":created,"include_added":false}));
    }
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(io::Error::other)?
        .as_nanos();
    let parent = target.parent().unwrap_or(Path::new("."));
    let stem = target
        .file_name()
        .ok_or_else(|| io::Error::other("main config has no filename"))?
        .to_string_lossy();
    let backup = parent.join(format!("{stem}.ebb-init-{nonce}.bak"));
    let temp = parent.join(format!(".{stem}.ebb-init-{nonce}.tmp"));
    let mut saved = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&backup)?;
    saved.write_all(original.as_bytes())?;
    saved.set_permissions(permissions.clone())?;
    saved.sync_all()?;
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)?;
        file.write_all(original.as_bytes())?;
        if !original.ends_with('\n') {
            file.write_all(b"\n")?;
        }
        file.write_all(
            b"\n# Ebb resource settings (see DOCUMENTATION.md).\ninclude \"ebb.wave\"\n",
        )?;
        file.set_permissions(permissions)?;
        file.sync_all()?;
        if fs::read_to_string(&target)? != original {
            return Err(io::Error::other(
                "main config changed during init; leaving it alone",
            ));
        }
        fs::rename(&temp, &target)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result?;
    Ok(serde_json::json!({"file":ebb,"created":created,"include_added":true,"backup":backup}))
}

/// Adds each listed app's own memory to an `ebb-stats` reply. Read here, in
/// the CLI: `smaps_rollup` of a large client can take milliseconds, which the
/// compositor's main loop should never spend on a report.
pub fn add_client_memory(data: &mut serde_json::Value) {
    // `get_mut`, not indexing: indexing would insert the missing keys.
    let Some(entries) = data
        .get_mut("apps")
        .and_then(|apps| apps.get_mut("entries"))
        .and_then(serde_json::Value::as_array_mut)
    else {
        return;
    };
    for entry in entries {
        let pss = entry["pid"]
            .as_i64()
            .and_then(|pid| fs::read_to_string(format!("/proc/{pid}/smaps_rollup")).ok())
            .and_then(|text| pss_bytes(&text));
        entry["client_pss_bytes"] = serde_json::json!(pss);
    }
}

fn pss_bytes(rollup: &str) -> Option<u64> {
    rollup.lines().find_map(|line| {
        line.strip_prefix("Pss:")?
            .split_whitespace()
            .next()?
            .parse::<u64>()
            .ok()?
            .checked_mul(1024)
    })
}

pub fn default_config_path() -> io::Result<PathBuf> {
    let base =
        if let Some(path) = std::env::var_os("XDG_CONFIG_HOME").filter(|p| !p.is_empty()) {
            PathBuf::from(path)
        } else {
            PathBuf::from(std::env::var_os("HOME").ok_or_else(|| {
                io::Error::other("HOME and XDG_CONFIG_HOME are unset; use --config")
            })?)
            .join(".config")
        };
    Ok(base.join("tidewm/config.wave"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{symlink, PermissionsExt};
    #[test]
    fn init_preserves_symlinks_settings_permissions_and_is_idempotent() {
        let root = std::env::temp_dir().join(format!("tide-ebb-init-{}", std::process::id()));
        fs::create_dir_all(root.join("requested")).unwrap();
        fs::create_dir_all(root.join("actual")).unwrap();
        let target = root.join("actual/config.wave");
        fs::write(&target, "gaps = 9\n").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o640)).unwrap();
        let config = root.join("requested/config.wave");
        symlink(&target, &config).unwrap();
        let first = init(&config).unwrap();
        assert_eq!(first["include_added"], true);
        assert!(config.is_symlink());
        assert!(target.read_link().is_err());
        assert_eq!(
            fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o640
        );
        assert_eq!(
            fs::read_to_string(first["backup"].as_str().unwrap()).unwrap(),
            "gaps = 9\n"
        );
        let ebb = root.join("requested/ebb.wave");
        assert!(ebb.exists());
        fs::write(&ebb, "ebb {\n mode = off\n}\n").unwrap();
        let second = init(&config).unwrap();
        assert_eq!(second["created"], false);
        assert_eq!(second["include_added"], false);
        assert!(fs::read_to_string(&ebb).unwrap().contains("mode = off"));
        assert_eq!(
            fs::read_to_string(&target)
                .unwrap()
                .matches("include \"ebb.wave\"")
                .count(),
            1
        );
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn client_memory_is_added_per_app_and_unknown_stays_null() {
        assert_eq!(
            pss_bytes("Rss: 9 kB\nPss:   120 kB\nPss_Dirty: 4 kB\n"),
            Some(122_880)
        );
        assert_eq!(pss_bytes("Rss: 9 kB\n"), None);
        let mut data = serde_json::json!({"apps": {"entries": [
            {"app_id": "self", "pid": std::process::id()},
            {"app_id": "gone", "pid": null},
        ]}});
        add_client_memory(&mut data);
        let entries = data["apps"]["entries"].as_array().unwrap();
        assert!(entries[0]["client_pss_bytes"]
            .as_u64()
            .is_some_and(|pss| pss > 0));
        assert!(entries[1]["client_pss_bytes"].is_null());
        // Replies from an older compositor have no app list; leave them alone.
        let mut old = serde_json::json!({"phase": 2});
        add_client_memory(&mut old);
        assert_eq!(old, serde_json::json!({"phase": 2}));
    }

    #[test]
    fn init_refuses_to_shadow_inline_settings() {
        let root = std::env::temp_dir().join(format!("tide-ebb-inline-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let config = root.join("config.wave");
        let original = "ebb {\n mode = off\n}\n";
        fs::write(&config, original).unwrap();
        assert!(init(&config).is_err());
        assert_eq!(fs::read_to_string(&config).unwrap(), original);
        assert!(!root.join("ebb.wave").exists());
        fs::remove_dir_all(root).unwrap();
    }
}
