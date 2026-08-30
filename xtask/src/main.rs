//! Repository automation for `ocpp-kit`.
//!
//! ```text
//! cargo xtask codegen [--check]   regenerate src/v1_6, src/v2_0_1, src/v2_1 from schemas/
//! cargo xtask schema-report       action / type counts per version
//! cargo xtask coverage [--block B] requirement-ID coverage from the test suite
//! cargo xtask doctest-site       compile and run every Rust snippet on the website and in
//!                                the README
//! cargo xtask ci [--all]          run exactly what .github/workflows/ci.yml runs
//! ```

mod appendix;
mod ci;
mod emit;
mod model;
mod naming;
mod profiles;
mod registry;
mod schema;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use model::VersionId;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(String::as_str).unwrap_or("help");
    let flags: BTreeSet<&str> = args.iter().skip(1).map(String::as_str).collect();

    match cmd {
        "codegen" => codegen(flags.contains("--check")),
        "appendix" => appendix_codegen(flags.contains("--check")),
        "schema-report" => schema_report(),
        "coverage" => coverage(&args[1..]),
        "doctest-site" => doctest_site(),
        "ci" => ci::run(flags.contains("--all")),
        _ => {
            println!("{}", env!("CARGO_PKG_NAME"));
            println!(
                "usage: cargo xtask <codegen [--check] | appendix [--check] | schema-report \
                        | coverage [--block <B>] [--profile <NAME>] | doctest-site \
                        | ci [--all]>"
            );
            Ok(())
        }
    }
}

/// Compiles and runs every Rust snippet on the website, so the pages cannot drift from the API.
///
/// `rustdoc --test` reads a Markdown file directly, which is what makes this a one-liner.
fn doctest_site() -> Result<()> {
    let root = root();
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());

    let built = Command::new(&cargo)
        .current_dir(&root)
        .args(["build", "--features", "full"])
        .status()
        .context("spawning cargo build")?;
    if !built.success() {
        bail!("`cargo build --features full` failed");
    }

    let rlib = root.join("target/debug/libocpp_kit.rlib");
    if !rlib.exists() {
        bail!("{} is missing after a successful build", rlib.display());
    }
    let deps = root.join("target/debug/deps");

    let mut pages: Vec<PathBuf> = Vec::new();
    collect_markdown(&root.join("site/content"), &mut pages)?;
    pages.sort();
    // The README makes the same claims the site does, so it is held to the same standard.
    pages.push(root.join("README.md"));

    let mut failed = Vec::new();
    for page in &pages {
        let mut rustdoc = Command::new("rustdoc");
        rustdoc
            .current_dir(&root)
            .args(["--edition", "2024", "--test"])
            .arg(page)
            .arg("--extern")
            .arg(format!("ocpp_kit={}", rlib.display()))
            .arg("-L")
            .arg(&deps);
        // Pages show `tokio::spawn`, so the snippets need to name it too.
        if let Some(tokio) = newest_rlib(&deps, "tokio")? {
            rustdoc
                .arg("--extern")
                .arg(format!("tokio={}", tokio.display()));
        }
        let status = rustdoc.status().context("spawning rustdoc")?;
        if !status.success() {
            failed.push(
                page.strip_prefix(&root)
                    .unwrap_or(page)
                    .display()
                    .to_string(),
            );
        }
    }

    if !failed.is_empty() {
        bail!("snippets failed on:\n  {}", failed.join("\n  "));
    }
    println!("{} page(s) checked", pages.len());
    Ok(())
}

/// Finds the most recently built `lib<name>-<hash>.rlib` in a dependency directory.
fn newest_rlib(deps: &Path, name: &str) -> Result<Option<PathBuf>> {
    let prefix = format!("lib{name}-");
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    for entry in std::fs::read_dir(deps).with_context(|| format!("reading {}", deps.display()))? {
        let entry = entry?;
        let path = entry.path();
        let matches = path.extension().is_some_and(|e| e == "rlib")
            && path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(&prefix));
        if !matches {
            continue;
        }
        let modified = entry.metadata()?.modified()?;
        if best.as_ref().is_none_or(|(seen, _)| modified > *seen) {
            best = Some((modified, path));
        }
    }
    Ok(best.map(|(_, path)| path))
}

fn collect_markdown(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let path = entry?.path();
        if path.is_dir() {
            collect_markdown(&path, out)?;
        } else if path.extension().is_some_and(|e| e == "md") {
            out.push(path);
        }
    }
    Ok(())
}

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root")
        .to_path_buf()
}

fn codegen(check: bool) -> Result<()> {
    let root = root();
    let schemas = root.join("schemas");
    let mut changed = Vec::new();

    for version in VersionId::ALL {
        let model = schema::load(version, &schemas)
            .with_context(|| format!("loading {} schemas", version.dir()))?;
        let dir = root.join("src").join(version.dir());
        std::fs::create_dir_all(&dir)?;

        let files = [
            ("mod.rs", emit::module(version, &model)),
            ("action.rs", emit::action(version, &model)),
            ("enums.rs", emit::enums(version, &model)),
            ("types.rs", emit::types(version, &model)),
            ("messages.rs", emit::messages(version, &model)),
        ];
        for (name, contents) in files {
            let path = dir.join(name);
            let formatted =
                rustfmt(&contents).with_context(|| format!("formatting {}", path.display()))?;
            let previous = std::fs::read_to_string(&path).unwrap_or_default();
            if previous != formatted {
                changed.push(
                    path.strip_prefix(&root)
                        .unwrap_or(&path)
                        .display()
                        .to_string(),
                );
                if !check {
                    std::fs::write(&path, &formatted)?;
                }
            }
        }
        println!(
            "{:>6}: {} actions, {} enums, {} shared types",
            version.dir(),
            model.messages.len(),
            model.enums.len(),
            model.structs.len()
        );
    }

    if check && !changed.is_empty() {
        bail!(
            "generated code is stale; run `cargo xtask codegen`:\n  {}",
            changed.join("\n  ")
        );
    }
    if !changed.is_empty() {
        println!("updated {} file(s)", changed.len());
    }
    Ok(())
}

/// Pipes generated source through `rustfmt` so the committed output is diff-friendly.
fn rustfmt(source: &str) -> Result<String> {
    use std::io::Write as _;
    use std::process::Stdio;

    let mut child = Command::new("rustfmt")
        .args(["--edition", "2024", "--emit", "stdout", "--quiet"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .context("spawning rustfmt")?;
    child
        .stdin
        .take()
        .expect("piped")
        .write_all(source.as_bytes())?;
    let out = child.wait_with_output()?;
    if !out.status.success() {
        bail!("rustfmt failed");
    }
    Ok(String::from_utf8(out.stdout)?)
}

/// Regenerates `src/standard/` from the OCPP 2.1 Part 2 appendices.
///
/// The appendix is a PDF this repository does not redistribute, so this runs on a developer's
/// machine and the result is committed. CI checks the *schema* codegen, not this one.
fn appendix_codegen(check: bool) -> Result<()> {
    let root = root();
    let source = appendix::source_path(&root);
    if !source.exists() {
        bail!(
            "{} is missing. Extract it first:\n  \
             pdftotext -layout specs/ocpp-2.1/OCPP-2.1_part2_appendices_v20.pdf {}",
            source.display(),
            source.display()
        );
    }
    let parsed = appendix::parse(&std::fs::read_to_string(&source)?)?;
    println!(
        "parsed {} security events, {} components ({} variables), {} standardized variables, {} reason codes, {} units",
        parsed.security_events.len(),
        parsed.components.len(),
        parsed
            .components
            .iter()
            .map(|c| c.variables.len())
            .sum::<usize>(),
        parsed.variable_types.len(),
        parsed.reason_codes.len(),
        parsed.units.len(),
    );

    let dir = root.join("src").join("standard");
    std::fs::create_dir_all(&dir)?;
    let mut changed = Vec::new();
    for (name, contents) in appendix::emit(&parsed)? {
        let path = dir.join(name);
        let formatted = rustfmt(&contents).with_context(|| format!("formatting {name}"))?;
        if std::fs::read_to_string(&path).unwrap_or_default() != formatted {
            changed.push(name);
            if !check {
                std::fs::write(&path, &formatted)?;
            }
        }
    }
    if check && !changed.is_empty() {
        bail!(
            "src/standard is stale; run `cargo xtask appendix`: {}",
            changed.join(", ")
        );
    }
    if !changed.is_empty() {
        println!("updated {}", changed.join(", "));
    }
    Ok(())
}

fn schema_report() -> Result<()> {
    let schemas = root().join("schemas");
    for version in VersionId::ALL {
        let model = schema::load(version, &schemas)?;
        let mut by_block: BTreeMap<&str, usize> = BTreeMap::new();
        for m in &model.messages {
            *by_block.entry(m.block).or_default() += 1;
        }
        println!("\n=== {} ===", version.label());
        println!(
            "actions: {}  (CS→CSMS {}, CSMS→CS {}, SEND {})",
            model.messages.len(),
            model.messages.iter().filter(|m| m.origin.from_cs()).count(),
            model
                .messages
                .iter()
                .filter(|m| m.origin.from_csms())
                .count(),
            model
                .messages
                .iter()
                .filter(|m| m.kind == registry::Kind::Send)
                .count(),
        );
        println!(
            "enums: {}  shared types: {}",
            model.enums.len(),
            model.structs.len()
        );
        for (block, n) in by_block {
            println!("  {:<44} {n:>3}", registry::block_name(version, block));
        }
    }
    Ok(())
}

/// Reports which specification requirement IDs (`B02.FR.02`, `N15.FR.01`, …) are referenced
/// by the source and the test suite, and — with `--profile` — how much of a certification
/// profile the tests reach.
fn coverage(args: &[String]) -> Result<()> {
    let filter = args
        .iter()
        .position(|a| a == "--block")
        .and_then(|i| args.get(i + 1))
        .map(String::as_str);
    let profile = args
        .iter()
        .position(|a| a == "--profile")
        .and_then(|i| args.get(i + 1))
        .map(String::as_str);

    if let Some(profile) = profile {
        return profile_coverage(profile);
    }

    let root = root();
    let mut found: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let pattern = |s: &str| -> Vec<String> {
        let bytes = s.as_bytes();
        let mut ids = Vec::new();
        for (i, _) in s.match_indices(".FR.") {
            // walk left over the block id, right over the digits
            let mut start = i;
            while start > 0 && bytes[start - 1].is_ascii_alphanumeric() {
                start -= 1;
            }
            let mut end = i + 4;
            while end < bytes.len() && bytes[end].is_ascii_alphanumeric() {
                end += 1;
            }
            if end > i + 4 && start < i {
                ids.push(s[start..end].to_string());
            }
        }
        ids
    };

    for dir in ["src", "tests"] {
        visit(&root.join(dir), &mut |path| {
            if path.extension().is_some_and(|e| e == "rs") {
                let text = std::fs::read_to_string(path)?;
                let rel = path
                    .strip_prefix(&root)
                    .unwrap_or(path)
                    .display()
                    .to_string();
                for id in pattern(&text) {
                    let entry = found.entry(id).or_default();
                    if !entry.contains(&rel) {
                        entry.push(rel.clone());
                    }
                }
            }
            Ok(())
        })?;
    }

    let mut shown = 0;
    for (id, files) in &found {
        if let Some(f) = filter {
            if !id.starts_with(f) {
                continue;
            }
        }
        shown += 1;
        println!("{id:<14} {}", files.join(", "));
    }
    println!("\n{shown} requirement ID(s) referenced");
    Ok(())
}

/// Reports how much of one OCPP 2.0.1 certification profile the test suite exercises.
///
/// "Exercised" means the action's name appears in `tests/` — which is a coverage *signal*,
/// not a certification. Certification is a test-lab activity against the OCA test tool; this
/// is the question you can answer in CI on the way there.
fn profile_coverage(name: &str) -> Result<()> {
    let Some(profile) = profiles::find(name) else {
        let names: Vec<&str> = profiles::PROFILES.iter().map(|p| p.slug).collect();
        bail!(
            "unknown certification profile {name:?}; try one of: {}",
            names.join(", ")
        );
    };

    let root = root();
    let mut corpus = String::new();
    visit(&root.join("tests"), &mut |path| {
        if path.extension().is_some_and(|e| e == "rs") {
            corpus.push_str(&std::fs::read_to_string(path)?);
        }
        Ok(())
    })?;

    println!("{} (OCPP 2.0.1 Part 5)\n", profile.name);
    let mut covered = 0;
    for action in profile.actions {
        let seen = corpus.contains(action);
        covered += usize::from(seen);
        println!("  [{}] {action}", if seen { "x" } else { " " });
    }
    println!(
        "\n  {covered}/{} action(s) named in a scenario test",
        profile.actions.len()
    );
    println!(
        "  (every action of every version is exercised by the schema conformance suite; this\n            counts the ones a *scenario* test drives, which is what certification asks about)"
    );

    if !profile.components.is_empty() {
        println!("\n  mandatory controller components (Part 5 §5):");
        for component in profile.components {
            // The standardized catalogue is what a station declares them from.
            let known = std::fs::read_to_string(root.join("src/standard/components.rs"))
                .map(|source| source.contains(&format!("name: {component:?}")))
                .unwrap_or(false);
            println!("    [{}] {component}", if known { "x" } else { " " });
        }
    }
    Ok(())
}

fn visit(dir: &Path, f: &mut impl FnMut(&Path) -> Result<()>) -> Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            visit(&path, f)?;
        } else {
            f(&path)?;
        }
    }
    Ok(())
}
