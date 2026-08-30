//! The intermediate representation the code generator emits from.

use std::collections::BTreeMap;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum VersionId {
    V1_6,
    V2_0_1,
    V2_1,
}

impl VersionId {
    pub const ALL: [VersionId; 3] = [VersionId::V1_6, VersionId::V2_0_1, VersionId::V2_1];

    pub const fn dir(self) -> &'static str {
        match self {
            VersionId::V1_6 => "v1_6",
            VersionId::V2_0_1 => "v2_0_1",
            VersionId::V2_1 => "v2_1",
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            VersionId::V1_6 => "OCPP 1.6",
            VersionId::V2_0_1 => "OCPP 2.0.1",
            VersionId::V2_1 => "OCPP 2.1",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Constraints {
    pub max_length: Option<u32>,
    pub min_length: Option<u32>,
    pub minimum: Option<f64>,
    pub maximum: Option<f64>,
    pub min_items: Option<u32>,
    pub max_items: Option<u32>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Ty {
    Bool,
    Int,
    Decimal,
    Str,
    DateTime,
    AnyJson,
    Named(String),
    List(Box<Typed>),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Typed {
    pub ty: Ty,
    pub c: Constraints,
}

#[derive(Clone, Debug)]
pub struct Field {
    pub json: String,
    pub rust: String,
    pub typed: Typed,
    pub required: bool,
    pub doc: Option<String>,
    pub default: Option<serde_json::Value>,
}

#[derive(Clone, Debug)]
pub struct StructDef {
    pub name: String,
    pub doc: Option<String>,
    pub fields: Vec<Field>,
}

#[derive(Clone, Debug)]
pub struct EnumVariant {
    pub rust: String,
    pub wire: String,
}

#[derive(Clone, Debug)]
pub struct EnumDef {
    pub name: String,
    pub doc: Option<String>,
    pub variants: Vec<EnumVariant>,
    pub default: Option<String>,
}

/// One OCPP action, with the payload structs that belong to it.
#[derive(Clone, Debug)]
pub struct MessageDef {
    /// Spec action name, e.g. `BootNotification`.
    pub action: String,
    /// Rust identifier for the [`Action`] variant.
    pub variant: String,
    pub request: StructDef,
    /// `None` for `SEND`-only actions (2.1 `NotifyPeriodicEventStream`).
    pub response: Option<StructDef>,
    pub origin: crate::registry::Origin,
    pub kind: crate::registry::Kind,
    pub block: &'static str,
}

#[derive(Debug, Default)]
pub struct VersionModel {
    pub enums: BTreeMap<String, EnumDef>,
    pub structs: BTreeMap<String, StructDef>,
    pub messages: Vec<MessageDef>,
}
