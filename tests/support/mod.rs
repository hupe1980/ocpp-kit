//! A miniature JSON Schema engine, just large enough for the OCPP schemas.
//!
//! The vendored schemas use a small, regular subset of draft-04/draft-06: objects with
//! `properties` / `required` / `additionalProperties`, `$ref` into `definitions`, string
//! enumerations, `maxLength` / `minLength`, `minimum` / `maximum`, arrays with
//! `minItems` / `maxItems`, and `format: date-time`. Nothing else appears, so a
//! purpose-built generator and validator are both short and exact — and, unlike a general
//! validator crate, they can be asked to *produce* instances, which is what turns the
//! conformance suite into a real test of the generated Rust types.

#![allow(dead_code)]
// A test-only generator: values are bounded by the schemas, far below any cast limit.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]
#![allow(clippy::cast_possible_wrap, clippy::items_after_statements)]

use std::path::{Path, PathBuf};

use ocpp_kit::Version;

use serde_json::{Map, Value, json};

// ---------------------------------------------------------------------------
// Deterministic pseudo-random source
// ---------------------------------------------------------------------------

/// xorshift64*, so a failing case can always be reproduced from its seed.
pub fn schemas_dir(version: Version) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("schemas")
        .join(version.slug())
}

/// Locates the schema file for one action, honouring 1.6's unsuffixed request files and
/// 2.1's response-less `NotifyPeriodicEventStream`.
pub fn schema_path(version: Version, action: &str, response: bool) -> Option<PathBuf> {
    let dir = schemas_dir(version);
    let candidates = if response {
        vec![format!("{action}Response.json")]
    } else {
        vec![format!("{action}Request.json"), format!("{action}.json")]
    };
    candidates
        .into_iter()
        .map(|name| dir.join(name))
        .find(|path| path.exists())
}

pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed | 1)
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    pub fn below(&mut self, bound: usize) -> usize {
        if bound == 0 {
            0
        } else {
            (self.next_u64() % bound as u64) as usize
        }
    }

    pub fn chance(&mut self, percent: u64) -> bool {
        self.next_u64() % 100 < percent
    }
}

// ---------------------------------------------------------------------------
// Instance generation
// ---------------------------------------------------------------------------

pub struct Generator<'a> {
    root: &'a Value,
    rng: Rng,
    /// Probability, in percent, that an optional member is included.
    pub optional_chance: u64,
    depth: usize,
}

impl<'a> Generator<'a> {
    pub fn new(root: &'a Value, seed: u64) -> Self {
        Self {
            root,
            rng: Rng::new(seed),
            optional_chance: 70,
            depth: 0,
        }
    }

    pub fn generate(&mut self) -> Value {
        let root = self.root.clone();
        self.value(&root)
    }

    fn resolve(&self, node: &Value) -> Value {
        match node.get("$ref").and_then(Value::as_str) {
            Some(reference) => {
                let name = reference.trim_start_matches("#/definitions/");
                self.root["definitions"][name].clone()
            }
            None => node.clone(),
        }
    }

    fn value(&mut self, node: &Value) -> Value {
        let node = self.resolve(node);
        if let Some(values) = node.get("enum").and_then(Value::as_array) {
            let index = self.rng.below(values.len());
            return values[index].clone();
        }
        match node.get("type").and_then(Value::as_str) {
            Some("object") => self.object(&node),
            Some("array") => self.array(&node),
            Some("string") => Value::String(self.string(&node)),
            Some("integer") => {
                // The schemas spell bounds as JSON numbers (`"maximum": 100.0`), so they
                // must be read as floats even for an integer member.
                let min = node
                    .get("minimum")
                    .and_then(Value::as_f64)
                    .map_or(0, |value| value.ceil() as i64);
                let max = node
                    .get("maximum")
                    .and_then(Value::as_f64)
                    .map_or(min + 1000, |value| value.floor() as i64);
                let span = usize::try_from(max - min).unwrap_or(1000) + 1;
                json!(min + self.rng.below(span) as i64)
            }
            Some("number") => {
                let min = node
                    .get("minimum")
                    .and_then(Value::as_f64)
                    .unwrap_or(-1000.0);
                let max = node
                    .get("maximum")
                    .and_then(Value::as_f64)
                    .unwrap_or(1000.0);
                // Both integral and fractional values are generated: `Decimal` carries the
                // number the schema instance wrote, so `5` comes back as `5` rather than as
                // `5.0` and there is no longer a spelling the round trip cannot survive.
                let steps = self.rng.below(100) as f64 / 100.0;
                let value = min + (max - min) * steps;
                let rounded = (value * 100.0).round() / 100.0;
                if self.rng.chance(30) {
                    json!(rounded.trunc() as i64)
                } else {
                    json!(rounded + 0.25)
                }
            }
            Some("boolean") => json!(self.rng.chance(50)),
            // Only OCPP 2.x `DataTransfer.data` is untyped.
            _ => json!({ "arbitrary": "value" }),
        }
    }

    fn object(&mut self, node: &Value) -> Value {
        let mut out = Map::new();
        let required: Vec<&str> = node
            .get("required")
            .and_then(Value::as_array)
            .map(|items| items.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let Some(properties) = node.get("properties").and_then(Value::as_object) else {
            return Value::Object(out);
        };
        self.depth += 1;
        for (name, schema) in properties {
            let is_required = required.contains(&name.as_str());
            // Bound recursion through `customData`, and thin out deep optional branches so
            // one instance stays a readable size.
            let include = is_required
                || (self.depth < 6 && self.rng.chance(self.optional_chance / self.depth as u64));
            if include {
                out.insert(name.clone(), self.value(schema));
            }
        }
        self.depth -= 1;
        Value::Object(out)
    }

    fn array(&mut self, node: &Value) -> Value {
        let items = node
            .get("items")
            .cloned()
            .unwrap_or(json!({"type": "string"}));
        let min = node.get("minItems").and_then(Value::as_u64).unwrap_or(1) as usize;
        let max = node.get("maxItems").and_then(Value::as_u64).unwrap_or(2) as usize;
        // Nested arrays multiply, and OCPP nests them four deep (charging profiles, tariffs,
        // DER curves). Keep the element count at the minimum plus at most one, or a single
        // instance reaches megabytes.
        let extra = usize::from(self.depth < 3 && self.rng.chance(50));
        let count = (min + extra).clamp(min, max.max(min));
        Value::Array((0..count).map(|_| self.value(&items)).collect())
    }

    fn string(&mut self, node: &Value) -> String {
        if node.get("format").and_then(Value::as_str) == Some("date-time") {
            // Always UTC: the Rust types normalise offsets to `Z` on the way out.
            let year = 2020 + self.rng.below(6);
            let month = 1 + self.rng.below(12);
            let day = 1 + self.rng.below(28);
            return format!(
                "{year:04}-{month:02}-{day:02}T{:02}:00:00Z",
                self.rng.below(24)
            );
        }
        let max = node.get("maxLength").and_then(Value::as_u64).unwrap_or(12) as usize;
        let min = node.get("minLength").and_then(Value::as_u64).unwrap_or(1) as usize;
        let len = min
            .max(1)
            .max(self.rng.below(max.min(24) + 1))
            .min(max.max(1));
        const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
        (0..len)
            .map(|_| ALPHABET[self.rng.below(ALPHABET.len())] as char)
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

/// Validates `instance` against `schema`, returning every problem found.
pub fn validate(schema: &Value, instance: &Value) -> Vec<String> {
    let mut errors = Vec::new();
    check(schema, schema, instance, "", &mut errors);
    errors
}

fn resolve<'a>(root: &'a Value, node: &'a Value) -> &'a Value {
    match node.get("$ref").and_then(Value::as_str) {
        Some(reference) => &root["definitions"][reference.trim_start_matches("#/definitions/")],
        None => node,
    }
}

fn check(root: &Value, node: &Value, instance: &Value, path: &str, errors: &mut Vec<String>) {
    let node = resolve(root, node);

    if let Some(values) = node.get("enum").and_then(Value::as_array) {
        if !values.contains(instance) {
            errors.push(format!("{path}: {instance} is not one of {values:?}"));
        }
        return;
    }

    match node.get("type").and_then(Value::as_str) {
        Some("object") => {
            let Some(members) = instance.as_object() else {
                errors.push(format!("{path}: expected an object, found {instance}"));
                return;
            };
            for name in node
                .get("required")
                .and_then(Value::as_array)
                .map(|items| items.iter().filter_map(Value::as_str).collect::<Vec<_>>())
                .unwrap_or_default()
            {
                if !members.contains_key(name) {
                    errors.push(format!("{path}: required member {name:?} is missing"));
                }
            }
            let empty = Map::new();
            let properties = node
                .get("properties")
                .and_then(Value::as_object)
                .unwrap_or(&empty);
            let closed = node.get("additionalProperties") == Some(&Value::Bool(false));
            for (name, value) in members {
                match properties.get(name) {
                    Some(schema) => check(root, schema, value, &format!("{path}/{name}"), errors),
                    None if closed => {
                        errors.push(format!(
                            "{path}/{name}: member is not defined by the schema"
                        ));
                    }
                    None => {}
                }
            }
        }
        Some("array") => {
            let Some(items) = instance.as_array() else {
                errors.push(format!("{path}: expected an array, found {instance}"));
                return;
            };
            if let Some(min) = node.get("minItems").and_then(Value::as_u64) {
                if (items.len() as u64) < min {
                    errors.push(format!("{path}: minItems {min} not reached"));
                }
            }
            if let Some(max) = node.get("maxItems").and_then(Value::as_u64) {
                if (items.len() as u64) > max {
                    errors.push(format!("{path}: maxItems {max} exceeded"));
                }
            }
            if let Some(schema) = node.get("items") {
                for (index, item) in items.iter().enumerate() {
                    check(root, schema, item, &format!("{path}/{index}"), errors);
                }
            }
        }
        Some("string") => {
            let Some(text) = instance.as_str() else {
                errors.push(format!("{path}: expected a string, found {instance}"));
                return;
            };
            let len = text.chars().count() as u64;
            if let Some(max) = node.get("maxLength").and_then(Value::as_u64) {
                if len > max {
                    errors.push(format!("{path}: maxLength {max} exceeded ({len})"));
                }
            }
            if let Some(min) = node.get("minLength").and_then(Value::as_u64) {
                if len < min {
                    errors.push(format!("{path}: minLength {min} not reached ({len})"));
                }
            }
        }
        Some("integer") => {
            if !instance.is_i64() && !instance.is_u64() {
                errors.push(format!("{path}: expected an integer, found {instance}"));
                return;
            }
            check_range(node, instance, path, errors);
        }
        Some("number") => {
            if !instance.is_number() {
                errors.push(format!("{path}: expected a number, found {instance}"));
                return;
            }
            check_range(node, instance, path, errors);
        }
        Some("boolean") if !instance.is_boolean() => {
            errors.push(format!("{path}: expected a boolean, found {instance}"));
        }
        _ => {}
    }
}

fn check_range(node: &Value, instance: &Value, path: &str, errors: &mut Vec<String>) {
    let Some(value) = instance.as_f64() else {
        return;
    };
    if let Some(min) = node.get("minimum").and_then(Value::as_f64) {
        if value < min {
            errors.push(format!("{path}: minimum {min} violated ({value})"));
        }
    }
    if let Some(max) = node.get("maximum").and_then(Value::as_f64) {
        if value > max {
            errors.push(format!("{path}: maximum {max} violated ({value})"));
        }
    }
}

/// Compares two JSON documents, ignoring member order and integer/float spelling.
pub fn differences(expected: &Value, actual: &Value, path: &str, out: &mut Vec<String>) {
    match (expected, actual) {
        (Value::Object(a), Value::Object(b)) => {
            for (key, value) in a {
                match b.get(key) {
                    Some(other) => differences(value, other, &format!("{path}/{key}"), out),
                    None => out.push(format!("{path}/{key}: dropped by the Rust type")),
                }
            }
            for key in b.keys() {
                if !a.contains_key(key) {
                    out.push(format!("{path}/{key}: invented by the Rust type"));
                }
            }
        }
        (Value::Array(a), Value::Array(b)) => {
            if a.len() != b.len() {
                out.push(format!("{path}: length {} became {}", a.len(), b.len()));
                return;
            }
            for (index, (x, y)) in a.iter().zip(b).enumerate() {
                differences(x, y, &format!("{path}/{index}"), out);
            }
        }
        (Value::Number(a), Value::Number(b)) => {
            if a.as_f64() != b.as_f64() {
                out.push(format!("{path}: {a} became {b}"));
            }
        }
        _ => {
            if expected != actual {
                out.push(format!("{path}: {expected} became {actual}"));
            }
        }
    }
}
