//! `cargo xtask no-floats` — the crate's promise that no OCPP quantity is a float.
//!
//! Meter readings, charging limits and prices are decimal quantities. Carrying one in an
//! `f64` loses the meter's resolution claim (`2935.600` and `2935.6` are the same `f64`) and
//! makes the subtraction OCPP defines a session's energy as inexact. `crate::types::Decimal`
//! is exact instead — but a type is only a guarantee while nothing routes around it, and an
//! `f64` that reaches a public signature is a hole a downstream crate cannot see.
//!
//! So this reads every public item in `src/` and fails on a floating-point type in one. It is
//! deliberately a text scan rather than a rustdoc-JSON pass: rustdoc JSON needs nightly and a
//! schema that changes under it, and a lint that only runs sometimes is not a guarantee.
//!
//! Two kinds of exception are allowed, both narrow:
//!
//! * `src/decimal.rs`, which is where the conversions live. They are named `*_f64_lossy` so
//!   the signature says what it costs.
//! * A line carrying an `// allow(floats): why` comment.

use std::path::Path;

use anyhow::{Context, Result, bail};

/// The one module allowed to name a float in public, because it is the boundary itself.
const BOUNDARY: &str = "decimal.rs";

/// The opt-out, which has to state a reason after the colon.
const ESCAPE: &str = "// allow(floats):";

pub fn run() -> Result<()> {
    let root = super::root().join("src");
    let mut files = Vec::new();
    collect(&root, &mut files)?;
    files.sort();

    let mut findings = Vec::new();
    for file in &files {
        if file.file_name().is_some_and(|name| name == BOUNDARY) {
            continue;
        }
        let text =
            std::fs::read_to_string(file).with_context(|| format!("reading {}", file.display()))?;
        let relative = file
            .strip_prefix(super::root())
            .unwrap_or(file)
            .display()
            .to_string();
        for (index, line) in text.lines().enumerate() {
            if !is_public_signature(line) || line.contains(ESCAPE) {
                continue;
            }
            if let Some(float) = float_in(line) {
                findings.push(format!(
                    "{relative}:{}: `{float}` in a public signature\n      {}",
                    index + 1,
                    line.trim()
                ));
            }
        }
    }

    if !findings.is_empty() {
        for finding in &findings {
            println!("  {finding}");
        }
        bail!(
            "{} public signature(s) name a floating-point type; carry the quantity as \
             `types::Decimal`, or annotate the line with `{ESCAPE} <reason>`",
            findings.len()
        );
    }
    println!(
        "{} file(s) checked: no floats in the public API",
        files.len()
    );
    Ok(())
}

/// Whether a line declares something visible outside the crate.
///
/// A field of a `pub struct` is written without `pub` only when it is private, so keying on
/// the `pub` keyword catches both the items and their fields. Doc comments and ordinary
/// comments are skipped, since a float in prose is exactly what this module is documenting.
fn is_public_signature(line: &str) -> bool {
    let trimmed = line.trim_start();
    if trimmed.starts_with("//") {
        return false;
    }
    trimmed.starts_with("pub fn")
        || trimmed.starts_with("pub const fn")
        || trimmed.starts_with("pub async fn")
        || trimmed.starts_with("pub unsafe fn")
        || trimmed.starts_with("pub ")
        // A continuation line of a multi-line signature, which is where the parameters are.
        || (trimmed.starts_with(|c: char| c.is_alphanumeric() || c == '_') && trimmed.contains(": f"))
}

/// The floating-point type named on a line, if any.
///
/// Matched as whole words, so `f64` is a finding and `buf64` or `Inf64` are not.
fn float_in(line: &str) -> Option<&'static str> {
    ["f64", "f32"].into_iter().find(|float| {
        line.match_indices(float).any(|(at, _)| {
            let before = line[..at].chars().next_back();
            let after = line[at + float.len()..].chars().next();
            !before.is_some_and(|c| c.is_alphanumeric() || c == '_')
                && !after.is_some_and(|c| c.is_alphanumeric() || c == '_')
        })
    })
}

fn collect(dir: &Path, out: &mut Vec<std::path::PathBuf>) -> Result<()> {
    for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let path = entry?.path();
        if path.is_dir() {
            collect(&path, out)?;
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
    Ok(())
}
