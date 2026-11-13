//! `cargo xtask`: the automation entry point of littleguy.
//!
//! Every CI gate runs through this binary. The alias in `.cargo/config.toml`
//! maps `cargo xtask <command>` to `cargo run -p xtask -- <command>`.
//! `docs/xtask.md` lists each command. Do not add a shell script: add a
//! command here.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use smop::prelude::*;

mod container;
mod verus;

/// Repository automation for littleguy.
#[derive(Debug, Parser)]
#[command(name = "xtask", about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

/// The commands of `cargo xtask`.
#[derive(Debug, Subcommand)]
enum Command {
    /// Check the format of every crate.
    Fmt,
    /// Run clippy on every target with warnings denied.
    Clippy,
    /// Run every test of the workspace.
    Test,
    /// Measure line coverage and fail under the bound.
    Coverage(CoverageArgs),
    /// Verify the Verus proofs in `formal/verus`.
    Verus,
    /// Check container and workflow supply-chain invariants.
    ContainerCheck,
    /// Run fmt, clippy, container-check and test, in that order. The pre-push gate.
    Ci,
}

/// Arguments of `cargo xtask coverage`.
#[derive(Debug, Args)]
struct CoverageArgs {
    /// The lowest line coverage that passes, in percent.
    #[arg(long, default_value_t = 85)]
    fail_under: u8,
}

/// The repository root: the parent of this crate's manifest directory.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf)
}

fn cargo(args: &[&str]) -> Result<()> {
    sh::cargo().args(args).dir(repo_root()).echo().run()
}

fn fmt() -> Result<()> {
    cargo(&["fmt", "--all", "--check"])
}

fn clippy() -> Result<()> {
    cargo(&[
        "clippy",
        "--workspace",
        "--all-targets",
        "--locked",
        "-j",
        "2",
        "--",
        "-D",
        "warnings",
    ])
}

fn test() -> Result<()> {
    cargo(&["test", "--workspace", "--locked", "-j", "2"])
}

/// The arguments of the coverage run. The xtask crate is excluded: it drives
/// tools, and its tests would need those tools installed.
fn coverage_args(fail_under: u8) -> Vec<String> {
    [
        "llvm-cov",
        "--workspace",
        "--exclude",
        "xtask",
        "--locked",
        "--fail-under-lines",
    ]
    .iter()
    .map(ToString::to_string)
    .chain(std::iter::once(fail_under.to_string()))
    .collect()
}

fn coverage(args: &CoverageArgs) -> Result<()> {
    path::which("cargo-llvm-cov")
        .context("install cargo-llvm-cov: `cargo install cargo-llvm-cov --locked`")?;
    sh::cargo()
        .args(coverage_args(args.fail_under))
        .dir(repo_root())
        .echo()
        .run()
}

fn run(command: &Command) -> Result<()> {
    match command {
        Command::Fmt => fmt(),
        Command::Clippy => clippy(),
        Command::Test => test(),
        Command::Coverage(args) => coverage(args),
        Command::Verus => verus::check(&repo_root()),
        Command::ContainerCheck => container::check(&repo_root()),
        Command::Ci => {
            fmt()?;
            clippy()?;
            container::check(&repo_root())?;
            test()
        }
    }
}

fn main() -> ExitCode {
    match run(&Cli::parse().command) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error:#}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_root_holds_the_workspace_manifest() {
        assert!(repo_root().join("Cargo.toml").is_file());
        assert!(repo_root().join("xtask").is_dir());
    }

    #[test]
    fn coverage_passes_its_bound_and_skips_xtask() {
        let args = coverage_args(85);
        assert_eq!(args.last().map(String::as_str), Some("85"));
        assert!(args.windows(2).any(|pair| pair == ["--exclude", "xtask"]));
    }

    #[test]
    fn the_cli_parses_every_command() {
        for name in [
            "fmt",
            "clippy",
            "test",
            "verus",
            "container-check",
            "ci",
            "coverage",
        ] {
            assert!(Cli::try_parse_from(["xtask", name]).is_ok(), "{name}");
        }
        let cli = Cli::try_parse_from(["xtask", "coverage", "--fail-under", "90"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Coverage(CoverageArgs { fail_under: 90 })
        ));
    }
}
