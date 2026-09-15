//! Repository automation for `ocpp-kit`.
//!
//! ```text
//! cargo xtask codegen [--check]   regenerate src/v1_6, src/v2_0_1, src/v2_1 from schemas/
//! cargo xtask schema-report       action / type counts per version
//! cargo xtask schema-diff [--check]  what actually differs between 2.0.1 and 2.1
//! cargo xtask coverage [--block B] requirement-ID coverage from the test suite
//! cargo xtask doctest-site       compile and run every Rust snippet on the website and in
//!                                the README
//! cargo xtask no-floats          fail if any public signature names f32/f64
//! cargo xtask ci [--all]          run exactly what .github/workflows/ci.yml runs,
//!                                commands and workflow env alike
//! ```

mod appendix;
mod ci;
mod emit;
mod floats;
mod model;
mod naming;
mod profiles;
mod registry;
mod schema;
mod schema_diff;

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
        "schema-diff" => schema_diff::run(&root().join("schemas"), flags.contains("--check")),
        "coverage" => coverage(&args[1..]),
        "doctest-site" => doctest_site(),
        "no-floats" => floats::run(),
        "ci" => ci::run(flags.contains("--all")),
        _ => {
            println!("{}", env!("CARGO_PKG_NAME"));
            println!(
                "usage: cargo xtask <codegen [--check] | appendix [--check] | schema-report \
                        | schema-diff [--check] | coverage [--block <B>] [--profile <NAME>] \
                        | doctest-site | no-floats | ci [--all]>"
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
        let from_cs = model.messages.iter().filter(|m| m.origin.from_cs()).count();
        let from_csms = model
            .messages
            .iter()
            .filter(|m| m.origin.from_csms())
            .count();
        let both = model
            .messages
            .iter()
            .filter(|m| m.origin.from_cs() && m.origin.from_csms())
            .count();
        let sends = model
            .messages
            .iter()
            .filter(|m| m.kind == registry::Kind::Send)
            .count();
        println!("\n=== {} ===", version.label());
        // The columns overlap on purpose and so do not sum to the total: `DataTransfer` is
        // originated by either peer and is counted under both, and a SEND is also counted
        // under the direction it travels. Printing the overlap is cheaper than printing
        // three numbers that visibly do not add up.
        println!("actions: {}", model.messages.len());
        print!("  CS→CSMS {from_cs}, CSMS→CS {from_csms}");
        if both > 0 {
            print!(" (of which {both} either-way)");
        }
        println!(", SEND {sends}");
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
        if profile == "all" {
            return all_profiles();
        }
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
/// One line per certification profile — the summary worth quoting.
fn all_profiles() -> Result<()> {
    let root = root();
    let mut corpus = String::new();
    visit(&root.join("tests"), &mut |path| {
        if path.extension().is_some_and(|e| e == "rs") {
            corpus.push_str(&strip_comments(&std::fs::read_to_string(path)?));
        }
        Ok(())
    })?;

    println!("Certification profiles (OCPP 2.0.1 Part 5)\n");
    let (mut total, mut total_covered) = (0, 0);
    for profile in profiles::PROFILES {
        let covered = profile
            .actions
            .iter()
            .filter(|action| drives(&corpus, action).is_some())
            .count();
        total += profile.actions.len();
        total_covered += covered;
        let mark = if covered == profile.actions.len() {
            "x"
        } else {
            " "
        };
        println!(
            "  [{mark}] {:<30} {covered}/{}",
            profile.name,
            profile.actions.len()
        );
    }
    println!("\n  {total_covered}/{total} action(s) driven by a scenario test");
    println!(
        "  (a coverage signal, not a certification: that is a test-lab activity against the\n            OCA test tool. See concepts/QUALITY.md.)"
    );
    Ok(())
}

fn profile_coverage(name: &str) -> Result<()> {
    let Some(profile) = profiles::find(name) else {
        let names: Vec<&str> = profiles::PROFILES.iter().map(|p| p.slug).collect();
        bail!(
            "unknown certification profile {name:?}; try `all`, or one of: {}",
            names.join(", ")
        );
    };

    let root = root();
    let mut corpus = String::new();
    visit(&root.join("tests"), &mut |path| {
        if path.extension().is_some_and(|e| e == "rs") {
            corpus.push_str(&strip_comments(&std::fs::read_to_string(path)?));
        }
        Ok(())
    })?;

    println!("{} (OCPP 2.0.1 Part 5)\n", profile.name);
    let mut covered = 0;
    for action in profile.actions {
        let how = drives(&corpus, action);
        covered += usize::from(how.is_some());
        match how {
            Some(evidence) => println!("  [x] {action:<38} {evidence}"),
            None => println!("  [ ] {action}"),
        }
    }
    println!(
        "\n  {covered}/{} action(s) driven by a scenario test",
        profile.actions.len()
    );
    println!(
        "  (every action of every version is exercised by the schema conformance suite; this\n              counts the ones a *scenario* test drives, which is what certification asks about.\n              Comments are stripped before matching: an action named in prose is not a test.)"
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

/// How a test drives an action, if it does.
///
/// A plain substring search over the test sources counted an action as covered when a *doc
/// comment* mentioned it, which is not a test — it is a sentence. Coverage now requires one of
/// two things that only appear when code actually handles the action:
///
/// * a typed payload — `<Action>Request` / `<Action>Response`, which a test can only name by
///   constructing or decoding one;
/// * the wire action name as a string literal, which is how a test drives the action through
///   the engine or the framing layer.
///
/// Returns the evidence so the report can be audited rather than believed.
fn drives(corpus: &str, action: &str) -> Option<&'static str> {
    if corpus.contains(&format!("{action}Request")) || corpus.contains(&format!("{action}Response"))
    {
        return Some("typed payload");
    }
    if corpus.contains(&format!("\"{action}\"")) {
        return Some("on the wire");
    }
    None
}

/// Removes `//` and `/* */` comments, leaving string literals intact.
///
/// Coverage is measured over what the tests *do*, and a comment is not a test. String literals
/// are kept because `Input::Received(r#"[2,"c1","ClearCache",{}]"#)` is exactly how a scenario
/// test drives an action.
fn strip_comments(source: &str) -> String {
    let bytes = source.as_bytes();
    let mut out = String::with_capacity(source.len());
    let mut i = 0;
    let (mut in_str, mut in_raw, mut esc) = (false, false, false);
    while i < bytes.len() {
        let b = bytes[i];
        if in_str {
            out.push(b as char);
            if in_raw {
                // A raw string ends at the first quote; `r#"..."#` hashes are handled by the
                // quote-then-hash sequence, which the `#` below simply copies through.
                if b == b'"' {
                    in_str = false;
                    in_raw = false;
                }
            } else if esc {
                esc = false;
            } else if b == b'\\' {
                esc = true;
            } else if b == b'"' {
                in_str = false;
            }
            i += 1;
            continue;
        }
        if b == b'r' && i + 1 < bytes.len() && (bytes[i + 1] == b'"' || bytes[i + 1] == b'#') {
            let mut j = i + 1;
            while j < bytes.len() && bytes[j] == b'#' {
                j += 1;
            }
            if j < bytes.len() && bytes[j] == b'"' {
                out.push_str(&source[i..=j]);
                i = j + 1;
                in_str = true;
                in_raw = true;
                continue;
            }
        }
        if b == b'"' {
            in_str = true;
            out.push('"');
            i += 1;
            continue;
        }
        if b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
            let mut depth = 1;
            i += 2;
            while i < bytes.len() && depth > 0 {
                if bytes[i] == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
                    depth += 1;
                    i += 2;
                } else if bytes[i] == b'*' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
                    depth -= 1;
                    i += 2;
                } else {
                    i += 1;
                }
            }
            continue;
        }
        out.push(b as char);
        i += 1;
    }
    out
}
