//! Optional NVIDIA data-file check. Kept out of compositor/runtime code.
use super::{compositor_pid, read_proc_environ, Check, Diagnostics, Verdict};
use std::{io::Read, os::unix::fs::OpenOptionsExt, path::Path};

const PROFILE: &str = "50-tidewm.json";
const BUNDLED: &str = include_str!("../../../share/nvidia/50-tidewm.json");
const LIMIT: u64 = 64 * 1024;

pub(super) fn check(diagnostics: Option<&Diagnostics>) -> Check {
    let name = "NVIDIA memory profile";
    let Some(pid) = diagnostics.and_then(|diag| compositor_pid(&diag.socket)) else {
        return Check::new(
            name,
            Verdict::Skip,
            "compositor not running; driver use unknown",
        );
    };
    let Ok(maps) = std::fs::read_to_string(format!("/proc/{pid}/maps")) else {
        return Check::new(
            name,
            Verdict::Skip,
            "cannot inspect the selected compositor's OpenGL libraries",
        );
    };
    if !uses_nvidia_opengl(&maps) {
        return Check::new(
            name,
            Verdict::Skip,
            "selected compositor has no proprietary NVIDIA OpenGL library mapped",
        );
    }
    let env = read_proc_environ(pid);
    let home = env
        .get("HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(std::path::PathBuf::from));
    let globals = home.as_ref().and_then(|home| {
        read_file(&home.join(".nv/nvidia-application-profile-globals-rc"))
            .ok()
            .flatten()
    });
    if profiles_disabled(
        env.get("__GL_APPLICATION_PROFILE").map(String::as_str),
        globals.as_deref(),
    ) {
        return Check::new(
            name,
            Verdict::Skip,
            "NVIDIA application profiles explicitly disabled; leaving that preference alone",
        );
    }
    // NVIDIA's documented search locations, only for this optional host-side
    // diagnostic. No vendor paths or driver choices enter the compositor.
    let mut paths = Vec::new();
    if let Some(home) = home {
        paths.push(
            home.join(".nv/nvidia-application-profiles-rc.d")
                .join(PROFILE),
        );
    }
    paths.push(Path::new("/etc/nvidia/nvidia-application-profiles-rc.d").join(PROFILE));
    for path in paths {
        match read_file(&path) {
            Ok(None) => {},
            Ok(Some(text)) if is_bundled_profile(&text) => return Check::new(name, Verdict::Pass,
                format!("TideWM profile present at {}; driver activation and other overrides are not verified", path.display())),
            Ok(Some(_)) => return Check::new(name, Verdict::Skip,
                format!("{} is customized; effective NVIDIA settings are not evaluated", path.display())),
            Err(reason) => return Check::new(name, Verdict::Skip,
                format!("cannot verify {}: {reason}", path.display())),
        }
    }
    Check::new(name, Verdict::Warn,
        "optional TideWM NVIDIA memory-reuse profile not found; TideWM and Ebb still work. See DOCUMENTATION.md, NVIDIA memory profile; custom profiles under other filenames are not evaluated")
}

fn uses_nvidia_opengl(maps: &str) -> bool {
    maps.lines()
        .filter_map(|line| line.split_whitespace().nth(5))
        .any(|path| {
            let basename = Path::new(path)
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("");
            ["libnvidia-glcore.so", "libnvidia-eglcore.so"]
                .iter()
                .any(|library| {
                    basename
                        .strip_prefix(library)
                        .is_some_and(|suffix| suffix.is_empty() || suffix.starts_with('.'))
                })
        })
}

fn profiles_disabled(env: Option<&str>, globals: Option<&str>) -> bool {
    match env {
        Some("0") => true,
        Some("1") => false,
        _ => globals
            .and_then(|text| serde_json::from_str::<serde_json::Value>(text).ok())
            .is_some_and(|value| value["enabled"] == false),
    }
}

fn is_bundled_profile(text: &str) -> bool {
    // Compare parsed data, not whitespace. This deliberately does not try to
    // reimplement NVIDIA's extended JSON, rule precedence or full search path.
    let expected: serde_json::Value =
        serde_json::from_str(BUNDLED).expect("valid bundled NVIDIA profile");
    serde_json::from_str::<serde_json::Value>(text).is_ok_and(|value| value == expected)
}

fn read_file(path: &Path) -> Result<Option<String>, String> {
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.to_string()),
    };
    if !metadata.is_file() || metadata.len() > LIMIT {
        return Err("not a regular profile file within the 64 KiB diagnostic limit".into());
    }
    let mut text = String::new();
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
        .map_err(|error| error.to_string())?;
    if !file
        .metadata()
        .map_err(|error| error.to_string())?
        .is_file()
    {
        return Err("profile changed to a non-regular file".into());
    }
    file.take(LIMIT + 1)
        .read_to_string(&mut text)
        .map_err(|error| error.to_string())?;
    if text.len() as u64 > LIMIT {
        return Err("profile exceeds the diagnostic limit".into());
    }
    Ok(Some(text))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checks_the_selected_process_not_other_loaded_gpu_modules() {
        assert!(uses_nvidia_opengl(
            "0-1 r-xp 0000 00:00 0 /usr/lib/libnvidia-glcore.so.595.104.02\n"
        ));
        assert!(uses_nvidia_opengl(
            "0-1 r-xp 0000 00:00 0 /nix/store/test/lib/libnvidia-eglcore.so.1 (deleted)\n"
        ));
        assert!(!uses_nvidia_opengl(
            "0-1 r-xp 0000 00:00 0 /usr/lib/libgallium.so\n"
        ));
        assert!(!uses_nvidia_opengl(
            "0-1 r-xp 0000 00:00 0 /usr/lib/libnvidia-glcore.so_fake\n"
        ));
    }

    #[test]
    fn honors_explicit_disable_and_environment_precedence() {
        assert!(profiles_disabled(Some("0"), Some("{\"enabled\":true}")));
        assert!(!profiles_disabled(Some("1"), Some("{\"enabled\":false}")));
        assert!(profiles_disabled(None, Some("{\"enabled\":false}")));
        assert!(!profiles_disabled(None, None));
        assert!(!profiles_disabled(None, Some("custom driver syntax")));
    }

    #[test]
    fn profile_reads_are_bounded_and_distinguish_missing_files() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("tidewm-profile-{}-{nonce}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        let path = root.join(PROFILE);
        assert_eq!(read_file(&path).unwrap(), None);
        std::fs::write(&path, BUNDLED).unwrap();
        assert!(is_bundled_profile(&read_file(&path).unwrap().unwrap()));
        std::fs::write(&path, vec![b' '; LIMIT as usize + 1]).unwrap();
        assert!(read_file(&path).is_err());
        assert!(read_file(&root).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn verifies_profile_content_without_claiming_custom_settings_are_broken() {
        let expected: serde_json::Value = serde_json::from_str(BUNDLED).unwrap();
        assert_eq!(expected["rules"][0]["pattern"]["matches"], "TideWM");
        assert_eq!(
            expected["profiles"][0]["settings"][0]["key"],
            "GLVidHeapReuseRatio"
        );
        assert_eq!(expected["profiles"][0]["settings"][0]["value"], 0);
        assert!(is_bundled_profile(&expected.to_string()));
        assert!(!is_bundled_profile("{}"));
        assert!(!is_bundled_profile("# user-managed profile"));
        let mut changed = expected;
        changed["profiles"][0]["settings"][0]["value"] = serde_json::json!(1);
        assert!(!is_bundled_profile(&changed.to_string()));
    }
}
