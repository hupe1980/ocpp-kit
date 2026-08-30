//! Extracts the catalogues in OCPP 2.1 Part 2 — Appendices into Rust source.
//!
//! The appendix is a PDF, and it is OCA-licensed material that this repository does not
//! redistribute. What *is* extracted is the interoperability surface: the names of security
//! events and their criticality, the names of standardized components and variables and
//! their data types, the standardized units of measure, and the standardized reason codes.
//! Those are identifiers two implementations must agree on, exactly like the action names and
//! enumeration values the vendored schemas already carry. The prose descriptions are
//! deliberately **not** copied — read them in the appendix.
//!
//! ```text
//! pdftotext -layout specs/ocpp-2.1/OCPP-2.1_part2_appendices_v20.pdf specs/ocpp-2.1/appendices.txt
//! cargo xtask appendix
//! ```
//!
//! The result is committed, so the crate builds without the appendix.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::Path;

use anyhow::{Context, Result, bail};

/// One entry of the security-event table (appendix chapter 1).
#[derive(Debug)]
pub struct SecurityEvent {
    pub name: String,
    pub critical: bool,
}

/// One variable of a standardized component.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Variable {
    pub name: String,
    /// The variable instance, written `Name[Instance]` in the appendix.
    pub instance: Option<String>,
    /// The attribute, written `Name(Attribute)`.
    pub attribute: Option<String>,
    /// The `DataEnumType`, where the appendix gives one.
    pub data_type: Option<String>,
}

/// One standardized component (appendix chapter 3).
#[derive(Debug)]
pub struct Component {
    pub name: String,
    /// `true` for the `…Ctrlr` logical components of §3.1, `false` for the physical ones.
    pub controller: bool,
    pub variables: Vec<Variable>,
}

/// Everything the extractor found.
#[derive(Debug, Default)]
pub struct Appendix {
    pub security_events: Vec<SecurityEvent>,
    pub components: Vec<Component>,
    /// Chapter 4: standardized variable name -> (data type, unit).
    pub variable_types: BTreeMap<String, (String, Option<String>)>,
    /// Chapter 2.
    pub units: Vec<String>,
    /// Chapter 5.
    pub reason_codes: Vec<String>,
}

const DATA_TYPES: &[&str] = &[
    "string",
    "integer",
    "decimal",
    "boolean",
    "dateTime",
    "OptionList",
    "SequenceList",
    "MemberList",
];

pub fn parse(text: &str) -> Result<Appendix> {
    let lines: Vec<&str> = text.lines().collect();
    Ok(Appendix {
        security_events: security_events(&lines)?,
        components: components(&lines)?,
        variable_types: variable_types(&lines),
        units: units(&lines),
        reason_codes: reason_codes(&lines),
    })
}

/// Page furniture that `pdftotext` interleaves with the tables.
fn is_noise(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.is_empty()
        || trimmed.starts_with("OCPP 2.1 ©")
        || trimmed.starts_with("v2.0, 2025")
        || trimmed.starts_with("Part 2 - Appendices")
}

fn chapter<'a>(lines: &[&'a str], start: &str, end: &str) -> Vec<&'a str> {
    let from = lines.iter().position(|l| l.starts_with(start));
    let Some(from) = from else { return Vec::new() };
    let to = lines[from + 1..]
        .iter()
        .position(|l| l.starts_with(end))
        .map_or(lines.len(), |offset| from + 1 + offset);
    lines[from..to].to_vec()
}

// ---------------------------------------------------------------------------
// Chapter 1 — security events
// ---------------------------------------------------------------------------

fn security_events(lines: &[&str]) -> Result<Vec<SecurityEvent>> {
    let section = chapter(lines, "Chapter 1. Security Events", "Chapter 2.");
    let header = section
        .iter()
        .position(|l| l.starts_with("Security Event") && l.contains("Critical"))
        .context("the security-event table header is missing")?;
    let mut events = Vec::new();
    for line in &section[header + 1..] {
        if is_noise(line) {
            continue;
        }
        // A row starts at column 0 and carries a Yes/No in the Critical column.
        let Some(name) = line.split_whitespace().next() else {
            continue;
        };
        if !line.starts_with(name) || !is_identifier(name) {
            continue;
        }
        // A long description overflows into the Critical column, so the verdict is read from
        // the end of the line rather than from a fixed offset.
        let critical = match line.split_whitespace().next_back() {
            Some("Yes") => true,
            Some("No") => false,
            _ => continue,
        };
        events.push(SecurityEvent {
            name: name.to_owned(),
            critical,
        });
    }
    if events.len() < 15 {
        bail!(
            "only {} security events found; the table layout changed",
            events.len()
        );
    }
    Ok(events)
}

// ---------------------------------------------------------------------------
// Chapter 2 — units of measure
// ---------------------------------------------------------------------------

fn units(lines: &[&str]) -> Vec<String> {
    let section = chapter(
        lines,
        "Chapter 2. Standardized Units of Measure",
        "Chapter 3.",
    );
    let Some(header) = section
        .iter()
        .position(|l| l.starts_with("Value") && l.contains("Description"))
    else {
        return Vec::new();
    };
    let mut units = Vec::new();
    for line in &section[header + 1..] {
        if is_noise(line) {
            continue;
        }
        let Some(value) = line.split_whitespace().next() else {
            continue;
        };
        if line.starts_with(value) && value.chars().all(|c| c.is_ascii_alphanumeric()) {
            units.push(value.to_owned());
        }
    }
    units
}

// ---------------------------------------------------------------------------
// Chapter 3 — standardized components
// ---------------------------------------------------------------------------

fn components(lines: &[&str]) -> Result<Vec<Component>> {
    let section = chapter(
        lines,
        "Chapter 3. Standardized Components",
        "3.3. Summary List",
    );
    let mut components: Vec<Component> = Vec::new();
    let mut current: Option<Component> = None;
    // Column offsets of the current table, learned from its header row.
    let mut name_width = 0usize;
    let mut type_column: Option<usize> = None;
    let mut in_table = false;
    // The raw token most recently taken from the name column, which is what decides whether
    // the next one continues it.
    let mut last_token = String::new();

    for line in &section {
        if let Some(heading) = component_heading(line) {
            if let Some(component) = current.take() {
                components.push(component);
            }
            current = Some(Component {
                name: heading.0,
                controller: heading.1,
                variables: Vec::new(),
            });
            in_table = false;
            last_token.clear();
            continue;
        }
        // A numbered section heading or a chapter title ends the table that was in progress;
        // without this the prose introducing §3.2 is read as more of §3.1.19's variables.
        if line.starts_with("Chapter ")
            || line
                .split_whitespace()
                .next()
                .is_some_and(is_section_number)
        {
            in_table = false;
            last_token.clear();
            continue;
        }
        if current.is_none() {
            continue;
        }

        // §3.1 tables are "Variables | Type | Description"; §3.2 tables are
        // "Typically used variables | Description".
        if line.starts_with("Variables") && line.contains("Type") {
            name_width = line.find("Type").expect("checked");
            type_column = Some(name_width);
            in_table = true;
            continue;
        }
        if line.starts_with("Typically used variables") {
            name_width = line.find("Description").unwrap_or(40);
            type_column = None;
            in_table = true;
            continue;
        }
        // `pdftotext` repeats the header word on a page break.
        if line.trim() == "Description" {
            continue;
        }
        if !in_table || is_noise(line) {
            continue;
        }

        let Some(token) = line.split_whitespace().next() else {
            continue;
        };
        if !line.starts_with(token) {
            // Indented: a wrapped description, which is not extracted.
            continue;
        }

        let component = current.as_mut().expect("checked");
        let continues = is_continuation(token, &last_token, component.variables.last(), name_width);
        last_token = token.to_owned();
        if continues {
            if let Some(last) = component.variables.last_mut() {
                last.name.push_str(token);
                // Re-split, since the join may have completed an instance or attribute.
                let rejoined = last.name.clone();
                let (name, instance, attribute) = split_variable(&rejoined);
                last.name = name;
                last.instance = instance.or_else(|| last.instance.clone());
                last.attribute = attribute.or_else(|| last.attribute.clone());
            }
            continue;
        }

        let data_type = type_column.and_then(|column| {
            line.get(column..)
                .and_then(|rest| rest.split_whitespace().next())
                .filter(|word| DATA_TYPES.contains(word))
                .map(ToOwned::to_owned)
        });
        let (name, instance, attribute) = split_variable(token);
        if !is_identifier(&name) {
            continue;
        }
        component.variables.push(Variable {
            name,
            instance,
            attribute,
            data_type,
        });
    }
    if let Some(component) = current.take() {
        components.push(component);
    }

    if components.len() < 60 {
        bail!(
            "only {} components found; the appendix layout changed",
            components.len()
        );
    }
    Ok(components)
}

fn component_heading(line: &str) -> Option<(String, bool)> {
    let rest = line
        .strip_prefix("3.1.")
        .map(|r| (r, true))
        .or_else(|| line.strip_prefix("3.2.").map(|r| (r, false)))?;
    let (rest, controller) = rest;
    // "12. OCPPCommCtrlr (Updated in v1.4)"
    let (_, name) = rest.split_once(". ")?;
    let name = name.split_whitespace().next()?;
    is_identifier(name).then(|| (name.to_owned(), controller))
}

/// Whether a column-0 token continues the previous variable's name rather than starting a new
/// one.
///
/// `pdftotext` breaks a name that did not fit its cell, and it does so mid-word — so the two
/// signals are a fragment that does not start like a name (`mpts`, `reUpdate`), and a previous
/// name that filled its column (`AllowNewSessionsPendingFirmware` + `Update`).
fn is_continuation(
    token: &str,
    last_token: &str,
    previous: Option<&Variable>,
    name_width: usize,
) -> bool {
    if previous.is_none() || last_token.is_empty() {
        return false;
    }
    if token.starts_with(|c: char| c.is_lowercase()) {
        return true;
    }
    // Only the *raw* token that was last placed in the name column can have been truncated by
    // the column edge — not the name accumulated from several of them.
    name_width > 0 && last_token.len() + 1 >= name_width
}

/// `Count[ChargingProfiles](MaxLimit)` -> ("Count", Some("ChargingProfiles"), Some("MaxLimit")).
fn split_variable(token: &str) -> (String, Option<String>, Option<String>) {
    let mut name = token;
    let mut attribute = None;
    if let Some(open) = name.rfind('(') {
        if name.ends_with(')') {
            attribute = Some(name[open + 1..name.len() - 1].to_owned());
            name = &name[..open];
        }
    }
    let mut instance = None;
    if let Some(open) = name.find('[') {
        if name.ends_with(']') {
            instance = Some(name[open + 1..name.len() - 1].to_owned());
            name = &name[..open];
        }
    }
    (name.to_owned(), instance, attribute)
}

/// Whether a token is a numbered section heading such as `3.2.` or `3.1.19.`.
fn is_section_number(token: &str) -> bool {
    token.ends_with('.')
        && token.len() > 1
        && token[..token.len() - 1]
            .split('.')
            .all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit()))
}

fn is_identifier(name: &str) -> bool {
    !name.is_empty()
        && name.starts_with(|c: char| c.is_ascii_uppercase())
        && name.chars().all(|c| c.is_ascii_alphanumeric())
}

// ---------------------------------------------------------------------------
// Chapter 4 — standardized variables
// ---------------------------------------------------------------------------

fn variable_types(lines: &[&str]) -> BTreeMap<String, (String, Option<String>)> {
    let section = chapter(lines, "Chapter 4. Standardized Variables", "Chapter 5.");
    let mut columns: Option<(usize, usize, usize)> = None;
    let mut out: BTreeMap<String, (String, Option<String>)> = BTreeMap::new();
    let mut last: Option<String> = None;

    for line in &section {
        // `pdftotext` repeats the header on every page, and the column offsets shift with the
        // widest cell on that page — so they are re-learned each time rather than fixed once.
        if line.starts_with("Name") && line.contains("DataType") && line.contains("Unit") {
            let type_column = line.find("DataType").expect("checked");
            let unit_column = line.find("Unit").expect("checked");
            let description_column = line.find("Description").unwrap_or(unit_column + 12);
            columns = Some((type_column, unit_column, description_column));
            continue;
        }
        let Some((type_column, unit_column, description_column)) = columns else {
            continue;
        };
        if is_noise(line) {
            continue;
        }
        let Some(token) = line.split_whitespace().next() else {
            continue;
        };
        if !line.starts_with(token) {
            continue;
        }
        let data_type = line
            .get(type_column..)
            .and_then(|rest| rest.split_whitespace().next())
            .filter(|word| DATA_TYPES.contains(word));

        match data_type {
            Some(data_type) => {
                let unit = line
                    .get(unit_column..description_column.max(unit_column))
                    .map(str::trim)
                    .filter(|unit| !unit.is_empty() && !DATA_TYPES.contains(unit))
                    .map(ToOwned::to_owned);
                let (name, _, _) = split_variable(token);
                if is_identifier(&name) {
                    out.insert(name.clone(), (data_type.to_owned(), unit));
                    last = Some(name);
                }
            }
            // A fragment of a wrapped name: complete the previous entry.
            None if token.starts_with(|c: char| c.is_lowercase()) => {
                if let Some(previous) = last.take() {
                    if let Some(value) = out.remove(&previous) {
                        out.insert(format!("{previous}{token}"), value);
                    }
                }
            }
            None => {}
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Chapter 5 — reason codes
// ---------------------------------------------------------------------------

fn reason_codes(lines: &[&str]) -> Vec<String> {
    let section = chapter(lines, "Chapter 5. Reason Codes", "Chapter 6.");
    let mut columns: Option<(usize, usize)> = None;
    let mut codes: BTreeSet<String> = BTreeSet::new();

    for line in &section {
        if line.contains("Reason code") && line.contains("Description") {
            columns = Some((
                line.find("Reason code").expect("checked"),
                line.find("Description").expect("checked"),
            ));
            continue;
        }
        let Some((code_column, description_column)) = columns else {
            continue;
        };
        if is_noise(line) {
            continue;
        }
        let Some(cell) = line.get(code_column..description_column) else {
            continue;
        };
        let cell = cell.trim();
        // The Group column is at 0 and must be empty: a group heading is not a code.
        if is_identifier(cell) && line[..code_column].trim().is_empty() {
            codes.insert(cell.to_owned());
        }
    }
    codes.into_iter().collect()
}

// ---------------------------------------------------------------------------
// Emission
// ---------------------------------------------------------------------------

const HEADER: &str = "\
// @generated by `cargo xtask appendix` from OCPP 2.1 Part 2 — Appendices v2.0 (2025-01-23).
//
// Only the interoperability surface is reproduced: names, data types and criticality. The
// appendix's prose descriptions are not copied, and the appendix itself is not redistributed.
// DO NOT EDIT — re-run the generator instead.
";

pub fn emit(appendix: &Appendix) -> Result<BTreeMap<&'static str, String>> {
    let mut files = BTreeMap::new();
    files.insert("security_events.rs", emit_security_events(appendix));
    files.insert("reason_codes.rs", emit_reason_codes(appendix));
    files.insert("components.rs", emit_components(appendix));
    Ok(files)
}

fn emit_security_events(appendix: &Appendix) -> String {
    let mut out = String::from(HEADER);
    out.push_str(
        "\n//! The standardized security events of OCPP 2.x, and which of them are critical.\n\
         //!\n\
         //! Appendix chapter 1: an implemented event is written to the security log, and an\n\
         //! implemented event marked **critical** is additionally pushed to the CSMS with\n\
         //! `SecurityEventNotification`. Deciding *which* events to implement is the\n\
         //! application's business; knowing which of them are critical is not.\n\n",
    );
    out.push_str("ocpp_enum! {\n");
    out.push_str("    /// A security event from the standardized list.\n");
    out.push_str("    ///\n");
    out.push_str(
        "    /// The list is explicitly non-exhaustive: a vendor-specific event is carried in\n    /// `UnknownValue`, and the specification asks that a standardized event be used\n    /// wherever one matches.\n",
    );
    out.push_str("    SecurityEvent {\n");
    for event in &appendix.security_events {
        let _ = writeln!(out, "        /// `{}`.", event.name);
        let _ = writeln!(out, "        {} = {:?},", event.name, event.name);
    }
    out.push_str("    }\n}\n\n");

    out.push_str("impl SecurityEvent {\n");
    out.push_str(
        "    /// Whether the event must be pushed to the CSMS as well as logged.\n    ///\n    /// A vendor-specific event is treated as critical: the safe default is to report it.\n    #[must_use]\n    #[allow(clippy::match_same_arms)]\n    pub fn is_critical(&self) -> bool {\n        match self {\n",
    );
    for event in &appendix.security_events {
        let _ = writeln!(
            out,
            "            Self::{} => {},",
            event.name, event.critical
        );
    }
    out.push_str("            Self::UnknownValue(_) => true,\n        }\n    }\n\n");
    out.push_str(
        "    /// Every event the specification defines as critical.\n    #[must_use]\n    pub fn critical() -> alloc::vec::Vec<Self> {\n        Self::VARIANTS\n            .iter()\n            .map(|name| Self::from_wire(name))\n            .filter(Self::is_critical)\n            .collect()\n    }\n}\n",
    );
    out
}

fn emit_reason_codes(appendix: &Appendix) -> String {
    let mut out = String::from(HEADER);
    out.push_str(
        "\n//! The standardized `StatusInfo.reasonCode` values.\n\
         //!\n\
         //! Appendix chapter 5. `statusInfo` is optional and every message carries a status of\n\
         //! its own, so a reason code adds insight rather than meaning — but using the\n\
         //! standardized spelling is what lets the other side act on it automatically.\n\n",
    );
    out.push_str("ocpp_enum! {\n");
    out.push_str("    /// A standardized reason code.\n    ReasonCode {\n");
    for code in &appendix.reason_codes {
        let _ = writeln!(out, "        /// `{code}`.");
        let _ = writeln!(out, "        {code} = {code:?},");
    }
    out.push_str("    }\n}\n");
    out
}

fn emit_components(appendix: &Appendix) -> String {
    let mut out = String::from(HEADER);
    out.push_str(
        "\n//! The standardized components and variables of the OCPP 2.x device model.\n\
         //!\n\
         //! Appendix chapter 3 lists the component names two implementations are expected to\n\
         //! agree on, and the variables typically associated with each. Chapter 4 gives the\n\
         //! data type and unit of the variables that are not specific to one controller; those\n\
         //! are filled in here where the appendix leaves the type column empty.\n\
         //!\n\
         //! The list does not imply that a Charging Station must implement any of it. It is a\n\
         //! vocabulary, not a requirement — which is why\n\
         //! [`DeviceModel`](crate::station::device_model::DeviceModel) takes what it needs from\n\
         //! here rather than starting from all of it.\n\n",
    );
    out.push_str(
        "/// Whether a component is one of the logical `…Ctrlr` components or a physical one.\n#[derive(Clone, Copy, Debug, PartialEq, Eq)]\npub enum ComponentKind {\n    /// A logical controller: `OCPPCommCtrlr`, `SecurityCtrlr`, `TxCtrlr`, …\n    Controller,\n    /// A physical part of the station: `EVSE`, `Connector`, `PowerContactor`, …\n    Physical,\n}\n\n",
    );
    out.push_str(
        "/// One variable the appendix associates with a component.\n#[derive(Clone, Copy, Debug, PartialEq, Eq)]\npub struct StandardVariable {\n    /// The variable name.\n    pub name: &'static str,\n    /// The variable instance, written `Name[Instance]` in the appendix.\n    pub instance: Option<&'static str>,\n    /// The attribute, written `Name(Attribute)` — `MaxLimit`, `MinSet`, …\n    pub attribute: Option<&'static str>,\n    /// The `DataEnumType`, where the appendix gives one.\n    pub data_type: Option<&'static str>,\n    /// The unit, for the variables chapter 4 gives one for.\n    pub unit: Option<&'static str>,\n}\n\n",
    );
    out.push_str(
        "/// One standardized component.\n#[derive(Clone, Copy, Debug, PartialEq, Eq)]\npub struct StandardComponent {\n    /// The component name.\n    pub name: &'static str,\n    /// Logical or physical.\n    pub kind: ComponentKind,\n    /// The variables typically associated with it.\n    pub variables: &'static [StandardVariable],\n}\n\n",
    );

    for component in &appendix.components {
        let _ = writeln!(
            out,
            "static {}: &[StandardVariable] = &[",
            component.name.to_uppercase()
        );
        for variable in &component.variables {
            let looked_up = appendix.variable_types.get(&variable.name);
            let data_type = variable
                .data_type
                .clone()
                .or_else(|| looked_up.map(|(t, _)| t.clone()));
            let unit = looked_up.and_then(|(_, unit)| unit.clone());
            let _ = writeln!(
                out,
                "    StandardVariable {{ name: {:?}, instance: {}, attribute: {}, data_type: {}, unit: {} }},",
                variable.name,
                option(variable.instance.as_deref()),
                option(variable.attribute.as_deref()),
                option(data_type.as_deref()),
                option(unit.as_deref()),
            );
        }
        out.push_str("];\n\n");
    }

    let _ = writeln!(
        out,
        "/// Every standardized component, in appendix order.\npub static COMPONENTS: &[StandardComponent] = &["
    );
    for component in &appendix.components {
        let kind = if component.controller {
            "Controller"
        } else {
            "Physical"
        };
        let _ = writeln!(
            out,
            "    StandardComponent {{ name: {:?}, kind: ComponentKind::{kind}, variables: {} }},",
            component.name,
            component.name.to_uppercase()
        );
    }
    out.push_str("];\n\n");

    out.push_str(
        "/// Looks a component up by name.\n#[must_use]\npub fn component(name: &str) -> Option<&'static StandardComponent> {\n    COMPONENTS.iter().find(|component| component.name == name)\n}\n\n",
    );
    out.push_str(
        "/// Looks a variable up within a component.\n#[must_use]\npub fn variable(component: &str, name: &str) -> Option<&'static StandardVariable> {\n    self::component(component)?\n        .variables\n        .iter()\n        .find(|variable| variable.name == name)\n}\n",
    );
    out
}

fn option(value: Option<&str>) -> String {
    value.map_or_else(|| "None".to_string(), |value| format!("Some({value:?})"))
}

/// Where the extracted appendix text is expected to live.
pub fn source_path(root: &Path) -> std::path::PathBuf {
    root.join("specs").join("ocpp-2.1").join("appendices.txt")
}
