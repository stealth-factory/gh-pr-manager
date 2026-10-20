//! What the binary can say about itself.
//!
//! The version comes from `Cargo.toml`; the commit and the date come from
//! here, because neither is knowable from source alone. Ported from
//! opscope's `core/build.rs`. The release pipeline runs `gh-pr-manager
//! --version` on every build and refuses one that says `unknown`, so the
//! binary has to print `gh-pr-manager <version> (<commit>, <date>)` using
//! the two variables set below (see docs/releasing.md).

use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    Command::new("git")
        .args(args)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Days since 1970-01-01 to a civil date (Howard Hinnant's algorithm), so
/// SOURCE_DATE_EPOCH can be formatted without shelling out to `date`, which
/// has no `-d` on macOS and does not exist on Windows.
fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    (y, m, d)
}

fn main() {
    // The commit, short. A source tarball has no `.git`, and that is an
    // honest "unknown" rather than a build failure.
    let commit = git(&["rev-parse", "--short", "HEAD"]).unwrap_or_else(|| "unknown".into());

    // Whether that commit had anything uncommitted under it. A sha alone
    // says which commit was checked out, not which source was compiled.
    let dirty = Command::new("git")
        .args(["status", "--porcelain", "--untracked-files=no"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(false);
    let commit = if dirty && commit != "unknown" { format!("{commit}-dirty") } else { commit };

    // SOURCE_DATE_EPOCH first (the reproducible-builds convention), then
    // the commit's own date. Never wall-clock: it would make every build
    // differ from every other for no reader's benefit.
    let date = std::env::var("SOURCE_DATE_EPOCH")
        .ok()
        .and_then(|s| s.parse::<i64>().ok())
        .map(|secs| {
            let (y, m, d) = civil(secs.div_euclid(86_400));
            format!("{y:04}-{m:02}-{d:02}")
        })
        .or_else(|| git(&["log", "-1", "--date=format:%Y-%m-%d", "--format=%cd"]))
        .unwrap_or_else(|| "unknown".into());

    println!("cargo:rustc-env=GH_PR_MANAGER_COMMIT={commit}");
    println!("cargo:rustc-env=GH_PR_MANAGER_BUILD_DATE={date}");

    // Emitting any rerun-if-* replaces cargo's default of "rerun when any
    // file in the package changes", so both halves are watched explicitly:
    // the git files for the commit, the sources for `-dirty`. HEAD alone is
    // not enough - on a branch it holds `ref: refs/heads/<branch>` and does
    // not move when a commit lands - so the ref it names is followed too,
    // and git is asked where these live so a linked worktree works.
    let watch_git = |path: &str| {
        if let Some(file) = git(&["rev-parse", "--git-path", path]) {
            println!("cargo:rerun-if-changed={file}");
        }
    };
    watch_git("HEAD");
    watch_git("packed-refs");
    if let Some(head_ref) = git(&["symbolic-ref", "-q", "HEAD"]) {
        watch_git(&head_ref);
    }
    for path in ["src", "Cargo.toml", "Cargo.lock"] {
        println!("cargo:rerun-if-changed={path}");
    }
    println!("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH");
}
