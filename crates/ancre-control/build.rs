//! Stamp the build's identity into the binary.
//!
//! `gateway_version` is a pin. It goes into the canonical encoding of every
//! event, and its whole job is to answer "which build served this request"
//! two years later, when the answer decides whether a finding applies to code
//! that is still running. A constant reading `0.1.0+unknown` cannot answer
//! that, so it is not evidence — it is a column that looks like evidence.
//!
//! Three sources, in order:
//!
//! 1. `ANCRE_BUILD_SHA`, for a container build. The repository's `.git` is not
//!    in the image build context, and putting it there to compute one string
//!    would be the wrong trade.
//! 2. `git rev-parse`, for a build from a checkout.
//! 3. The literal `unknown`, when neither is available.
//!
//! The third is deliberately not an error. A build that refuses to compile
//! outside a git checkout is a build an auditor cannot reproduce from a source
//! tarball — and `unknown` is a true statement, which is the standard the rest
//! of this system holds itself to.
//!
//! A dirty working tree is stamped `<sha>.dirty`. A pin naming a commit whose
//! code is not the code that ran is worse than one naming nothing.
//!
//! Byte-identical to `ancre-gateway/build.rs`, and duplicated rather than
//! shared. A build script cannot be pulled from a path dependency without
//! making the trust root — the crate every hash descends from — depend on a
//! crate that shells out to `git` at compile time. Fifty lines twice is the
//! cheaper of the two.

fn main() {
    ensure_ui_dist();
    println!("cargo:rerun-if-env-changed=ANCRE_BUILD_SHA");
    for path in ["../../.git/HEAD", "../../.git/index"] {
        if std::path::Path::new(path).exists() {
            println!("cargo:rerun-if-changed={path}");
        }
    }

    let sha = std::env::var("ANCRE_BUILD_SHA")
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty() && s != "unknown")
        .or_else(git_sha)
        .unwrap_or_else(|| "unknown".to_owned());

    println!("cargo:rustc-env=ANCRE_BUILD_SHA={sha}");
}

fn git_sha() -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["rev-parse", "--short=12", "HEAD"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let sha = String::from_utf8(out.stdout).ok()?.trim().to_owned();
    if sha.is_empty() {
        return None;
    }

    // Tracked files only. Untracked files do not change what was compiled, and
    // counting them would mark every developer's build dirty forever, which
    // would make the marker mean nothing.
    let dirty = std::process::Command::new("git")
        .args(["status", "--porcelain", "--untracked-files=no"])
        .output()
        .is_ok_and(|o| !o.stdout.is_empty());

    Some(if dirty { format!("{sha}.dirty") } else { sha })
}

/// Guarantee `ui/dist` exists before `rust_embed` reads it.
///
/// `#[derive(Embed)]` resolves its folder at compile time and fails the build
/// if it is missing, and the directory is missing in three ordinary
/// situations: a fresh clone (it holds only build output, so it is gitignored),
/// a `cargo build` by someone who has never run `npm`, and — the one that
/// actually bit — immediately after `vite build`, whose `emptyOutDir` deletes
/// any placeholder committed to keep the directory alive.
///
/// A committed `.gitkeep` cannot survive that third case, so the invariant is
/// established here instead of being asked of git. An empty directory embeds
/// nothing and `ui.rs` answers with build instructions, which is the honest
/// outcome for a binary built without a dashboard.
fn ensure_ui_dist() {
    let dist = std::path::Path::new("ui/dist");
    if !dist.exists() {
        // A failure here is not fatal: `rust_embed` will produce its own,
        // clearer error, and a build that dies inside a directory-creation
        // helper is harder to diagnose than one that dies where the folder is
        // actually read.
        let _ = std::fs::create_dir_all(dist);
    }
}
