//! Embeds build provenance into the binary: the git commit, whether the
//! working tree was dirty at build time, and the build date. Exposed as
//! `TIDEWM_GIT_COMMIT` / `TIDEWM_GIT_DIRTY` / `TIDEWM_BUILD_DATE` env vars
//! for `env!`/`option_env!` reads in both binaries (`tidewm` and
//! `tidectl`), so a bug report can say exactly which tree built the
//! binary -- the "version, build" fields of `tidectl report`.
//!
//! Everything here is best-effort: outside a git checkout (a tarball
//! release), the commit env is simply absent and the binary reports
//! "unknown". The build date honors `SOURCE_DATE_EPOCH` so reproducible
//! builds stay reproducible when they want to be.

use std::process::Command;

fn main() {
    let commit = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .filter(|hash| !hash.is_empty());

    if let Some(commit) = &commit {
        let dirty = Command::new("git")
            .args(["status", "--porcelain"])
            .output()
            .ok()
            .filter(|out| out.status.success())
            .map(|out| !out.stdout.is_empty())
            .unwrap_or(false);
        println!("cargo:rustc-env=TIDEWM_GIT_COMMIT={commit}");
        if dirty {
            println!("cargo:rustc-env=TIDEWM_GIT_DIRTY=1");
        }
    }

    println!("cargo:rustc-env=TIDEWM_BUILD_DATE={}", build_date());
}

/// Today's date as `YYYY-MM-DD` in UTC, or `SOURCE_DATE_EPOCH`'s date when
/// that env is set. Howard Hinnant's civil-from-days algorithm, so no
/// third-party dependency.
fn build_date() -> String {
    let days = std::env::var("SOURCE_DATE_EPOCH")
        .ok()
        .and_then(|epoch| epoch.parse::<i64>().ok())
        .or_else(|| {
            use std::time::{SystemTime, UNIX_EPOCH};
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .ok()
        })
        .unwrap_or(0)
        .div_euclid(86_400);

    // civil_from_days: days since 1970-01-01 -> (year, month, day).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };
    format!("{year:04}-{month:02}-{day:02}")
}
