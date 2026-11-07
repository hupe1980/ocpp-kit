//! `cargo xtask schemas-check`: the vendored OCA schemas are exactly the files
//! `schemas/SHA256SUMS` records — none altered, none added, none missing.
//!
//! `schemas/NOTICE` says the files are the OCA's own, byte for byte; this makes that a checked
//! claim. Corrections the specification makes to a schema belong in the generator
//! (`SchemaPatch` in `schema.rs`), never in the vendored file.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

pub fn run(schemas: &Path) -> Result<()> {
    let sums = std::fs::read_to_string(schemas.join("SHA256SUMS"))
        .context("reading schemas/SHA256SUMS")?;
    let mut expected = BTreeMap::new();
    for line in sums.lines().filter(|l| !l.trim().is_empty()) {
        let (hash, file) = line
            .split_once("  ")
            .with_context(|| format!("malformed SHA256SUMS line: {line}"))?;
        expected.insert(file.to_string(), hash.to_string());
    }

    let mut problems = Vec::new();
    let mut seen = 0usize;
    for dir in ["v1_6", "v2_0_1", "v2_1"] {
        let mut files: Vec<_> = std::fs::read_dir(schemas.join(dir))?
            .map(|e| e.map(|e| e.path()))
            .collect::<std::result::Result<_, _>>()?;
        files.retain(|p| p.extension().is_some_and(|e| e == "json"));
        files.sort();
        for path in files {
            let name = format!("{dir}/{}", path.file_name().unwrap().to_string_lossy());
            let digest = Sha256::digest(std::fs::read(&path)?);
            let mut hex = String::with_capacity(64);
            for byte in digest {
                let _ = write!(hex, "{byte:02x}");
            }
            seen += 1;
            match expected.remove(&name) {
                None => problems.push(format!("{name}: not in SHA256SUMS")),
                Some(want) if want != hex => problems.push(format!("{name}: modified")),
                Some(_) => {}
            }
        }
    }
    problems.extend(expected.keys().map(|name| format!("{name}: missing")));

    if !problems.is_empty() {
        for p in &problems {
            println!("  {p}");
        }
        bail!(
            "{} vendored schema file(s) differ from schemas/SHA256SUMS",
            problems.len()
        );
    }
    println!("{seen} vendored schema files match schemas/SHA256SUMS");
    Ok(())
}
