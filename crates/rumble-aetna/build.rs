use std::process::Command;

fn main() {
    // Re-run when the commit moves or the index changes. Unstaged
    // working-tree edits won't trigger a rebuild on their own, so the
    // dirty flag here reflects state at the last cargo invocation that
    // actually rebuilt the build script.
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/index");
    println!("cargo:rerun-if-changed=../../.git/refs");
    println!("cargo:rerun-if-changed=build.rs");

    let describe =
        run_git(&["describe", "--always", "--dirty", "--tags", "--long"]).unwrap_or_else(|| "unknown".to_string());
    let hash = run_git(&["rev-parse", "HEAD"]).unwrap_or_else(|| "unknown".to_string());
    let short_hash = run_git(&["rev-parse", "--short", "HEAD"]).unwrap_or_else(|| "unknown".to_string());
    let tag = run_git(&["describe", "--exact-match", "--tags", "HEAD"]).unwrap_or_default();
    let dirty = Command::new("git")
        .args(["status", "--porcelain"])
        .output()
        .ok()
        .map(|o| o.status.success() && !o.stdout.is_empty())
        .unwrap_or(false);

    println!("cargo:rustc-env=RUMBLE_AETNA_GIT_DESCRIBE={describe}");
    println!("cargo:rustc-env=RUMBLE_AETNA_GIT_HASH={hash}");
    println!("cargo:rustc-env=RUMBLE_AETNA_GIT_SHORT_HASH={short_hash}");
    println!("cargo:rustc-env=RUMBLE_AETNA_GIT_TAG={tag}");
    println!(
        "cargo:rustc-env=RUMBLE_AETNA_GIT_DIRTY={}",
        if dirty { "true" } else { "false" }
    );
}

fn run_git(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let s = String::from_utf8(output.stdout).ok()?.trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}
