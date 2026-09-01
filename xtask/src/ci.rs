//! Runs locally exactly what `.github/workflows/ci.yml` runs.
//!
//! Two things are read out of the workflow rather than restated here, for the same reason:
//! the `- run:` commands, and the workflow-level `env:` block. `RUSTFLAGS: -D warnings`
//! decides what passes, and a copy of it that drifts is how a green local run and a red CI
//! run happen at once — which is the failure this module exists to prevent.
//!
//! A command is skipped only when this machine genuinely cannot run it, decided by probing
//! for the tool rather than by a list of commands assumed to need one. A check quietly
//! skipped because it was on such a list is the same failure wearing a different hat: the
//! feature-powerset run is exactly the one that catches a warning only some feature
//! combination produces, and it is exactly the one a list would skip.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

/// The one CI job that does not run on stable.
///
/// `.github/workflows/ci.yml` gives each job its own toolchain, so running the whole list on
/// one is not what CI does — and gets different answers, since a cross target installed for
/// stable is not installed for nightly.
const NEEDS_NIGHTLY: &[&str] = &["cargo fuzz"];

/// Extracts every `- run:` line from the CI workflow and executes it.
///
/// The point is that the list is *read from the workflow*, not restated here: a command added
/// to CI is a command this runs, and one whose flags change cannot drift out of step.
///
/// `all` runs even the commands this machine looks unable to run, which is occasionally worth
/// it — a probe can be wrong, and a real failure is more informative than a skip.
pub fn run(all: bool) -> Result<()> {
    let root = super::root();
    let workflow = root.join(".github/workflows/ci.yml");
    let text = std::fs::read_to_string(&workflow)
        .with_context(|| format!("reading {}", workflow.display()))?;

    let commands = extract_runs(&text);
    if commands.is_empty() {
        bail!("no `- run:` commands found in {}", workflow.display());
    }
    // Read from the workflow rather than restated here, for the same reason the commands are:
    // `RUSTFLAGS: -D warnings` changes what passes, and a copy of it drifts.
    let environment = extract_env(&text);

    let mut failed = Vec::new();
    let mut skipped = 0usize;
    for command in &commands {
        // Skipping is decided by what this machine actually has, not by a list of commands
        // assumed to need something. A tool that *is* installed runs — the whole point is
        // that a green local run means a green CI run, and a check quietly skipped because it
        // was on a list is exactly how that stops being true.
        if let Some(reason) = unavailable(command) {
            if all {
                println!("run   {command}   (forced; {reason})");
            } else {
                println!("skip  {command}   ({reason})");
                skipped += 1;
                continue;
            }
        } else {
            println!("run   {command}");
        }
        if !execute(&root, command, &environment)? {
            failed.push(command.clone());
        }
    }

    println!(
        "\n{} command(s): {} ok, {} failed, {} skipped",
        commands.len(),
        commands.len() - failed.len() - skipped,
        failed.len(),
        skipped
    );
    if !failed.is_empty() {
        for command in &failed {
            println!("  FAILED  {command}");
        }
        bail!("{} CI command(s) failed", failed.len());
    }
    if skipped > 0 && !all {
        println!("(`--all` runs the skipped ones anyway)");
    }
    Ok(())
}

/// Why this machine cannot run a command, or `None` when it can.
fn unavailable(command: &str) -> Option<String> {
    // An install step: CI performs it, and there is nothing here to check.
    if command.starts_with("cargo install") {
        return Some("an install step, not a check".to_owned());
    }
    // A cross target has to be installed for the toolchain the job uses.
    if let Some(target) = flag_value(command, "--target") {
        if !target_installed(&target) {
            return Some(format!("`rustup target add {target}` first"));
        }
    }
    // Everything else is a program, or a cargo subcommand, that has to be installed.
    let mut words = command.split_whitespace();
    let program = words.next()?;
    let (name, probe) = if program == "cargo" {
        let subcommand = words.next()?;
        // A built-in subcommand is always there; a plugin is a separate binary.
        if !matches!(subcommand, "hack" | "fuzz" | "deny" | "semver-checks") {
            return None;
        }
        (
            format!("cargo-{subcommand}"),
            arguments(&["cargo", subcommand, "--version"]),
        )
    } else {
        (program.to_owned(), arguments(&[program, "--version"]))
    };
    if runs(&probe, needs_nightly(command)) {
        return None;
    }
    Some(format!("`{name}` is not installed"))
}

/// The value after `flag` in a command line, when it is there.
fn flag_value(command: &str, flag: &str) -> Option<String> {
    let mut words = command.split_whitespace();
    while let Some(word) = words.next() {
        if word == flag {
            return words.next().map(ToOwned::to_owned);
        }
        if let Some(value) = word.strip_prefix(&format!("{flag}=")) {
            return Some(value.to_owned());
        }
    }
    None
}

fn arguments(words: &[&str]) -> Vec<String> {
    words.iter().map(|word| (*word).to_owned()).collect()
}

fn target_installed(target: &str) -> bool {
    Command::new("rustup")
        .args(["target", "list", "--installed"])
        .output()
        .is_ok_and(|output| {
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .any(|line| line.trim() == target)
        })
}

/// Whether a probe command exists and answers.
fn runs(probe: &[String], nightly: bool) -> bool {
    let mut command = if nightly {
        let mut command = Command::new("rustup");
        command.args(["run", "nightly"]).args(probe);
        command
    } else {
        let mut command = Command::new(&probe[0]);
        command.args(&probe[1..]);
        command
    };
    command
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn needs_nightly(command: &str) -> bool {
    NEEDS_NIGHTLY.iter().any(|hint| command.contains(hint))
}

/// The workflow's top-level `env:` block.
///
/// Only the workflow level, which is where `ci.yml` puts the settings that change what
/// passes; a job-level block belongs to one job and would be wrong to apply to all of them.
fn extract_env(workflow: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let mut inside = false;
    for line in workflow.lines() {
        if line.trim_end() == "env:" && !line.starts_with(char::is_whitespace) {
            inside = true;
            continue;
        }
        if !inside {
            continue;
        }
        // The block ends at the first line that is not one of its entries.
        let Some(entry) = line.strip_prefix("  ") else {
            if line.trim().is_empty() {
                continue;
            }
            break;
        };
        let entry = entry.split('#').next().unwrap_or(entry);
        if let Some((key, value)) = entry.split_once(':') {
            out.insert(key.trim().to_owned(), value.trim().to_owned());
        }
    }
    out
}

/// Every `- run: …` in a workflow, in order.
fn extract_runs(workflow: &str) -> Vec<String> {
    workflow
        .lines()
        .filter_map(|line| line.trim_start().strip_prefix("- run: "))
        .map(|command| command.trim().to_owned())
        .filter(|command| !command.is_empty() && !command.starts_with('|'))
        .collect()
}

/// Runs one command with the workflow's own environment, on the toolchain its job uses.
fn execute(root: &Path, command: &str, environment: &BTreeMap<String, String>) -> Result<bool> {
    let command = if needs_nightly(command) {
        format!("rustup run nightly {command}")
    } else {
        command.to_owned()
    };
    let status = Command::new("sh")
        .arg("-c")
        .arg(&command)
        .current_dir(root)
        .envs(environment)
        .status()
        .with_context(|| format!("spawning `{command}`"))?;
    Ok(status.success())
}

#[cfg(test)]
mod tests {
    use super::{extract_env, extract_runs, flag_value};

    #[test]
    fn run_steps_are_read_out_of_the_workflow() {
        let workflow = "\
jobs:
  a:
    steps:
      - uses: actions/checkout@v5
      - run: cargo fmt --all --check
      - name: Something
        run: not-a-list-item
      - run: cargo test --all-features
";
        assert_eq!(
            extract_runs(workflow),
            ["cargo fmt --all --check", "cargo test --all-features"]
        );
    }

    /// `RUSTFLAGS: -D warnings` decides what passes. Restating it here rather than reading it
    /// is how a green local run and a red CI run happen at once — the thing this module
    /// exists to prevent.
    #[test]
    fn the_workflow_environment_is_read_rather_than_restated() {
        let workflow = "\
env:
  RUSTFLAGS: -D warnings
  RUST_BACKTRACE: 1    # noisy but harmless

jobs:
  a:
    env:
      NOT_WORKFLOW_LEVEL: 1
";
        let environment = extract_env(workflow);
        assert_eq!(environment["RUSTFLAGS"], "-D warnings");
        assert_eq!(environment["RUST_BACKTRACE"], "1");
        assert!(!environment.contains_key("NOT_WORKFLOW_LEVEL"));
    }

    #[test]
    fn a_cross_target_is_recognised_in_either_spelling() {
        assert_eq!(
            flag_value("cargo check --target thumbv7em-none-eabihf", "--target").as_deref(),
            Some("thumbv7em-none-eabihf")
        );
        assert_eq!(
            flag_value("cargo check --target=wasm32-unknown-unknown", "--target").as_deref(),
            Some("wasm32-unknown-unknown")
        );
        assert_eq!(flag_value("cargo test --all-features", "--target"), None);
    }
}
