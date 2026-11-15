//! Hands the version inputs to the crate: `ASCIIDOC_LSP_VERSION` and the
//! output of `git describe`. The choice between them happens in
//! `src/version.rs` so it can be unit-tested.

use std::path::Path;
use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8(out.stdout).ok()?;
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_owned())
}

fn main() {
    println!("cargo:rerun-if-env-changed=ASCIIDOC_LSP_VERSION");
    // Source edits flip `-dirty`; HEAD, ref and index changes move the describe
    // output. Limit: edits to other tracked files (Cargo.toml, asciidoc-core,
    // the grammar submodule, docs) are not watched, because full working-tree
    // dirtiness cannot be watched cheaply. `-dirty` may be stale until the
    // index, HEAD, a ref or the sources change.
    println!("cargo:rerun-if-changed=src");
    // A missing rerun-if-changed path counts as always stale, so only emit
    // paths that exist (`packed-refs` is absent until `git pack-refs` runs).
    for path in ["HEAD", "refs", "packed-refs", "index"] {
        let Some(p) = git(&["rev-parse", "--git-path", path]) else {
            continue;
        };
        if Path::new(&p).exists() {
            println!("cargo:rerun-if-changed={p}");
        }
    }
    if let Some(common) = git(&["rev-parse", "--git-common-dir"]) {
        for name in ["refs", "packed-refs"] {
            let p = format!("{common}/{name}");
            if Path::new(&p).exists() {
                println!("cargo:rerun-if-changed={p}");
            }
        }
    }

    if let Ok(v) = std::env::var("ASCIIDOC_LSP_VERSION") {
        println!("cargo:rustc-env=ASCIIDOC_LSP_BUILD_VERSION={v}");
    }
    if let Some(d) = git(&["describe", "--tags", "--always", "--dirty"]) {
        println!("cargo:rustc-env=ASCIIDOC_LSP_BUILD_GIT_DESCRIBE={d}");
    }
}
