//! Identifier casing helpers shared by the code generator.

/// Rust keywords that must be escaped with the raw-identifier prefix.
const KEYWORDS: &[&str] = &[
    "as", "async", "await", "box", "break", "const", "continue", "crate", "dyn", "else", "enum",
    "extern", "false", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move",
    "mut", "pub", "ref", "return", "self", "static", "struct", "super", "trait", "true", "type",
    "union", "unsafe", "use", "where", "while", "yield", "abstract", "become", "do", "final",
    "macro", "override", "priv", "try", "typeof", "unsized", "virtual",
];

/// `chargePointVendor` -> `charge_point_vendor`, `dischargeLimit_L2` -> `discharge_limit_l2`.
#[must_use]
pub fn snake(input: &str) -> String {
    let chars: Vec<char> = input.chars().collect();
    let mut out = String::with_capacity(input.len() + 4);
    for (i, &c) in chars.iter().enumerate() {
        if c == '_' || c == '-' || c == '.' || c == ' ' {
            push_sep(&mut out);
            continue;
        }
        if c.is_ascii_uppercase() {
            let prev_is_lower_or_digit =
                i > 0 && (chars[i - 1].is_lowercase() || chars[i - 1].is_ascii_digit());
            let next_is_lower = chars.get(i + 1).is_some_and(|n| n.is_lowercase());
            let prev_is_upper = i > 0 && chars[i - 1].is_ascii_uppercase();
            if prev_is_lower_or_digit || (prev_is_upper && next_is_lower) {
                push_sep(&mut out);
            }
            out.extend(c.to_lowercase());
        } else {
            out.push(c);
        }
    }
    let out = out.trim_matches('_').to_string();
    if KEYWORDS.contains(&out.as_str()) {
        format!("r#{out}")
    } else if out.starts_with(|c: char| c.is_ascii_digit()) {
        format!("n{out}")
    } else {
        out
    }
}

fn push_sep(out: &mut String) {
    if !out.is_empty() && !out.ends_with('_') {
        out.push('_');
    }
}

/// `Energy.Active.Import.Register` -> `EnergyActiveImportRegister`, `L1-N` -> `L1N`,
/// `SHA256` -> `SHA256`, `kWh` -> `KWh`.
///
/// Internal capitalisation is preserved so spec acronyms survive verbatim.
#[must_use]
pub fn upper_camel(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for part in input.split(|c: char| !c.is_ascii_alphanumeric()) {
        if part.is_empty() {
            continue;
        }
        let mut cs = part.chars();
        if let Some(first) = cs.next() {
            out.extend(first.to_uppercase());
            out.push_str(cs.as_str());
        }
    }
    if out.starts_with(|c: char| c.is_ascii_digit()) {
        format!("V{out}")
    } else {
        out
    }
}

/// Turns a schema `description` into a doc comment body, normalising the CRLF and
/// `urn:x-oca:` noise that the official schemas are full of.
#[must_use]
pub fn doc_lines(description: Option<&str>, indent: &str) -> String {
    let Some(text) = description else {
        return String::new();
    };
    // Tabs in a doc comment confuse rustdoc's indentation rules; the schemas contain a
    // handful of them.
    let cleaned = text
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .replace('\t', "    ");
    let mut lines: Vec<String> = Vec::new();
    for line in cleaned.split('\n') {
        let line = line.trim_end();
        // Drop the OCA-internal urn identifiers; they are noise in rustdoc.
        if line.trim_start().starts_with("urn:x-oca:") {
            continue;
        }
        lines.push(line.to_string());
    }
    while lines.last().is_some_and(|l| l.trim().is_empty()) {
        lines.pop();
    }
    while lines.first().is_some_and(|l| l.trim().is_empty()) {
        lines.remove(0);
    }
    if lines.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    for line in lines {
        if line.trim().is_empty() {
            out.push_str(indent);
            out.push_str("///\n");
        } else {
            out.push_str(indent);
            out.push_str("/// ");
            out.push_str(&escape_doc(&line));
            out.push('\n');
        }
    }
    out
}

/// Escapes text so it cannot be mistaken for rustdoc markup (bare links, code fences).
fn escape_doc(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '[' => out.push_str("\\["),
            ']' => out.push_str("\\]"),
            '<' if chars.peek().is_some_and(|c| c.is_ascii_alphabetic()) => out.push_str("\\<"),
            _ => out.push(c),
        }
    }
    out
}
