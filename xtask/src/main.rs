//! Repository maintenance. Run with `cargo xtask <command>`.

mod embed_clamp;
mod fmt;
mod policy;

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "xtask")]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Enforce the asset policy: the repository holds only small text files
    /// we wrote. Checks tracked and untracked-but-not-ignored files.
    Policy {
        /// Check only what is staged for commit (used by the pre-commit hook).
        #[arg(long)]
        staged: bool,
    },
    /// Point git at .githooks/ so every commit runs the policy check.
    InstallHooks,
    /// Write crates/victory-gpu/src/clamp_bytecode.rs from the output of
    /// the clamp-bytecode workflow (a folder with clamp.dxil and clamp.dxbc).
    EmbedClamp { folder: PathBuf },
    /// Write crates/victory-gpu/src/depth_bytecode.rs from depth.dxil and
    /// depth.dxbc in a folder (depth.hlsl compiled as the clamp-bytecode
    /// workflow compiles clamp.hlsl).
    EmbedDepth { folder: PathBuf },
    /// Format the fork's own Rust files with rustfmt: those that are not in
    /// the last merged upstream commit (the merge base of HEAD and
    /// upstream/main). Files that exist upstream are left as they are, also
    /// where the fork changed them: upstream's layout is kept so that
    /// merges do not conflict on formatting.
    Fmt,
    /// Check that the fork's own Rust files are formatted, without writing.
    /// Files that exist upstream are not checked, also where the fork
    /// changed them. Without an `upstream` remote (a plain clone) nothing
    /// is checked and the command succeeds with a message.
    FmtCheck,
}

fn main() -> Result<()> {
    let root = workspace_root();
    match Cli::parse().command {
        Cmd::Policy { staged } => policy::run(&root, staged),
        Cmd::EmbedClamp { folder } => embed_clamp::run(
            &embed_clamp::CLAMP,
            &root.join("crates/victory-gpu/src"),
            &folder,
        ),
        Cmd::EmbedDepth { folder } => embed_clamp::run(
            &embed_clamp::DEPTH,
            &root.join("crates/victory-gpu/src"),
            &folder,
        ),
        Cmd::Fmt => fmt::run(&root, true),
        Cmd::FmtCheck => fmt::run(&root, false),
        Cmd::InstallHooks => {
            let status = Command::new("git")
                .args(["config", "core.hooksPath", ".githooks"])
                .current_dir(&root)
                .status()
                .context("running git")?;
            anyhow::ensure!(
                status.success(),
                "git config failed (is this a git repository?)"
            );
            println!("hooks installed: commits now run `cargo xtask policy --staged`");
            Ok(())
        }
    }
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask lives in the workspace")
        .into()
}
