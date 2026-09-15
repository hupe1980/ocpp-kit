//! What actually changed between two OCPP versions that share schema file names.
//!
//! The crate generates a separate type set per version, and the argument for doing so is a
//! number: how much of the surface 2.0.1 and 2.1 *appear* to share actually differs. That
//! number was quoted in three documents for a long time and was wrong in all three, because
//! nothing derived it — so it is derived here instead.
//!
//! A textual diff says nothing: the 2.1 schemas were re-indented wholesale and every file
//! gained an `$id`. What matters is whether a *validator* built from one version's file would
//! accept the other version's messages, so the comparison drops the three keys that cannot
//! change that — `comment` (the edition string), `$id` (added throughout in 2.1) and
//! `description` (prose) — and compares what is left.

use std::collections::BTreeSet;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value};

use crate::model::VersionId;

/// Keys that cannot change whether a payload validates.
const IGNORED: [&str; 3] = ["comment", "$id", "description"];

/// The pinned result of comparing 2.0.1 with 2.1.
///
/// Asserted by `--check` so that a schema update fails the build rather than silently making
/// the documentation wrong — which is exactly how the old numbers became wrong.
const EXPECTED: Expectation = Expectation {
    shared: 128,
    differing: 95,
    actions: 58,
    added_files: 53,
};

struct Expectation {
    shared: usize,
    differing: usize,
    actions: usize,
    added_files: usize,
}

/// One version pair's comparison.
pub struct Diff {
    pub older: VersionId,
    pub newer: VersionId,
    /// Files present in both, by name.
    pub shared: Vec<String>,
    /// Of those, the ones whose validating structure differs.
    pub differing: Vec<String>,
    /// Files only the newer version has.
    pub added: Vec<String>,
    /// Files only the older version has.
    pub removed: Vec<String>,
}

impl Diff {
    /// The actions touched: a file is `<Action>Request.json` or `<Action>Response.json`, and
    /// an action counts as touched when either half of it changed.
    pub fn actions_touched(&self) -> Vec<String> {
        let mut actions: BTreeSet<String> = BTreeSet::new();
        for file in &self.differing {
            actions.insert(action_of(file));
        }
        actions.into_iter().collect()
    }
}

fn action_of(file: &str) -> String {
    let stem = file.strip_suffix(".json").unwrap_or(file);
    stem.strip_suffix("Request")
        .or_else(|| stem.strip_suffix("Response"))
        .unwrap_or(stem)
        .to_string()
}

/// Strips the keys that carry no validation meaning, recursively.
fn strip(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut out = Map::new();
            for (key, child) in map {
                if IGNORED.contains(&key.as_str()) {
                    continue;
                }
                out.insert(key.clone(), strip(child));
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(strip).collect()),
        other => other.clone(),
    }
}

fn load_dir(dir: &Path) -> Result<BTreeSet<String>> {
    let mut names = BTreeSet::new();
    let entries = std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))?;
    for entry in entries {
        let path = entry?.path();
        if path.extension().is_some_and(|ext| ext == "json") {
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                names.insert(name.to_string());
            }
        }
    }
    Ok(names)
}

fn read_stripped(path: &Path) -> Result<Value> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let value: Value =
        serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    Ok(strip(&value))
}

/// Compares two versions' schema directories.
pub fn compare(older: VersionId, newer: VersionId, schemas: &Path) -> Result<Diff> {
    let older_dir = schemas.join(older.dir());
    let newer_dir = schemas.join(newer.dir());
    let older_files = load_dir(&older_dir)?;
    let newer_files = load_dir(&newer_dir)?;

    let shared: Vec<String> = older_files.intersection(&newer_files).cloned().collect();
    let added: Vec<String> = newer_files.difference(&older_files).cloned().collect();
    let removed: Vec<String> = older_files.difference(&newer_files).cloned().collect();

    let mut differing = Vec::new();
    for name in &shared {
        if read_stripped(&older_dir.join(name))? != read_stripped(&newer_dir.join(name))? {
            differing.push(name.clone());
        }
    }

    Ok(Diff {
        older,
        newer,
        shared,
        differing,
        added,
        removed,
    })
}

/// Prints the comparison, and with `--check` asserts it still matches what the documentation
/// says.
pub fn run(schemas: &Path, check: bool) -> Result<()> {
    let diff = compare(VersionId::V2_0_1, VersionId::V2_1, schemas)?;
    let actions = diff.actions_touched();
    let identical = diff.shared.len() - diff.differing.len();

    println!("\n=== {} → {} ===", diff.older.label(), diff.newer.label());
    println!("schema files shared by name : {}", diff.shared.len());
    println!("  structurally identical     : {identical}");
    println!("  structurally different     : {}", diff.differing.len());
    println!("actions touched              : {}", actions.len());
    println!(
        "files only in {}         : {}",
        diff.newer.label(),
        diff.added.len()
    );
    if !diff.removed.is_empty() {
        println!(
            "files only in {}       : {}",
            diff.older.label(),
            diff.removed.len()
        );
    }
    println!("\nactions whose shared schema changed:");
    for chunk in actions.chunks(4) {
        println!("  {}", chunk.join(", "));
    }
    println!(
        "\n(`description`, `comment` and `$id` are ignored: 2.1 re-indented every file and\n\
         added an `$id` to all of them, so a textual diff reports 128 of 128 and means nothing.)"
    );

    if check {
        let mut wrong = Vec::new();
        if diff.shared.len() != EXPECTED.shared {
            wrong.push(format!(
                "shared files: expected {}, found {}",
                EXPECTED.shared,
                diff.shared.len()
            ));
        }
        if diff.differing.len() != EXPECTED.differing {
            wrong.push(format!(
                "differing files: expected {}, found {}",
                EXPECTED.differing,
                diff.differing.len()
            ));
        }
        if actions.len() != EXPECTED.actions {
            wrong.push(format!(
                "actions touched: expected {}, found {}",
                EXPECTED.actions,
                actions.len()
            ));
        }
        if diff.added.len() != EXPECTED.added_files {
            wrong.push(format!(
                "files added by 2.1: expected {}, found {}",
                EXPECTED.added_files,
                diff.added.len()
            ));
        }
        if !wrong.is_empty() {
            bail!(
                "the schemas changed, so the numbers quoted in the documentation are now wrong:\n  \
                 {}\n\nUpdate EXPECTED in xtask/src/schema_diff.rs and the numbers in \
                 site/content/docs/versions.md, site/content/docs/design.md and README.md.",
                wrong.join("\n  ")
            );
        }
        println!("\nmatches what the documentation says.");
    }

    Ok(())
}
