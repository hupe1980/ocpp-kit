//! Extracts the requirement ids of OCPP 2.0.1 and 2.1 Part 2 into committed data, and finds
//! the requirement ids the source and the tests cite.
//!
//! Part 2 states every normative rule as a row of a "<use case> - Requirements" table, keyed by
//! an id such as `E01.FR.08`. `cargo xtask coverage` used to count which ids the code cites
//! without knowing which ids exist, so a typo counted as coverage. This module makes the list of
//! ids mechanical:
//!
//! ```text
//! pdftotext -layout OCPP-2.0.1_part2_specification_edition2.pdf part2-2.0.1.txt
//! OCPP_PART2_2_0_1=part2-2.0.1.txt cargo xtask requirements
//! ```
//!
//! Without the environment variables (`OCPP_PART2_2_0_1`, `OCPP_PART2_2_1`) the renderings are
//! read from the maintainers' local, gitignored OCA corpus under `concepts/reference/`.
//!
//! Only identifiers are committed — requirement ids, use-case ids and use-case titles, the
//! same kind of interoperability surface the vendored schemas carry. The requirement text is
//! OCA-licensed and is **not** copied; read it in the specification.
//!
//! The committed files under `xtask/data/requirements/` are what `cargo xtask coverage` reads,
//! so the coverage check runs in CI without the PDFs.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

/// A Part 2 edition whose requirement ids are extracted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Edition {
    V2_0_1,
    V2_1,
}

impl Edition {
    pub const ALL: [Self; 2] = [Self::V2_0_1, Self::V2_1];

    pub fn label(self) -> &'static str {
        match self {
            Self::V2_0_1 => "OCPP 2.0.1",
            Self::V2_1 => "OCPP 2.1",
        }
    }

    /// The edition the text rendering is of, as `SPEC_EDITIONS.md` records it.
    fn edition_note(self) -> &'static str {
        match self {
            Self::V2_0_1 => "OCPP 2.0.1 Edition 2 Part 2 - Specification (FINAL, 2022-12-15)",
            Self::V2_1 => "OCPP 2.1 Edition 1 Part 2 - Specification (2025-01-23)",
        }
    }

    /// The directory name the generated code uses for the version (`src/v2_0_1`).
    pub fn dir(self) -> &'static str {
        match self {
            Self::V2_0_1 => "v2_0_1",
            Self::V2_1 => "v2_1",
        }
    }

    fn env_var(self) -> &'static str {
        match self {
            Self::V2_0_1 => "OCPP_PART2_2_0_1",
            Self::V2_1 => "OCPP_PART2_2_1",
        }
    }

    /// Where the Part 2 text rendering is expected: the environment variable if set, else the
    /// local OCA corpus.
    pub fn source_path(self, root: &Path) -> PathBuf {
        std::env::var_os(self.env_var()).map_or_else(
            || {
                let reference = root.join("concepts").join("reference");
                match self {
                    Self::V2_0_1 => reference
                        .join("ocpp-2.0.1")
                        .join("OCPP-2.0.1_part2_specification_edition2.txt"),
                    Self::V2_1 => reference
                        .join("ocpp-2.1")
                        .join("OCPP-2.1_edition1_part2_specification.txt"),
                }
            },
            PathBuf::from,
        )
    }

    fn pdf_name(self) -> &'static str {
        match self {
            Self::V2_0_1 => "OCPP-2.0.1_part2_specification_edition2.pdf",
            Self::V2_1 => "OCPP-2.1_edition1_part2_specification.pdf",
        }
    }

    /// The committed extraction.
    pub fn data_path(self, root: &Path) -> PathBuf {
        data_dir(root).join(format!("{}.txt", self.dir()))
    }

    /// Ids every correct extraction of this edition contains, from both ends of the document
    /// and from tables that span pages. A layout change that loses them fails loudly.
    fn known_ids(self) -> &'static [&'static str] {
        const COMMON: &[&str] = &[
            "A00.FR.207",
            "B01.FR.10",
            "B02.FR.09",
            "E01.FR.08",
            "K08.FR.01",
            "K15.FR.07",
            "P02.FR.01",
        ];
        // 2.1-only ids; `validate` checks the common ones for 2.1 as well.
        const V2_1: &[&str] = &["C25.FR.21", "C25.FR.27", "FR.01", "V2X.01"];
        match self {
            Self::V2_0_1 => COMMON,
            Self::V2_1 => V2_1,
        }
    }
}

pub fn data_dir(root: &Path) -> PathBuf {
    root.join("xtask").join("data").join("requirements")
}

/// One use case (or, for the cross-cutting tables, a group named after its first table).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct UseCase {
    pub title: String,
    /// Requirement ids, in specification order (`B01.FR.2` before `B01.FR.10`).
    pub ids: Vec<String>,
}

/// Everything extracted from one Part 2 rendering, keyed by use case.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Requirements {
    pub use_cases: BTreeMap<UseCaseKey, UseCase>,
}

/// Orders use cases as the specification does: functional blocks A–S, then the cross-cutting
/// groups (`FR`, `SC`, `V2X`).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct UseCaseKey {
    generic: bool,
    pub id: String,
}

impl UseCaseKey {
    pub fn new(id: &str) -> Self {
        Self {
            generic: !is_use_case_id(id),
            id: id.to_owned(),
        }
    }

    /// The functional block a use case belongs to (`B` for `B01`), or the group itself for a
    /// cross-cutting group.
    pub fn block(&self) -> &str {
        if self.generic {
            &self.id
        } else {
            &self.id[..1]
        }
    }
}

/// Diagnostics a maintainer should read, but that do not make the extraction wrong.
#[derive(Debug, Default)]
pub struct Findings {
    /// Ids the specification itself states twice, with the line numbers.
    pub duplicates: Vec<(String, Vec<usize>)>,
    /// Ids found in the requirements table of a different use case: (id, table use case, line).
    pub misplaced: Vec<(String, String, usize)>,
}

impl Requirements {
    pub fn len(&self) -> usize {
        self.use_cases.values().map(|u| u.ids.len()).sum()
    }

    pub fn contains(&self, id: &str) -> bool {
        use_case_of(id)
            .and_then(|uc| self.use_cases.get(&UseCaseKey::new(uc)))
            .is_some_and(|u| u.ids.iter().any(|i| i == id))
    }

    pub fn use_case(&self, id: &str) -> Option<&UseCase> {
        self.use_cases.get(&UseCaseKey::new(id))
    }

    /// Requirement count per functional block.
    pub fn per_block(&self) -> BTreeMap<String, (usize, usize)> {
        let mut out: BTreeMap<String, (usize, usize)> = BTreeMap::new();
        for (key, uc) in &self.use_cases {
            let entry = out.entry(key.block().to_owned()).or_default();
            entry.0 += 1;
            entry.1 += uc.ids.len();
        }
        out
    }
}

// ---------------------------------------------------------------------------
// Id shapes
// ---------------------------------------------------------------------------

/// `B01`: one capital letter and two digits.
fn is_use_case_id(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 3 && b[0].is_ascii_uppercase() && b[1].is_ascii_digit() && b[2].is_ascii_digit()
}

/// The prefixes of the cross-cutting requirement tables, which have no use case: the generic
/// requirements (`FR.01`, chapter 3), the smart-charging rules for multiple actors (`SC.01`,
/// K) and the generic V2X rules (`V2X.01`, Q, 2.1 only).
const GENERIC_PREFIXES: &[&str] = &["FR", "SC", "V2X"];

/// The use case (or group) a requirement id belongs to, if it is shaped like one:
/// `B01.FR.10` → `B01`, `FR.01` → `FR`.
pub fn use_case_of(id: &str) -> Option<&str> {
    let (head, number) = id.rsplit_once('.')?;
    if number.is_empty() || !number.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if let Some(uc) = head.strip_suffix(".FR") {
        return is_use_case_id(uc).then_some(uc);
    }
    GENERIC_PREFIXES.contains(&head).then_some(head)
}

/// Sort key that orders `B01.FR.2` before `B01.FR.10`.
fn id_order(id: &str) -> (String, u32) {
    let (head, number) = id.rsplit_once('.').unwrap_or((id, "0"));
    (head.to_owned(), number.parse().unwrap_or(u32::MAX))
}

// ---------------------------------------------------------------------------
// Extraction from `pdftotext -layout` output
// ---------------------------------------------------------------------------

/// The columns of the requirements table being read, learned from its header row.
#[derive(Debug, Clone, Copy)]
struct Columns {
    /// Where the `ID` heading starts. The heading is centred over the id column, so a row id
    /// starts at or before it.
    id: usize,
    /// Where the `Precondition` heading starts; nothing at or beyond it is a row id.
    precondition: usize,
}

/// Recognises the header row `ID   Precondition   Requirement definition   Note`, which
/// `pdftotext` repeats at the top of every page a table continues on.
fn header_columns(line: &str) -> Option<Columns> {
    let id = line.find("ID")?;
    if !line[..id].trim().is_empty() {
        return None;
    }
    let rest = &line[id + 2..];
    let gap = rest.len() - rest.trim_start().len();
    if gap == 0 || !rest.trim_start().starts_with("Precondition") {
        return None;
    }
    Some(Columns {
        id,
        precondition: id + 2 + gap,
    })
}

/// If the line opens a table row, the id in its first column.
fn row_id(line: &str, columns: Columns) -> Option<&str> {
    let start = line.len() - line.trim_start().len();
    // A row id sits in the first column. Requirement ids also appear in the Precondition
    // column ("K15.FR.03 or / K15.FR.05") and in notes; those start further right.
    if start > columns.id + 1 || start >= columns.precondition {
        return None;
    }
    let token = line[start..].split_whitespace().next()?;
    use_case_of(token)?;
    Some(token)
}

/// `B01 - Cold Boot Charging Station`, the head of a use case title.
fn split_title(s: &str) -> Option<(&str, &str)> {
    let s = s.trim();
    let (id, title) = s.split_once(" - ")?;
    is_use_case_id(id).then_some((id, title.trim()))
}

/// A table-of-contents entry: `B01 - Cold Boot Charging Station . . . . . 44`.
fn toc_entry(line: &str) -> Option<(&str, String)> {
    let (id, rest) = split_title(line)?;
    let rest = rest
        .trim_end_matches(|c: char| c.is_ascii_digit())
        .trim_end();
    if !rest.ends_with(". .") && !rest.ends_with("..") {
        return None;
    }
    let title = rest.trim_end_matches(['.', ' ']);
    (!title.is_empty()).then(|| (id, title.to_owned()))
}

/// A table caption (`Table 11. Generic requirements`) or a numbered section heading
/// (`2.5. Generic smart charging rules for V2X`) at the left margin.
fn caption(line: &str) -> Option<&str> {
    if line.starts_with(' ') {
        return None;
    }
    if let Some(rest) = line.strip_prefix("Table ") {
        let (number, title) = rest.split_once(". ")?;
        return number
            .bytes()
            .all(|b| b.is_ascii_digit())
            .then_some(title.trim());
    }
    let (number, title) = line.split_once(". ")?;
    let numbered = !number.is_empty()
        && number
            .split('.')
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()));
    numbered.then_some(title.trim())
}

/// The use case whose requirements table a heading or caption opens:
/// `B01 - Cold Boot Charging Station - Requirements` or `Table 36. B01 - Requirements`.
fn requirements_heading(line: &str) -> Option<&str> {
    let line = line.trim_end();
    if !line.ends_with("Requirements") || line.starts_with(' ') {
        return None;
    }
    let line = caption(line).unwrap_or(line);
    let id = line.split_whitespace().next()?;
    (is_use_case_id(id) && line[3..].starts_with(" - ")).then_some(id)
}

/// Extracts every requirement id of a Part 2 rendering, grouped by use case.
pub fn parse(text: &str) -> (Requirements, Findings) {
    let mut titles: BTreeMap<String, String> = BTreeMap::new();
    let mut headings: BTreeMap<String, String> = BTreeMap::new();
    let mut first_caption: BTreeMap<String, String> = BTreeMap::new();
    let mut seen: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    let mut findings = Findings::default();

    let mut columns: Option<Columns> = None;
    let mut table_use_case: Option<String> = None;
    let mut last_caption = String::new();

    for (index, raw) in text.split('\n').enumerate() {
        let number = index + 1;
        // `pdftotext` starts each page with a form feed glued to its first line.
        let line = raw.trim_start_matches('\u{c}').trim_end();

        if let Some((id, title)) = toc_entry(line) {
            titles.entry(id.to_owned()).or_insert(title);
            continue;
        }
        if let Some(id) = requirements_heading(line) {
            table_use_case = Some(id.to_owned());
        } else if let Some((id, title)) = split_title(line).filter(|_| !line.starts_with(' ')) {
            // The body heading of a use case, used when the contents omit one.
            headings
                .entry(id.to_owned())
                .or_insert_with(|| title.to_owned());
        }
        if let Some(title) = caption(line) {
            last_caption = title.to_owned();
            if !title.ends_with("Requirements") && !title.ends_with("requirements") {
                // A caption that is not a requirements table ends the use-case context.
                if split_title(title).is_none() {
                    table_use_case = None;
                }
            }
        }
        if let Some(found) = header_columns(line) {
            columns = Some(found);
            continue;
        }
        let Some(columns) = columns else { continue };
        let Some(id) = row_id(line, columns) else {
            continue;
        };
        let use_case = use_case_of(id).expect("row_id checked the shape");
        if let Some(table) = &table_use_case {
            if is_use_case_id(use_case) && use_case != table {
                findings
                    .misplaced
                    .push((id.to_owned(), table.clone(), number));
            }
        }
        if !is_use_case_id(use_case) || use_case == "A00" {
            first_caption
                .entry(use_case.to_owned())
                .or_insert_with(|| last_caption.clone());
        }
        seen.entry(id.to_owned()).or_default().push(number);
    }

    let mut requirements = Requirements::default();
    for (id, lines) in seen {
        if lines.len() > 1 {
            findings.duplicates.push((id.clone(), lines));
        }
        let use_case = use_case_of(&id).expect("only ids are recorded").to_owned();
        requirements
            .use_cases
            .entry(UseCaseKey::new(&use_case))
            .or_default()
            .ids
            .push(id);
    }
    // Use cases the contents list but that state no requirements of their own (C03 in 2.1
    // was moved to C24; K23's requirements are K11's and K12's) are kept, so that 0/0 is
    // reported rather than the use case silently missing.
    for id in titles.keys() {
        requirements
            .use_cases
            .entry(UseCaseKey::new(id))
            .or_default();
    }
    for (key, use_case) in &mut requirements.use_cases {
        use_case.ids.sort_by_key(|id| id_order(id));
        use_case.title = titles
            .get(&key.id)
            .or_else(|| headings.get(&key.id))
            .or_else(|| first_caption.get(&key.id))
            .cloned()
            .unwrap_or_default();
    }
    (requirements, findings)
}

/// Fails when the extraction is implausible: too few ids, a known id missing, a use case
/// without a title.
pub fn validate(edition: Edition, requirements: &Requirements) -> Result<()> {
    if requirements.len() < 1000 {
        bail!(
            "only {} requirement ids extracted from {}; the table layout changed",
            requirements.len(),
            edition.label()
        );
    }
    let mut known: Vec<&str> = edition.known_ids().to_vec();
    if edition == Edition::V2_1 {
        known.extend(Edition::V2_0_1.known_ids());
    }
    let missing: Vec<&str> = known
        .into_iter()
        .filter(|id| !requirements.contains(id))
        .collect();
    if !missing.is_empty() {
        bail!(
            "{}: known requirement ids were not extracted: {}",
            edition.label(),
            missing.join(", ")
        );
    }
    let untitled: Vec<&str> = requirements
        .use_cases
        .iter()
        .filter(|(_, uc)| uc.title.is_empty())
        .map(|(key, _)| key.id.as_str())
        .collect();
    if !untitled.is_empty() {
        bail!(
            "{}: use cases without a title: {}",
            edition.label(),
            untitled.join(", ")
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// The committed format
// ---------------------------------------------------------------------------

/// Renders the committed file: a `usecase` line per use case followed by one `req` line per
/// requirement id, tab-separated.
pub fn render(edition: Edition, requirements: &Requirements) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "# Requirement ids of {}.\n\
         # Generated by `cargo xtask requirements` from a `pdftotext -layout` rendering; do not edit.\n\
         # Identifiers only: the requirement text is OCA-licensed and is not reproduced here.\n\
         # usecase<TAB>id<TAB>title   then   req<TAB>requirement id<TAB>use case\n\
         # {} use cases, {} requirement ids.",
        edition.edition_note(),
        requirements.use_cases.len(),
        requirements.len(),
    );
    for (key, use_case) in &requirements.use_cases {
        let _ = writeln!(out, "usecase\t{}\t{}", key.id, use_case.title);
        for id in &use_case.ids {
            let _ = writeln!(out, "req\t{id}\t{}", key.id);
        }
    }
    out
}

/// Reads a committed file back.
pub fn load(text: &str) -> Result<Requirements> {
    let mut requirements = Requirements::default();
    for (index, line) in text.lines().enumerate() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let fields: Vec<&str> = line.split('\t').collect();
        match fields.as_slice() {
            ["usecase", id, title] => {
                requirements.use_cases.insert(
                    UseCaseKey::new(id),
                    UseCase {
                        title: (*title).to_owned(),
                        ids: Vec::new(),
                    },
                );
            }
            ["req", id, use_case] => {
                if use_case_of(id) != Some(*use_case) {
                    bail!("line {}: {id} does not belong to {use_case}", index + 1);
                }
                requirements
                    .use_cases
                    .get_mut(&UseCaseKey::new(use_case))
                    .with_context(|| format!("line {}: {use_case} has no usecase line", index + 1))?
                    .ids
                    .push((*id).to_owned());
            }
            _ => bail!("line {}: unrecognised: {line:?}", index + 1),
        }
    }
    Ok(requirements)
}

/// Loads the committed extraction of every edition.
pub fn load_all(root: &Path) -> Result<BTreeMap<Edition, Requirements>> {
    let mut out = BTreeMap::new();
    for edition in Edition::ALL {
        let path = edition.data_path(root);
        let text = std::fs::read_to_string(&path).with_context(|| {
            format!(
                "reading {}; run `cargo xtask requirements` on a machine with the Part 2 text",
                path.display()
            )
        })?;
        out.insert(
            edition,
            load(&text).with_context(|| format!("parsing {}", path.display()))?,
        );
    }
    Ok(out)
}

/// `cargo xtask requirements [--check]`.
///
/// Like `cargo xtask appendix`, this needs the OCA text, which only a maintainer has; with
/// `--check` an edition whose text is absent is skipped rather than failed, so the command is
/// safe to run anywhere.
pub fn run(root: &Path, check: bool) -> Result<()> {
    let mut stale = Vec::new();
    for edition in Edition::ALL {
        let source = edition.source_path(root);
        if !source.exists() {
            if check {
                println!(
                    "{}: {} is absent, nothing to check against",
                    edition.label(),
                    source.display()
                );
                continue;
            }
            bail!(
                "{} is missing. Extract it first (or point {} at an extracted copy):\n  \
                 pdftotext -layout {} {}",
                source.display(),
                edition.env_var(),
                edition.pdf_name(),
                source.display()
            );
        }
        let text = std::fs::read_to_string(&source)
            .with_context(|| format!("reading {}", source.display()))?;
        let (requirements, findings) = parse(&text);
        validate(edition, &requirements)?;
        report(edition, &requirements, &findings);

        let path = edition.data_path(root);
        let rendered = render(edition, &requirements);
        if std::fs::read_to_string(&path).unwrap_or_default() != rendered {
            stale.push(
                path.strip_prefix(root)
                    .unwrap_or(&path)
                    .display()
                    .to_string(),
            );
            if !check {
                std::fs::create_dir_all(data_dir(root))?;
                std::fs::write(&path, rendered)?;
            }
        }
    }
    if check && !stale.is_empty() {
        bail!(
            "committed requirement ids are stale; run `cargo xtask requirements`:\n  {}",
            stale.join("\n  ")
        );
    }
    if !stale.is_empty() {
        println!("updated {}", stale.join(", "));
    }
    Ok(())
}

fn report(edition: Edition, requirements: &Requirements, findings: &Findings) {
    println!(
        "{}: {} requirement ids in {} use cases",
        edition.label(),
        requirements.len(),
        requirements.use_cases.len()
    );
    let blocks: Vec<String> = requirements
        .per_block()
        .into_iter()
        .map(|(block, (_, ids))| format!("{block} {ids}"))
        .collect();
    println!("  per block: {}", blocks.join(", "));
    let empty: Vec<&str> = requirements
        .use_cases
        .iter()
        .filter(|(_, uc)| uc.ids.is_empty())
        .map(|(key, _)| key.id.as_str())
        .collect();
    if !empty.is_empty() {
        println!("  use cases without requirements: {}", empty.join(", "));
    }
    for (id, lines) in &findings.duplicates {
        let lines: Vec<String> = lines.iter().map(ToString::to_string).collect();
        println!(
            "  note: the specification states {id} more than once (lines {})",
            lines.join(", ")
        );
    }
    for (id, table, line) in &findings.misplaced {
        println!("  note: {id} sits in the {table} requirements table (line {line})");
    }
}

// ---------------------------------------------------------------------------
// Citations in the source and the tests
// ---------------------------------------------------------------------------

/// One requirement id cited in a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Citation {
    pub id: String,
    /// 1-based.
    pub line: usize,
}

/// Finds the requirement ids a source file cites.
///
/// Three spellings are recognised:
///
/// * `E01.FR.08` — prose and doc comments;
/// * `e01_fr_08` — a test name, which cannot contain a dot;
/// * `B02.FR.04 / FR.07 / 08` — shorthand for further ids of the same use case.
///
/// A bare `FR.07` with no use case in front of it is not taken as a citation: in this code
/// base it names Part 4's rules as often as Part 2's generic requirements.
pub fn citations(source: &str) -> Vec<Citation> {
    let mut out = Vec::new();
    for (index, line) in source.lines().enumerate() {
        let bytes = line.as_bytes();
        let mut i = 0;
        while i + 6 < bytes.len() + 1 {
            let Some((use_case, number, end)) = id_at(bytes, i) else {
                i += 1;
                continue;
            };
            out.push(Citation {
                id: format!("{use_case}.FR.{number}"),
                line: index + 1,
            });
            i = end;
            // `/ FR.07` and `/07` continue the same use case.
            while let Some((number, end)) = continuation(bytes, i) {
                out.push(Citation {
                    id: format!("{use_case}.FR.{number}"),
                    line: index + 1,
                });
                i = end;
            }
        }
    }
    out
}

/// A full id starting at `i`: `X00.FR.<digits>` or `x00_fr_<digits>`, not glued to a longer
/// word on the left nor to more digits on the right.
fn id_at(bytes: &[u8], i: usize) -> Option<(String, String, usize)> {
    if i > 0 && bytes[i - 1].is_ascii_alphanumeric() {
        return None;
    }
    let head = bytes.get(i..i + 7)?;
    let (upper, sep) = if head[0].is_ascii_uppercase() {
        (true, b'.')
    } else if head[0].is_ascii_lowercase() {
        (false, b'_')
    } else {
        return None;
    };
    let fr: &[u8] = if upper { b"FR" } else { b"fr" };
    let shaped = head[1].is_ascii_digit()
        && head[2].is_ascii_digit()
        && head[3] == sep
        && &head[4..6] == fr
        && head[6] == sep;
    if !shaped {
        return None;
    }
    let digits = bytes[i + 7..]
        .iter()
        .take_while(|b| b.is_ascii_digit())
        .count();
    if digits == 0 {
        return None;
    }
    let end = i + 7 + digits;
    // `e01_fr_02_and_more` is fine; `E01.FR.08x` is not an id.
    if upper && bytes.get(end).is_some_and(u8::is_ascii_alphabetic) {
        return None;
    }
    let use_case = String::from_utf8_lossy(&head[..3]).to_ascii_uppercase();
    let number = String::from_utf8_lossy(&bytes[i + 7..end]).into_owned();
    Some((use_case, number, end))
}

/// ` / FR.07` or `/07` right after an id.
fn continuation(bytes: &[u8], mut i: usize) -> Option<(String, usize)> {
    while bytes.get(i) == Some(&b' ') {
        i += 1;
    }
    if bytes.get(i) != Some(&b'/') {
        return None;
    }
    i += 1;
    while bytes.get(i) == Some(&b' ') {
        i += 1;
    }
    if bytes.get(i..i + 3) == Some(b"FR.") {
        i += 3;
    }
    let digits = bytes[i.min(bytes.len())..]
        .iter()
        .take_while(|b| b.is_ascii_digit())
        .count();
    let after = bytes.get(i + digits);
    // `/v2`, `/2.0.1`: a version, not a requirement number.
    let dotted = after == Some(&b'.') && bytes.get(i + digits + 1).is_some_and(u8::is_ascii_digit);
    if digits == 0 || dotted || after.is_some_and(u8::is_ascii_alphanumeric) {
        return None;
    }
    Some((
        String::from_utf8_lossy(&bytes[i..i + digits]).into_owned(),
        i + digits,
    ))
}

/// The editions a file's citations can refer to: generated per-version code refers to its
/// own version, everything else to either.
pub fn editions_for(path: &Path) -> &'static [Edition] {
    let dir = |name: &str| path.components().any(|c| c.as_os_str() == name);
    if dir(Edition::V2_0_1.dir()) {
        &[Edition::V2_0_1]
    } else if dir(Edition::V2_1.dir()) {
        &[Edition::V2_1]
    } else {
        &Edition::ALL
    }
}

// ---------------------------------------------------------------------------
// Certification profiles → use cases
// ---------------------------------------------------------------------------

/// The OCPP 2.0.1 Part 5 Table 1 functionality of each certification profile, as Part 2 use
/// cases. Keyed by the slugs of [`crate::profiles::PROFILES`].
///
/// Table 1 names functionality, not use-case ids; the mapping is this repository's reading
/// and each line says which Table 1 entry it stands for. `A00` (the requirements of A §1,
/// security profiles 1–3 and certificates) belongs to Core for profiles 1 and 2 and to
/// Advanced Security for profile 3, so it is counted in both.
pub static PROFILE_USE_CASES: &[(&str, &[&str])] = &[
    (
        "core",
        &[
            "A00", // Basic Authentication; TLS - server-side certificate
            "A01", // Update Charging Station Password for HTTP Basic Authentication
            "A04", // Security Event Notification
            "B01", "B02", "B03", "B04", // Booting a Charging Station
            "B05", "B06", "B07", "B09", "B10", // Configuring a Charging Station (B08: ADM)
            "B11", "B12", // Resetting a Charging Station / EVSE
            "C01", "C09", // Authorization incl. GroupId
            "C16", // Stop Transaction with a Master Pass
            "E02",
            "E03", // Local start transaction - Cable plugin first & Authorization first
            "E01", "E06", // Start / Stop transaction options
            "E09", "E10", // Disconnect cable on EV-side
            "E14", // Check Transaction status
            "F01", "F02", "F03", // Remote start / stop transaction
            "F05", // Remote unlock Connector
            "F06", // Remote Trigger
            "G01", "G02", // (status and heartbeat, which booting and availability rest on)
            "G03", "G04", // Change Availability - Charging Station / EVSE / Connector
            "J01", "J02", // Clock-aligned Meter & Sampled Meter Values
            "M05", // Install CA certificates; AdditionalRootCertificateCheck
            "M03", // Retrieve certificates from Charging Station
            "M04", // Delete a certificate from a Charging Station
            "N01", // Retrieve Log Information
            "N09", "N10", // Get / Clear Customer Information
            "L01", // Secure Firmware Update
            "C10", "C11", // Store / Clear Authorization Data in Authorization Cache
            "C12", // Authorization through authorization cache
        ],
    ),
    (
        "advanced-security",
        &[
            "A00", // TLS - Client-side certificate (security profile 3)
            "A02", "A03", // Update Charging Station Certificate
            "A05", // Upgrade Charging Station Security Profile
        ],
    ),
    (
        "local-auth-list",
        &[
            "C13", "C14", // Authorization through local authorization list
            "D01", // Send Local Authorization List
            "D02", // Get Local List Version
        ],
    ),
    (
        "smart-charging",
        &[
            "K01", // Set charging profile
            "K05", // Remote start transaction with charging profile
            "K08", // Get Composite Schedule
            "K09", // Get Charging Profile
            "K10", // Clear Charging Profile
        ],
    ),
    (
        "advanced-device-management",
        &[
            "N02", // Get Monitoring report
            "N03", // Set Monitoring Base
            "N04", // Set Variable Monitoring
            "N05", // Set Monitoring Level
            "B08", // Get Custom Report
            "N06", // Clear / Remove Monitoring
            "N07", "N08", // Event Notification
        ],
    ),
    (
        "advanced-user-interface",
        &[
            "O01", "O02", "O06", // Set Display Message
            "O03", "O04", // Get Display Message
            "O05", // Clear a Display Message
            "I02", "I03", // Show EV Driver Running / Final Total Cost During / After Charging
            "I01", // Show EV Driver-specific Tariff Information
            "I06", // Update Tariff Information During Transaction
            "I04", "I05", // Configure Fallback Tariff Information & Total Cost Message
        ],
    ),
    (
        "reservation",
        &[
            "H01", // Reserve a specific / unspecified EVSE, a connector type
            "H03", // Reservations using GroupIdToken (using a reserved EVSE)
            "H02", // Cancel reservation of an EVSE
        ],
    ),
    (
        "iso15118",
        &[
            "A02", "A03", // Update Charging Station Certificate
            "M01", "M02", // (Contract) Certificate Installation / Update EV
            "M06", // Get Certificate Status
            "M05", // Install V2G / MO CA certificates
            "M03", // Retrieve V2G / MO certificates from Charging Station
            "M04", // Delete a certificate from a Charging Station
            "C08", // Authorization using External Identification Means
            "C07", // Authorization using Contract Certificates
            "K01", "K05", "K08", "K09", "K10", // ISO 15118 Smart Charging
            "K15", // (charging with load leveling based on HLC, which K16/K17 renegotiate)
            "K16", "K17", // Renegotiating a Charging Schedule
            "J03", // ISO 15118 signed meter values
        ],
    ),
];

/// The use cases of a certification profile, by slug.
pub fn profile_use_cases(slug: &str) -> &'static [&'static str] {
    PROFILE_USE_CASES
        .iter()
        .find(|(s, _)| *s == slug)
        .map_or(&[], |(_, use_cases)| use_cases)
}

/// Every id cited, deduplicated, per edition it can refer to and exists in.
pub fn cited_per_edition<'a>(
    data: &BTreeMap<Edition, Requirements>,
    citations: impl IntoIterator<Item = (&'a Path, &'a Citation)>,
) -> BTreeMap<Edition, BTreeSet<String>> {
    let mut out: BTreeMap<Edition, BTreeSet<String>> = BTreeMap::new();
    for (path, citation) in citations {
        for edition in editions_for(path) {
            if data.get(edition).is_some_and(|r| r.contains(&citation.id)) {
                out.entry(*edition).or_default().insert(citation.id.clone());
            }
        }
    }
    out
}

/// `file:line  id  (why)` for every citation of an id that none of the editions the file can
/// refer to defines.
pub fn unknown_citations(
    data: &BTreeMap<Edition, Requirements>,
    citations: &[(PathBuf, Citation)],
) -> Vec<String> {
    let mut unknown = Vec::new();
    for (path, citation) in citations {
        let editions = editions_for(path);
        if !editions
            .iter()
            .any(|e| data.get(e).is_some_and(|r| r.contains(&citation.id)))
        {
            let labels: Vec<&str> = editions.iter().map(|e| e.label()).collect();
            unknown.push(format!(
                "{}:{}  {}  (not a requirement of {} Part 2)",
                path.display(),
                citation.line,
                citation.id,
                labels.join(" or ")
            ));
        }
    }
    unknown
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A page of `pdftotext -layout` output with the features the parser has to survive: a
    /// contents entry, a table whose rows wrap, a precondition column that names other ids,
    /// a page break with repeated furniture and header, and a note column naming ids.
    const SAMPLE: &str = "\
         B01 - Cold Boot Charging Station . . . . . . . . . . . . . . 44
         B02 - Cold Boot Charging Station - Pending. . . . . . . . . 47
         C03 - Authorization using credit/debit card . . . . . . . . 95

Table 11. Generic requirements
     ID       Precondition                           Requirement definition                   Note
   FR.01                                             The sender SHALL wait.
   FR.02      When the Charging Station receives a The Charging Station SHALL respond
              valid OCPP request message

B01 - Cold Boot Charging Station
Table 35. B01 - Cold Boot Charging Station
   1    Name                      Cold Boot Charging Station

B01 - Cold Boot Charging Station - Requirements
Table 36. B01 - Requirements
       ID       Precondition                                        Requirement definition      Note
 B01.FR.01 After start-up.                                          The Charging Station SHALL  Information
 B01.FR.02 B01.FR.01                                                The CSMS SHALL respond.
           The CSMS has received

OCPP 2.0.1 Edition 2 - \u{a9} Open Charge Alliance 2022                          45/471     Part 2 - Specification
\u{c}Edition 2 FINAL, 2022-12-15                                                       B. Provisioning

    ID      Precondition             Requirements                                      Note
B01.FR.10 B01.FR.02 or               The CSMS SHALL respond with                       See also
          B01.FR.01                  CALLERROR.                                         B01.FR.02
B01.FR.11                            Lowercase continuation.
B02.FR.01 B01.FR.01                  Misplaced row.
";

    #[test]
    fn extracts_row_ids_and_ignores_preconditions_and_notes() {
        let (requirements, findings) = parse(SAMPLE);
        let b01 = requirements.use_case("B01").expect("B01");
        assert_eq!(b01.title, "Cold Boot Charging Station");
        assert_eq!(
            b01.ids,
            ["B01.FR.01", "B01.FR.02", "B01.FR.10", "B01.FR.11"]
        );
        assert!(requirements.contains("FR.01") && requirements.contains("FR.02"));
        assert_eq!(
            requirements.use_case("FR").unwrap().title,
            "Generic requirements"
        );
        assert!(findings.duplicates.is_empty(), "{findings:?}");
        assert_eq!(
            findings.misplaced,
            [("B02.FR.01".to_owned(), "B01".to_owned(), 29)]
        );
    }

    #[test]
    fn keeps_use_cases_without_requirements_and_their_titles() {
        let (requirements, _) = parse(SAMPLE);
        let c03 = requirements
            .use_case("C03")
            .expect("C03 is in the contents");
        assert!(c03.ids.is_empty());
        assert_eq!(c03.title, "Authorization using credit/debit card");
        assert_eq!(
            requirements.use_case("B02").unwrap().title,
            "Cold Boot Charging Station - Pending"
        );
    }

    #[test]
    fn reports_ids_the_specification_states_twice() {
        let text = format!("{SAMPLE} B01.FR.11 again\n");
        let (_, findings) = parse(&text);
        assert_eq!(findings.duplicates.len(), 1);
        assert_eq!(findings.duplicates[0].0, "B01.FR.11");
    }

    #[test]
    fn a_wrapped_precondition_at_the_left_of_its_column_is_not_a_row() {
        // K15.FR.07's precondition wraps onto a line of its own, left of the header text.
        let text = "    ID      Precondition             Requirements\n\
                    K15.FR.07 K15.FR.03 or               The CSMS SHALL send\n          \
                    K15.FR.05                  SetChargingProfileRequest\n";
        let (requirements, _) = parse(text);
        assert_eq!(requirements.use_case("K15").unwrap().ids, ["K15.FR.07"]);
    }

    #[test]
    fn ids_sort_numerically() {
        let text = "  ID   Precondition\n A00.FR.207\n A00.FR.21\n A00.FR.3\n";
        let (requirements, _) = parse(text);
        assert_eq!(
            requirements.use_case("A00").unwrap().ids,
            ["A00.FR.3", "A00.FR.21", "A00.FR.207"]
        );
    }

    #[test]
    fn render_and_load_round_trip() {
        let (requirements, _) = parse(SAMPLE);
        let text = render(Edition::V2_0_1, &requirements);
        assert!(!text.contains("SHALL"), "requirement prose leaked:\n{text}");
        assert_eq!(load(&text).unwrap(), requirements);
    }

    #[test]
    fn load_rejects_an_id_filed_under_the_wrong_use_case() {
        let text = "usecase\tB01\tx\nreq\tB02.FR.01\tB01\n";
        assert!(load(text).is_err());
    }

    #[test]
    fn id_shapes() {
        assert_eq!(use_case_of("B01.FR.10"), Some("B01"));
        assert_eq!(use_case_of("FR.01"), Some("FR"));
        assert_eq!(use_case_of("V2X.05"), Some("V2X"));
        assert_eq!(use_case_of("B01.FR."), None);
        assert_eq!(use_case_of("1.3.2"), None);
        assert_eq!(use_case_of("XX.01"), None);
    }

    fn ids(source: &str) -> Vec<String> {
        citations(source).into_iter().map(|c| c.id).collect()
    }

    #[test]
    fn finds_dotted_snake_case_and_shorthand_citations() {
        assert_eq!(ids("// see E01.FR.08."), ["E01.FR.08"]);
        assert_eq!(ids("fn rejects_b02_fr_09() {}"), ["B02.FR.09"]);
        assert_eq!(
            ids("// B02.FR.04 / FR.07 / FR.08: wait"),
            ["B02.FR.04", "B02.FR.07", "B02.FR.08"]
        );
        assert_eq!(ids("(A00.FR.204/207)"), ["A00.FR.204", "A00.FR.207"]);
        assert_eq!(ids("K01.FR.50 and K01.FR.51"), ["K01.FR.50", "K01.FR.51"]);
    }

    #[test]
    fn ignores_what_is_not_a_part2_citation() {
        // Part 4's rules, a longer word, a version path, a ratio.
        assert!(ids("(Part 4 §4.2.4, FR.07)").is_empty());
        assert!(ids("XB01.FR.01 B01.FR.01x b01_fr_").is_empty());
        assert!(ids("B01.FR.01 / 2.0.1").len() == 1);
        assert!(ids("B01.FR.01/v2").len() == 1);
    }

    #[test]
    fn citation_lines_are_one_based() {
        let found = citations("a\n// E01.FR.08\n");
        assert_eq!(found[0].line, 2);
    }

    #[test]
    fn per_version_code_cites_its_own_version() {
        assert_eq!(
            editions_for(Path::new("src/v2_1/types.rs")),
            &[Edition::V2_1]
        );
        assert_eq!(editions_for(Path::new("src/engine/mod.rs")), &Edition::ALL);
    }

    #[test]
    fn a_mistyped_citation_is_reported_with_file_and_line() {
        let both: BTreeMap<Edition, Requirements> = Edition::ALL
            .into_iter()
            .map(|e| (e, parse(SAMPLE).0))
            .collect();
        let source = "// B01.FR.10 holds\nfn rejects_b01_fr_99() {}\n// FR.07 is Part 4\n";
        let found: Vec<(PathBuf, Citation)> = citations(source)
            .into_iter()
            .map(|c| (PathBuf::from("tests/engine_rules.rs"), c))
            .collect();
        assert_eq!(
            unknown_citations(&both, &found),
            [
                "tests/engine_rules.rs:2  B01.FR.99  (not a requirement of OCPP 2.0.1 or OCPP 2.1 Part 2)"
            ]
        );
        let cited = cited_per_edition(&both, found.iter().map(|(p, c)| (p.as_path(), c)));
        assert_eq!(cited[&Edition::V2_0_1].len(), 1);
    }

    #[test]
    fn an_id_of_the_other_version_fails_in_per_version_code() {
        let mut data = BTreeMap::new();
        data.insert(Edition::V2_0_1, Requirements::default());
        data.insert(Edition::V2_1, parse(SAMPLE).0);
        let found = vec![(
            PathBuf::from("src/v2_0_1/types.rs"),
            Citation {
                id: "B01.FR.10".into(),
                line: 7,
            },
        )];
        assert_eq!(unknown_citations(&data, &found).len(), 1);
    }

    #[test]
    fn every_profile_has_use_cases() {
        for profile in crate::profiles::PROFILES {
            assert!(
                !profile_use_cases(profile.slug).is_empty(),
                "{} has no use cases",
                profile.slug
            );
        }
        assert_eq!(PROFILE_USE_CASES.len(), crate::profiles::PROFILES.len());
    }

    /// The committed data is what CI checks citations against, so it is checked here too —
    /// without the PDFs.
    #[test]
    fn committed_data_is_complete_sorted_and_canonical() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let all = load_all(root).expect("committed requirement data");
        for (edition, requirements) in &all {
            validate(*edition, requirements).unwrap();
            assert_eq!(
                &render(*edition, requirements),
                &std::fs::read_to_string(edition.data_path(root)).unwrap(),
                "{} is not in canonical form",
                edition.label()
            );
            let mut seen = BTreeSet::new();
            for use_case in requirements.use_cases.values() {
                for id in &use_case.ids {
                    assert!(seen.insert(id), "{id} listed twice");
                }
            }
        }
        for (slug, use_cases) in PROFILE_USE_CASES {
            for use_case in *use_cases {
                assert!(
                    all[&Edition::V2_0_1].use_case(use_case).is_some(),
                    "{slug}: {use_case} is not a 2.0.1 use case"
                );
            }
        }
    }
}
