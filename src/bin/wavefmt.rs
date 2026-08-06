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

use std::fs;
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
                if let Err(err) = fs::write(&file, &formatted) {
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
