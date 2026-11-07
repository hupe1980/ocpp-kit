//! Repository automation for `ocpp-kit`.
//!
//! ```text
//! cargo xtask codegen [--check]   regenerate src/v1_6, src/v2_0_1, src/v2_1 from schemas/
//! cargo xtask schema-report       action / type counts per version
//! cargo xtask schema-diff [--check]  what actually differs between 2.0.1 and 2.1
//! cargo xtask requirements [--check]  extract the Part 2 requirement ids into
//!                                xtask/data/requirements (needs the local OCA text)
//! cargo xtask coverage [--block B] [--use-cases] [--uncited] [--edition E]
//!                                [--profile NAME|all]
//!                                requirement-ID coverage; fails on a cited id that does
//!                                not exist
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
mod footprint;
mod model;
mod naming;
mod profiles;
mod registry;
mod requirements;
mod schema;
mod schema_diff;
mod schemas_check;

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
        "requirements" => requirements::run(&root(), flags.contains("--check")),
        "coverage" => coverage(&args[1..]),
        "doctest-site" => doctest_site(),
        "no-floats" => floats::run(),
        "schemas-check" => schemas_check::run(&root().join("schemas")),
        "ci" => ci::run(flags.contains("--all")),
        "footprint" => footprint::run(&root(), &args[1..]),
        _ => {
            println!("{}", env!("CARGO_PKG_NAME"));
            println!(
                "usage: cargo xtask <codegen [--check] | appendix [--check] | schema-report \
                        | schema-diff [--check] | requirements [--check] \
                        | coverage [--block <B>] [--use-cases] [--uncited] [--edition <E>] \
                          [--profile <NAME>] \
                        | doctest-site | no-floats | schemas-check | footprint [--bench] [--out FILE] | ci [--all]>"
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
            "{} is missing. Extract it first (or point OCPP_APPENDIX at an extracted copy):\n  \
             pdftotext -layout OCPP-2.1_edition2_part2_appendices_v21.pdf {}",
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
    let mut files = appendix::emit(&parsed)?;
    let whitepaper = appendix::whitepaper_path(&root);
    match std::fs::read_to_string(&whitepaper) {
        Ok(text) => {
            let events = appendix::whitepaper_events(&text)?;
            println!("parsed {} OCPP 1.6 security events", events.len());
            files.insert(
                "security_events_1_6.rs",
                appendix::emit_whitepaper_events(&events),
            );
        }
        Err(_) => println!(
            "{} is absent; security_events_1_6.rs left as it is",
            whitepaper.display()
        ),
    }
    for (name, contents) in files {
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

/// Reports which specification requirement IDs (`B02.FR.02`, `N15.FR.01`, …) the source and
/// the test suite cite, and how many of each use case's and each certification profile's
/// requirements that is.
///
/// Every cited id is checked against the committed extraction of Part 2
/// (`xtask/data/requirements/`, written by `cargo xtask requirements`): an id that exists in
/// neither edition it could refer to fails the command, naming the file and line. The check
/// reads only committed files, so it runs in CI without the OCA text.
fn coverage(args: &[String]) -> Result<()> {
    let value = |flag: &str| {
        args.iter()
            .position(|a| a == flag)
            .and_then(|i| args.get(i + 1))
            .map(String::as_str)
    };
    let filter = value("--block");
    let profile = value("--profile");
    let uncited = args.iter().any(|a| a == "--uncited");
    let by_use_case = uncited || args.iter().any(|a| a == "--use-cases");
    let editions: Vec<requirements::Edition> = match value("--edition") {
        None => requirements::Edition::ALL.to_vec(),
        Some("2.0.1") => vec![requirements::Edition::V2_0_1],
        Some("2.1") => vec![requirements::Edition::V2_1],
        Some(other) => bail!("unknown edition {other:?}; use 2.0.1 or 2.1"),
    };

    let root = root();
    let data = requirements::load_all(&root)?;
    let citations = scan_citations(&root)?;
    check_citations_exist(&data, &citations)?;
    let cited =
        requirements::cited_per_edition(&data, citations.iter().map(|(p, c)| (p.as_path(), c)));
    let empty = BTreeSet::new();
    let cited_in = |edition| cited.get(&edition).unwrap_or(&empty);

    if let Some(profile) = profile {
        let part5 = requirements::Edition::V2_0_1;
        if profile == "all" {
            return all_profiles(&data[&part5], cited_in(part5));
        }
        return profile_coverage(profile, &data[&part5], cited_in(part5), uncited);
    }

    if by_use_case {
        for edition in editions {
            println!("{} Part 2\n", edition.label());
            let keep = |id: &str| filter.is_none_or(|f| id.starts_with(f));
            let (n, m) = use_case_report(&data[&edition], cited_in(edition), &keep, uncited);
            println!("\n  {n}/{m} requirement(s) cited\n");
        }
        return Ok(());
    }

    let mut found: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    for (path, citation) in &citations {
        let rel = path.display().to_string();
        let entry = found.entry(&citation.id).or_default();
        if !entry.contains(&rel) {
            entry.push(rel);
        }
    }
    let mut shown = 0;
    for (id, files) in &found {
        if filter.is_some_and(|f| !id.starts_with(f)) {
            continue;
        }
        shown += 1;
        let editions: Vec<&str> = requirements::Edition::ALL
            .iter()
            .filter(|e| data[e].contains(id))
            .map(|e| e.label().trim_start_matches("OCPP "))
            .collect();
        println!("{id:<14} {:<11} {}", editions.join(" "), files.join(", "));
    }
    println!("\n{shown} requirement ID(s) referenced, every one of them defined in Part 2");
    for edition in requirements::Edition::ALL {
        println!(
            "  {}: {}/{} requirement(s) cited",
            edition.label(),
            cited_in(edition).len(),
            data[&edition].len()
        );
    }
    println!("  (`--use-cases` for n/m per use case, `--uncited` to list what is not cited)");
    Ok(())
}

/// Every requirement-id citation in `src/` and `tests/`, with the path relative to the root.
fn scan_citations(root: &Path) -> Result<Vec<(PathBuf, requirements::Citation)>> {
    let mut out = Vec::new();
    for dir in ["src", "tests"] {
        visit(&root.join(dir), &mut |path| {
            if path.extension().is_some_and(|e| e == "rs") {
                let text = std::fs::read_to_string(path)?;
                let rel = path.strip_prefix(root).unwrap_or(path).to_path_buf();
                for citation in requirements::citations(&text) {
                    out.push((rel.clone(), citation));
                }
            }
            Ok(())
        })?;
    }
    out.sort_by(|a, b| (&a.0, a.1.line).cmp(&(&b.0, b.1.line)));
    Ok(out)
}

/// Fails, naming `file:line` and the id, on any citation of an id that the editions the
/// file can refer to do not define.
fn check_citations_exist(
    data: &BTreeMap<requirements::Edition, requirements::Requirements>,
    citations: &[(PathBuf, requirements::Citation)],
) -> Result<()> {
    let unknown = requirements::unknown_citations(data, citations);
    if !unknown.is_empty() {
        bail!(
            "{} citation(s) of requirement ids that do not exist:\n  {}\n\
             (ids from xtask/data/requirements/, extracted by `cargo xtask requirements`)",
            unknown.len(),
            unknown.join("\n  ")
        );
    }
    Ok(())
}

/// Prints n/m per use case for the use cases `keep` admits; returns the totals.
fn use_case_report(
    requirements: &requirements::Requirements,
    cited: &BTreeSet<String>,
    keep: &dyn Fn(&str) -> bool,
    list_uncited: bool,
) -> (usize, usize) {
    let (mut total_cited, mut total) = (0, 0);
    for (key, use_case) in &requirements.use_cases {
        if !keep(&key.id) {
            continue;
        }
        let n = use_case.ids.iter().filter(|id| cited.contains(*id)).count();
        let m = use_case.ids.len();
        total_cited += n;
        total += m;
        let mark = match (n, m) {
            (_, 0) => "-",
            (n, m) if n == m => "x",
            (0, _) => " ",
            _ => "~",
        };
        let title: String = use_case.title.chars().take(52).collect();
        println!("  [{mark}] {:<4} {title:<52} {n:>3}/{m}", key.id);
        if list_uncited && n < m {
            let missing: Vec<&str> = use_case
                .ids
                .iter()
                .filter(|id| !cited.contains(*id))
                .map(String::as_str)
                .collect();
            for chunk in missing.chunks(6) {
                println!("          {}", chunk.join(" "));
            }
        }
    }
    (total_cited, total)
}

/// The requirements of a profile's use cases, and how many of them are cited.
fn profile_requirements(
    slug: &str,
    requirements: &requirements::Requirements,
    cited: &BTreeSet<String>,
) -> (usize, usize) {
    requirements::profile_use_cases(slug)
        .iter()
        .filter_map(|uc| requirements.use_case(uc))
        .flat_map(|uc| &uc.ids)
        .fold((0, 0), |(n, m), id| {
            (n + usize::from(cited.contains(id)), m + 1)
        })
}

fn test_corpus(root: &Path) -> Result<String> {
    let mut corpus = String::new();
    visit(&root.join("tests"), &mut |path| {
        if path.extension().is_some_and(|e| e == "rs") {
            corpus.push_str(&strip_comments(&std::fs::read_to_string(path)?));
        }
        Ok(())
    })?;
    Ok(corpus)
}

/// One line per certification profile — the summary worth quoting.
///
/// "Driven" means a scenario test names the action in code; "cited" means the source or a
/// test names the requirement id. Both are coverage *signals*, not a certification.
/// Certification is a test-lab activity against the OCA test tool; this is the question you
/// can answer in CI on the way there.
fn all_profiles(requirements: &requirements::Requirements, cited: &BTreeSet<String>) -> Result<()> {
    let corpus = test_corpus(&root())?;

    println!("Certification profiles (OCPP 2.0.1 Part 5)\n");
    println!("  {:<40} {:>9} {:>14}", "", "actions", "requirements");
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
        let (n, m) = profile_requirements(profile.slug, requirements, cited);
        println!(
            "  [{mark}] {:<36} {:>9} {:>14}",
            profile.name,
            format!("{covered}/{}", profile.actions.len()),
            format!("{n}/{m}"),
        );
    }
    println!("\n  {total_covered}/{total} action(s) driven by a scenario test");
    println!(
        "  requirements: ids of the profile's Part 2 use cases that the source or a test cites\n  \
         (`--profile <name> --uncited` lists the rest)"
    );
    println!(
        "  (a coverage signal, not a certification: that is a test-lab activity against the\n            OCA test tool.)"
    );
    Ok(())
}

/// Reports how much of one OCPP 2.0.1 certification profile the test suite exercises.
fn profile_coverage(
    name: &str,
    requirements: &requirements::Requirements,
    cited: &BTreeSet<String>,
    list_uncited: bool,
) -> Result<()> {
    let Some(profile) = profiles::find(name) else {
        let names: Vec<&str> = profiles::PROFILES.iter().map(|p| p.slug).collect();
        bail!(
            "unknown certification profile {name:?}; try `all`, or one of: {}",
            names.join(", ")
        );
    };

    let root = root();
    let corpus = test_corpus(&root)?;

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

    println!("\n  requirements of the profile's use cases (OCPP 2.0.1 Part 2):");
    let use_cases = requirements::profile_use_cases(profile.slug);
    let keep = |id: &str| use_cases.contains(&id);
    let (n, m) = use_case_report(requirements, cited, &keep, list_uncited);
    println!("\n  {n}/{m} requirement(s) cited by the source or a test");
    if !list_uncited && n < m {
        println!("  (`--uncited` lists the ids no one cites)");
    }

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
