use std::process::Command;

/// Provenance marker for the Omarchy helper build. `status` reports it so a
/// review can tell which source commit the *running* binary was built from
/// instead of inferring it from file mtimes. Falls back to `unknown` outside a
/// git checkout rather than failing the build.
fn helper_source_commit() -> String {
    // Package builds run from a release tarball with no `.git`, so the
    // packager passes the exact source commit of the patched tree instead. It
    // must be a bare 40-hex commit; anything else falls back to git.
    if let Ok(value) = std::env::var("OMARCHY_HELPER_SOURCE_COMMIT") {
        let value = value.trim().to_string();
        if value.len() == 40 && value.bytes().all(|c| c.is_ascii_hexdigit()) {
            return value.to_ascii_lowercase();
        }
    }
    let output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .filter(|commit| commit.len() == 40 && commit.bytes().all(|c| c.is_ascii_hexdigit()));
    let dirty = Command::new("git")
        .args(["status", "--porcelain", "--untracked-files=no", "--", "."])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .is_some_and(|output| !output.stdout.is_empty());
    match output {
        Some(commit) if dirty => format!("{commit}-dirty"),
        Some(commit) => commit,
        None => "unknown".into(),
    }
}

fn main() {
    #[cfg(feature = "devtools")]
    println!("cargo:warning=⚠️ tauri/devtools enabled");

    println!("cargo:rerun-if-changed=../../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../../.git/index");
    println!("cargo:rerun-if-env-changed=OMARCHY_HELPER_SOURCE_COMMIT");
    println!(
        "cargo:rustc-env=OMARCHY_HELPER_SOURCE_COMMIT={}",
        helper_source_commit()
    );

    tauri_build::build()
}
