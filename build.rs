// Bakes a short git hash and a build timestamp into the binary via env vars
// consumed by `get_version()` in src/bluetooth_manager.rs.
//
// Cargo.toml's `version` isn't bumped on every fix (it's been "0.5.5" since well
// before the June 2026 scan fixes) so it can't tell a rebuilt DLL apart from a
// stale one — this is the only cheap way to make `diagnostics_snapshot()` show
// which commit actually produced a given Windows build.
use std::process::Command;

fn main() {
    let git_hash = Command::new("git")
        .args(["rev-parse", "--short=12", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=GDBLE_GIT_HASH={}", git_hash);

    // Re-run only when HEAD actually moves, not on every `cargo build`.
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/index");
}
