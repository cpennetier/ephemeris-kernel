//! Build script: stamp the git SHA into the binary so the run manifest can
//! record which simulator/planner build produced a run. Falls back to
//! "unknown" outside a git checkout (e.g. a packaged source build).

use std::process::Command;

fn main() {
    let sha = Command::new("git")
        .args(["rev-parse", "--short=12", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string());

    println!("cargo:rustc-env=SIMULATOR_GIT_SHA={sha}");
    // Rebuild when HEAD moves so the stamp stays current.
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=.git/HEAD");
}
