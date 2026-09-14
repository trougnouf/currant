// SPDX-License-Identifier: GPL-3.0-or-later

fn main() {
    let git_hash = std::process::Command::new("git")
        .args(["rev-parse", "--short=8", "HEAD"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    println!("cargo:rustc-env=GIT_COMMIT_HASH={git_hash}");
    println!("cargo:rerun-if-changed=.git/HEAD");
}
