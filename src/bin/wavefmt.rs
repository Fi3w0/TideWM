//! wavefmt: canonical formatting for Wave config files.
//!
//! Reads one or more `.wave` files and prints the canonical form, the
//! same formatting the wave_fmt tests pin. The formatting logic is not
//! copied here: this binary includes the exact `src/tide_core/wave_fmt.rs`
//! file the compositor itself uses, so the rules live in exactly one
//! place until the parser crate extraction.
//!
//! Usage:
//!   wavefmt [-w | --write] [-c | --check] [file...]
//!
//!   (no flag)   print the formatted file to stdout
//!   -w/--write  rewrite the file in place
//!   -c/--check  exit 1 if any file is not already formatted (CI use)
//!   -h/--help   this help

#[path = "../tide_core/wave_fmt.rs"]
#[allow(dead_code)]
mod wave_fmt;

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::process::ExitCode;

fn usage() -> String {
    format!(
        "usage: {} [-w|--write] [-c|--check] [file...]\n\
         \n\
           (no flag)   print the formatted file to stdout\n\
           -w/--write  rewrite the file in place\n\
           -c/--check  exit 1 if any file is not already formatted\n",
        std::env::args()
            .next()
            .unwrap_or_else(|| "wavefmt".to_string())
    )
}

fn atomic_write(path: &Path, contents: &str) -> io::Result<()> {
    // Resolve a symlink before choosing the sibling temporary path so `-w`
    // keeps editing the target instead of replacing the link itself.
    let target = fs::canonicalize(path)?;
    let parent = target
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "file has no parent"))?;
    let name = target
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "file has no name"))?
        .to_string_lossy();
    let permissions = fs::metadata(&target)?.permissions();

    for nonce in 0..128u32 {
        let temporary = parent.join(format!(
            ".{name}.wavefmt.{}.{}.tmp",
            std::process::id(),
            nonce
        ));
        let mut file = match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
        {
            Ok(file) => file,
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err),
        };

        let result = (|| {
            file.write_all(contents.as_bytes())?;
            file.set_permissions(permissions.clone())?;
            file.sync_all()?;
            drop(file);
            fs::rename(&temporary, &target)?;
            fs::File::open(parent)?.sync_all()?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        return result;
    }

    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate a sibling temporary file",
    ))
}

fn main() -> ExitCode {
    let mut write = false;
    let mut check = false;
    let mut files = Vec::new();
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "-w" | "--write" => write = true,
            "-c" | "--check" => check = true,
            "-h" | "--help" => {
                eprint!("{}", usage());
                return ExitCode::SUCCESS;
            }
            _ if arg.starts_with('-') => {
                eprintln!("wavefmt: unknown flag `{arg}`\n{}", usage());
                return ExitCode::from(2);
            }
            _ => files.push(arg),
        }
    }
    if files.is_empty() {
        eprintln!("{}", usage());
        return ExitCode::from(2);
    }

    let mut any_unformatted = false;
    let mut failed = false;
    for file in files {
        let source = match fs::read_to_string(&file) {
            Ok(source) => source,
            Err(err) => {
                eprintln!("wavefmt: {file}: {err}");
                failed = true;
                continue;
            }
        };
        let formatted = wave_fmt::format_source(&source);
        if check {
            if formatted != source {
                eprintln!("wavefmt: {file}: would reformat");
                any_unformatted = true;
            }
        } else if write {
            if formatted != source {
                if let Err(err) = atomic_write(Path::new(&file), &formatted) {
                    eprintln!("wavefmt: {file}: {err}");
                    failed = true;
                    continue;
                }
            }
        } else {
            print!("{formatted}");
        }
    }
    if failed || (check && any_unformatted) {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{symlink, MetadataExt, PermissionsExt};
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn atomic_write_preserves_mode_and_symlink() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "tidewm-wavefmt-test-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir(&dir).expect("create test directory");
        let target = dir.join("config.wave");
        let link = dir.join("linked.wave");
        fs::write(&target, "gaps=8\n").expect("write source");
        fs::set_permissions(&target, fs::Permissions::from_mode(0o640)).expect("set mode");
        symlink(&target, &link).expect("create symlink");

        atomic_write(&link, "gaps = 8\n").expect("atomic rewrite");

        assert_eq!(fs::read_to_string(&target).unwrap(), "gaps = 8\n");
        assert!(fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(fs::metadata(&target).unwrap().mode() & 0o777, 0o640);
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 2);
        fs::remove_dir_all(&dir).expect("remove test directory");
    }
}
