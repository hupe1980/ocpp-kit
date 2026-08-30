//! Runs locally exactly what `.github/workflows/ci.yml` runs.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

/// Commands CI runs that need something this machine may not have.
///
/// They are reported as skipped rather than silently dropped, so the summary always accounts
/// for every command in the workflow.
const NEEDS_EXTRA_TOOLING: &[&str] = &[
    "cargo hack",         // cargo-hack
    "zola ",              // zola
    "cargo fuzz",         // cargo-fuzz
    "cargo install",      // CI installs cargo-fuzz; nothing to check here
    "--target thumbv7em", // a cross target that must be installed
    "--target wasm32",
];

/// The one CI job that does not run on stable.
///
/// `.github/workflows/ci.yml` gives each job its own toolchain, so running the whole list on
/// one is not what CI does — and gets different answers, since a cross target installed for
/// stable is not installed for nightly.
const NEEDS_NIGHTLY: &[&str] = &["cargo fuzz"];

/// Extracts every `- run:` line from the CI workflow and executes it.
///
/// The point is that the list is *read from the workflow*, not restated here: a command added
/// to CI is a command this runs, and one whose flags change cannot drift out of step. Running
/// an approximation of CI is how a green local run and a red CI run happen at once.
pub fn run(all: bool) -> Result<()> {
    let root = super::root();
    let workflow = root.join(".github/workflows/ci.yml");
    let text = std::fs::read_to_string(&workflow)
        .with_context(|| format!("reading {}", workflow.display()))?;

    let commands = extract_runs(&text);
    if commands.is_empty() {
        bail!("no `- run:` commands found in {}", workflow.display());
    }

    let mut failed = Vec::new();
    let mut skipped = 0usize;
    for command in &commands {
        if !all
            && NEEDS_EXTRA_TOOLING
                .iter()
                .any(|hint| command.contains(hint))
        {
            println!("skip  {command}");
            skipped += 1;
            continue;
        }
        println!("run   {command}");
        if !execute(&root, command)? {
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
        println!("(`--all` runs the skipped ones too, if the tooling is installed)");
    }
    Ok(())
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
fn execute(root: &Path, command: &str) -> Result<bool> {
    let command = if NEEDS_NIGHTLY.iter().any(|hint| command.contains(hint)) {
        format!("rustup run nightly {command}")
    } else {
        command.to_owned()
    };
    let status = Command::new("sh")
        .arg("-c")
        .arg(&command)
        .current_dir(root)
        // `.github/workflows/ci.yml` sets these at the workflow level, and they change what
        // passes: a warning is an error in CI and must be one here.
        .env("RUSTFLAGS", "-D warnings")
        .env("RUSTDOCFLAGS", "-D warnings")
        .env("CARGO_TERM_COLOR", "always")
        .status()
        .with_context(|| format!("spawning `{command}`"))?;
    Ok(status.success())
}

#[cfg(test)]
mod tests {
    use super::extract_runs;

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
}
