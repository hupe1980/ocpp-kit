//! Reads the vendored OCPP JSON schemas into the code-generator IR.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value};

use crate::model::{
    Constraints, EnumDef, EnumVariant, Field, MessageDef, StructDef, Ty, Typed, VersionId,
    VersionModel,
};
use crate::naming::{snake, upper_camel};
use crate::registry;

/// OCPP 1.6's schemas are anonymous: most enums and nested objects are inline, and the few
/// named `definitions` use the Java-flavoured `…EnumType` spelling. This table gives every
/// one of them the name the 1.6 specification itself uses. Keys are
/// `<schema file stem>:<json pointer>`; `[]` denotes array items and `#Name` a definition.
///
/// The generator fails if a schema contains an inline enum or object that is not listed
/// here, so the table can never silently drift from the schemas.
const V16_NAMES: &[(&str, &str)] = &[
    // ---- named definitions -------------------------------------------------
    (
        "#CertificateSignedStatusEnumType",
        "CertificateSignedStatus",
    ),
    ("#CertificateHashDataType", "CertificateHashData"),
    ("#CertificateUseEnumType", "CertificateUse"),
    (
        "#DeleteCertificateStatusEnumType",
        "DeleteCertificateStatus",
    ),
    ("#FirmwareStatusEnumType", "SignedFirmwareStatus"),
    ("#FirmwareType", "Firmware"),
    ("#GenericStatusEnumType", "GenericStatus"),
    (
        "#GetInstalledCertificateStatusEnumType",
        "GetInstalledCertificateStatus",
    ),
    ("#HashAlgorithmEnumType", "HashAlgorithm"),
    (
        "#InstallCertificateStatusEnumType",
        "InstallCertificateStatus",
    ),
    ("#LogEnumType", "LogType"),
    ("#LogParametersType", "LogParameters"),
    ("#LogStatusEnumType", "LogStatus"),
    ("#MessageTriggerEnumType", "ExtendedMessageTrigger"),
    ("#TriggerMessageStatusEnumType", "TriggerMessageStatus"),
    ("#UpdateFirmwareStatusEnumType", "UpdateFirmwareStatus"),
    ("#UploadLogStatusEnumType", "UploadLogStatus"),
    // ---- inline types ------------------------------------------------------
    ("AuthorizeResponse:/idTagInfo", "IdTagInfo"),
    ("AuthorizeResponse:/idTagInfo/status", "AuthorizationStatus"),
    ("BootNotificationResponse:/status", "RegistrationStatus"),
    (
        "CancelReservationResponse:/status",
        "CancelReservationStatus",
    ),
    ("ChangeAvailability:/type", "AvailabilityType"),
    ("ChangeAvailabilityResponse:/status", "AvailabilityStatus"),
    ("ChangeConfigurationResponse:/status", "ConfigurationStatus"),
    ("ClearCacheResponse:/status", "ClearCacheStatus"),
    (
        "ClearChargingProfile:/chargingProfilePurpose",
        "ChargingProfilePurpose",
    ),
    (
        "ClearChargingProfileResponse:/status",
        "ClearChargingProfileStatus",
    ),
    ("DataTransferResponse:/status", "DataTransferStatus"),
    ("DiagnosticsStatusNotification:/status", "DiagnosticsStatus"),
    ("FirmwareStatusNotification:/status", "FirmwareStatus"),
    ("GetCompositeSchedule:/chargingRateUnit", "ChargingRateUnit"),
    (
        "GetCompositeScheduleResponse:/status",
        "GetCompositeScheduleStatus",
    ),
    (
        "GetCompositeScheduleResponse:/chargingSchedule",
        "ChargingSchedule",
    ),
    (
        "GetCompositeScheduleResponse:/chargingSchedule/chargingRateUnit",
        "ChargingRateUnit",
    ),
    (
        "GetCompositeScheduleResponse:/chargingSchedule/chargingSchedulePeriod[]",
        "ChargingSchedulePeriod",
    ),
    ("GetConfigurationResponse:/configurationKey[]", "KeyValue"),
    ("MeterValues:/meterValue[]", "MeterValue"),
    ("MeterValues:/meterValue[]/sampledValue[]", "SampledValue"),
    (
        "MeterValues:/meterValue[]/sampledValue[]/context",
        "ReadingContext",
    ),
    (
        "MeterValues:/meterValue[]/sampledValue[]/format",
        "ValueFormat",
    ),
    (
        "MeterValues:/meterValue[]/sampledValue[]/measurand",
        "Measurand",
    ),
    ("MeterValues:/meterValue[]/sampledValue[]/phase", "Phase"),
    (
        "MeterValues:/meterValue[]/sampledValue[]/location",
        "Location",
    ),
    (
        "MeterValues:/meterValue[]/sampledValue[]/unit",
        "UnitOfMeasure",
    ),
    ("RemoteStartTransaction:/chargingProfile", "ChargingProfile"),
    (
        "RemoteStartTransaction:/chargingProfile/chargingProfilePurpose",
        "ChargingProfilePurpose",
    ),
    (
        "RemoteStartTransaction:/chargingProfile/chargingProfileKind",
        "ChargingProfileKind",
    ),
    (
        "RemoteStartTransaction:/chargingProfile/recurrencyKind",
        "RecurrencyKind",
    ),
    (
        "RemoteStartTransaction:/chargingProfile/chargingSchedule",
        "ChargingSchedule",
    ),
    (
        "RemoteStartTransaction:/chargingProfile/chargingSchedule/chargingRateUnit",
        "ChargingRateUnit",
    ),
    (
        "RemoteStartTransaction:/chargingProfile/chargingSchedule/chargingSchedulePeriod[]",
        "ChargingSchedulePeriod",
    ),
    (
        "RemoteStartTransactionResponse:/status",
        "RemoteStartStopStatus",
    ),
    (
        "RemoteStopTransactionResponse:/status",
        "RemoteStartStopStatus",
    ),
    ("ReserveNowResponse:/status", "ReservationStatus"),
    ("ResetResponse:/status", "ResetStatus"),
    ("Reset:/type", "ResetType"),
    ("SendLocalList:/updateType", "UpdateType"),
    (
        "SendLocalList:/localAuthorizationList[]",
        "AuthorizationData",
    ),
    (
        "SendLocalList:/localAuthorizationList[]/idTagInfo",
        "IdTagInfo",
    ),
    (
        "SendLocalList:/localAuthorizationList[]/idTagInfo/status",
        "AuthorizationStatus",
    ),
    ("SendLocalListResponse:/status", "UpdateStatus"),
    ("SetChargingProfile:/csChargingProfiles", "ChargingProfile"),
    (
        "SetChargingProfile:/csChargingProfiles/chargingProfilePurpose",
        "ChargingProfilePurpose",
    ),
    (
        "SetChargingProfile:/csChargingProfiles/chargingProfileKind",
        "ChargingProfileKind",
    ),
    (
        "SetChargingProfile:/csChargingProfiles/recurrencyKind",
        "RecurrencyKind",
    ),
    (
        "SetChargingProfile:/csChargingProfiles/chargingSchedule",
        "ChargingSchedule",
    ),
    (
        "SetChargingProfile:/csChargingProfiles/chargingSchedule/chargingRateUnit",
        "ChargingRateUnit",
    ),
    (
        "SetChargingProfile:/csChargingProfiles/chargingSchedule/chargingSchedulePeriod[]",
        "ChargingSchedulePeriod",
    ),
    (
        "SetChargingProfileResponse:/status",
        "ChargingProfileStatus",
    ),
    ("StartTransactionResponse:/idTagInfo", "IdTagInfo"),
    (
        "StartTransactionResponse:/idTagInfo/status",
        "AuthorizationStatus",
    ),
    ("StatusNotification:/errorCode", "ChargePointErrorCode"),
    ("StatusNotification:/status", "ChargePointStatus"),
    ("StopTransaction:/reason", "Reason"),
    ("StopTransaction:/transactionData[]", "MeterValue"),
    (
        "StopTransaction:/transactionData[]/sampledValue[]",
        "SampledValue",
    ),
    (
        "StopTransaction:/transactionData[]/sampledValue[]/context",
        "ReadingContext",
    ),
    (
        "StopTransaction:/transactionData[]/sampledValue[]/format",
        "ValueFormat",
    ),
    (
        "StopTransaction:/transactionData[]/sampledValue[]/measurand",
        "Measurand",
    ),
    (
        "StopTransaction:/transactionData[]/sampledValue[]/phase",
        "Phase",
    ),
    (
        "StopTransaction:/transactionData[]/sampledValue[]/location",
        "Location",
    ),
    (
        "StopTransaction:/transactionData[]/sampledValue[]/unit",
        "UnitOfMeasure",
    ),
    ("StopTransactionResponse:/idTagInfo", "IdTagInfo"),
    (
        "StopTransactionResponse:/idTagInfo/status",
        "AuthorizationStatus",
    ),
    ("TriggerMessage:/requestedMessage", "MessageTrigger"),
    ("TriggerMessageResponse:/status", "TriggerMessageStatus"),
    ("UnlockConnectorResponse:/status", "UnlockStatus"),
];

/// Enums whose value sets legitimately differ between schema files and are merged into the
/// union. `UnitOfMeasure` is the only one: the OCPP 1.6 `StopTransaction` schema omits
/// `Hertz`, which `MeterValues` lists — a schema defect, not a protocol difference.
const V16_UNION_ENUMS: &[&str] = &["UnitOfMeasure"];

pub struct Loader {
    version: VersionId,
    model: VersionModel,
    /// 2.x: schema definition name -> Rust type name.
    names: BTreeMap<String, String>,
}

pub fn load(version: VersionId, schema_dir: &Path) -> Result<VersionModel> {
    let mut loader = Loader {
        version,
        model: VersionModel::default(),
        names: BTreeMap::new(),
    };
    let dir = schema_dir.join(version.dir());

    let mut files: Vec<_> = std::fs::read_dir(&dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .map(|e| e.map(|e| e.path()))
        .collect::<std::result::Result<_, _>>()?;
    files.sort();
    files.retain(|p| p.extension().is_some_and(|e| e == "json"));

    // Pass 0: decide the Rust name of every 2.x definition up front. `…EnumType` normally
    // loses both suffixes — `BootReasonEnumType` becomes `BootReason` — but OCPP 2.x has
    // enums whose short name is already taken by an object (`IdTokenType`) or by an action
    // (`Reset`, `TransactionEvent`). Those keep their `Enum`, which is exactly the schema's
    // own `javaType`.
    if version != VersionId::V1_6 {
        let mut is_enum: BTreeMap<String, bool> = BTreeMap::new();
        for path in &files {
            let doc: Value = serde_json::from_str(&std::fs::read_to_string(path)?)?;
            if let Some(defs) = doc.get("definitions").and_then(Value::as_object) {
                for (name, def) in defs {
                    let e = def.get("enum").is_some();
                    is_enum
                        .entry(name.clone())
                        .and_modify(|v| *v |= e)
                        .or_insert(e);
                }
            }
        }
        let mut reserved: BTreeSet<String> = is_enum
            .iter()
            .filter(|(_, e)| !**e)
            .map(|(n, _)| n.strip_suffix("Type").unwrap_or(n).to_string())
            .collect();
        // Action names are reserved too: `ResetEnumType` must not become `Reset`, which is
        // the name of an action, or `TransactionEventEnumType` become `TransactionEvent`.
        for path in &files {
            let stem = stem(path);
            let action = stem
                .strip_suffix("Request")
                .or_else(|| stem.strip_suffix("Response"))
                .unwrap_or(&stem);
            reserved.insert(action.to_string());
        }
        for (name, enumish) in &is_enum {
            if name == "CustomDataType" {
                continue;
            }
            let base = name.strip_suffix("Type").unwrap_or(name);
            let rust = if *enumish {
                let stripped = base.strip_suffix("Enum").unwrap_or(base);
                if reserved.contains(stripped) {
                    base.to_string()
                } else {
                    stripped.to_string()
                }
            } else {
                base.to_string()
            };
            loader.names.insert(name.clone(), rust);
        }
    }

    // Pass 1: register every named definition so `$ref`s resolve regardless of file order.
    for path in &files {
        let doc: Value = serde_json::from_str(&std::fs::read_to_string(path)?)
            .with_context(|| format!("parsing {}", path.display()))?;
        loader
            .register_definitions(&doc)
            .with_context(|| path.display().to_string())?;
    }

    // Pass 2: message payloads.
    for info in registry::table(version) {
        let (req_path, resp_path) = schema_paths(version, &dir, info.action);
        // The 2.x table is shared between 2.0.1 and 2.1; an action without a schema in this
        // version's directory simply does not exist in that version.
        let Some(req_path) = req_path else {
            continue;
        };
        let req_doc: Value = serde_json::from_str(&std::fs::read_to_string(&req_path)?)?;
        let request = loader
            .object_struct(
                &format!("{}Request", info.action),
                &req_doc,
                &stem(&req_path),
                "",
                doc_of(&req_doc),
            )
            .with_context(|| req_path.display().to_string())?;

        let response = match resp_path {
            Some(p) => {
                let doc: Value = serde_json::from_str(&std::fs::read_to_string(&p)?)?;
                Some(
                    loader
                        .object_struct(
                            &format!("{}Response", info.action),
                            &doc,
                            &stem(&p),
                            "",
                            doc_of(&doc),
                        )
                        .with_context(|| p.display().to_string())?,
                )
            }
            None => None,
        };

        if response.is_none() && info.kind != registry::Kind::Send {
            bail!(
                "{}: {} has no response schema but is not a SEND",
                version.dir(),
                info.action
            );
        }

        loader.model.messages.push(MessageDef {
            action: info.action.to_string(),
            variant: upper_camel(info.action),
            request,
            response,
            origin: info.origin,
            kind: info.kind,
            block: info.block,
        });
    }

    // Every schema file must be accounted for; otherwise the action registry is stale.
    let mut expected: Vec<String> = Vec::new();
    for info in registry::table(version) {
        let (a, b) = schema_paths(version, &dir, info.action);
        expected.extend(a.iter().chain(b.iter()).map(|p| stem(p)));
    }
    for path in &files {
        let s = stem(path);
        if !expected.contains(&s) {
            bail!(
                "{}: schema {s}.json is not covered by the action registry",
                version.dir()
            );
        }
    }

    let expected_actions = match version {
        VersionId::V1_6 => 39,
        VersionId::V2_0_1 => 64,
        VersionId::V2_1 => 91,
    };
    if loader.model.messages.len() != expected_actions {
        bail!(
            "{}: expected {expected_actions} actions, generated {}",
            version.dir(),
            loader.model.messages.len()
        );
    }

    loader.check_collisions()?;
    Ok(loader.model)
}

fn stem(path: &Path) -> String {
    path.file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned()
}

fn doc_of(doc: &Value) -> Option<String> {
    doc.get("description")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

/// Maps an action to its `…Request` / `…Response` schema file names, honouring OCPP 1.6's
/// unsuffixed request files and 2.1's response-less `NotifyPeriodicEventStream`.
fn schema_paths(
    version: VersionId,
    dir: &Path,
    action: &str,
) -> (Option<std::path::PathBuf>, Option<std::path::PathBuf>) {
    let pick = |names: &[String]| -> Option<std::path::PathBuf> {
        names
            .iter()
            .map(|n| dir.join(format!("{n}.json")))
            .find(|p| p.exists())
    };
    let req = match version {
        VersionId::V1_6 => pick(&[action.to_string(), format!("{action}Request")]),
        _ => pick(&[format!("{action}Request"), action.to_string()]),
    };
    let resp = pick(&[format!("{action}Response")]);
    (req, resp)
}

impl Loader {
    fn register_definitions(&mut self, doc: &Value) -> Result<()> {
        let Some(defs) = doc.get("definitions").and_then(Value::as_object) else {
            return Ok(());
        };
        for (def_name, def) in defs {
            if def_name == "CustomDataType" {
                continue; // modelled by `ocpp_kit::types::CustomData`
            }
            let name = self.definition_name(def_name)?;
            self.register_named(&name, def, "", "")?;
        }
        Ok(())
    }

    fn definition_name(&self, def_name: &str) -> Result<String> {
        match self.version {
            VersionId::V1_6 => lookup_v16(&format!("#{def_name}"))
                .map(ToOwned::to_owned)
                .with_context(|| {
                    format!("OCPP 1.6 definition {def_name} is missing from V16_NAMES")
                }),
            _ => self
                .names
                .get(def_name)
                .cloned()
                .with_context(|| format!("unknown definition {def_name}")),
        }
    }

    /// Registers a named enum or struct, merging with any previously registered definition
    /// of the same name (the official 2.x schemas repeat definitions verbatim per file).
    fn register_named(
        &mut self,
        name: &str,
        node: &Value,
        file: &str,
        pointer: &str,
    ) -> Result<()> {
        if node.get("enum").is_some() {
            let def = self.enum_def(name, node)?;
            self.merge_enum(def)?;
        } else if node.get("properties").is_some()
            || node.get("type").and_then(Value::as_str) == Some("object")
        {
            let def = self.object_struct(name, node, file, pointer, description(node))?;
            self.merge_struct(def)?;
        } else {
            bail!("definition {name} is neither an enum nor an object: {node}");
        }
        Ok(())
    }

    fn enum_def(&self, name: &str, node: &Value) -> Result<EnumDef> {
        let values = node["enum"].as_array().context("enum must be an array")?;
        let mut variants = Vec::with_capacity(values.len());
        for v in values {
            let wire = v.as_str().context("only string enums are supported")?;
            variants.push(EnumVariant {
                rust: upper_camel(wire),
                wire: wire.to_string(),
            });
        }
        let mut seen = BTreeMap::new();
        for v in &variants {
            if v.rust == "UnknownValue" {
                bail!(
                    "enum {name}: value {:?} collides with the generated open `UnknownValue` \
                     variant; rename the catch-all in `src/macros.rs`",
                    v.wire
                );
            }
            if let Some(prev) = seen.insert(v.rust.clone(), v.wire.clone()) {
                bail!(
                    "enum {name}: variants {prev:?} and {:?} both map to `{}`",
                    v.wire,
                    v.rust
                );
            }
        }
        Ok(EnumDef {
            name: name.to_string(),
            doc: description(node),
            variants,
            default: node
                .get("default")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
        })
    }

    fn merge_enum(&mut self, def: EnumDef) -> Result<()> {
        match self.model.enums.get_mut(&def.name) {
            None => {
                self.model.enums.insert(def.name.clone(), def);
            }
            Some(existing) => {
                let same: Vec<_> = existing.variants.iter().map(|v| v.wire.clone()).collect();
                let incoming: Vec<_> = def.variants.iter().map(|v| v.wire.clone()).collect();
                if same != incoming {
                    if !V16_UNION_ENUMS.contains(&def.name.as_str()) {
                        bail!(
                            "enum {} declared twice with different values:\n  {same:?}\n  {incoming:?}",
                            def.name
                        );
                    }
                    for v in def.variants {
                        if !existing.variants.iter().any(|e| e.wire == v.wire) {
                            existing.variants.push(v);
                        }
                    }
                    existing.variants.sort_by(|a, b| a.wire.cmp(&b.wire));
                }
                if existing.doc.is_none() {
                    existing.doc = def.doc;
                }
            }
        }
        Ok(())
    }

    fn merge_struct(&mut self, def: StructDef) -> Result<()> {
        match self.model.structs.get_mut(&def.name) {
            None => {
                self.model.structs.insert(def.name.clone(), def);
            }
            Some(existing) => {
                // Shapes must agree; constraints may not. The OCPP 1.6 schemas repeat the
                // same type with slightly different bounds (`MeterValue.sampledValue` is
                // `minItems: 1` under `MeterValues` but unbounded under `StopTransaction`).
                // The tighter bound wins, so validation matches the specification's own
                // cardinality tables.
                let shape = |d: &StructDef| -> Vec<(String, Ty, bool)> {
                    d.fields
                        .iter()
                        .map(|f| (f.json.clone(), erase(&f.typed), f.required))
                        .collect()
                };
                if shape(existing) != shape(&def) {
                    bail!(
                        "struct {} declared twice with different shapes:\n  {:#?}\n  {:#?}",
                        def.name,
                        shape(existing),
                        shape(&def)
                    );
                }
                for (a, b) in existing.fields.iter_mut().zip(def.fields) {
                    if a.doc.is_none() {
                        a.doc = b.doc;
                    }
                    tighten(&mut a.typed, &b.typed);
                }
                if existing.doc.is_none() {
                    existing.doc = def.doc;
                }
            }
        }
        Ok(())
    }

    fn object_struct(
        &mut self,
        name: &str,
        node: &Value,
        file: &str,
        pointer: &str,
        doc: Option<String>,
    ) -> Result<StructDef> {
        let empty = Map::new();
        let props = node
            .get("properties")
            .and_then(Value::as_object)
            .unwrap_or(&empty);
        let required: Vec<&str> = node
            .get("required")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();

        let mut fields = Vec::with_capacity(props.len());
        for (json_name, prop) in props {
            let child_pointer = format!("{pointer}/{json_name}");
            let typed = self.typed(prop, file, &child_pointer)?;
            fields.push(Field {
                json: json_name.clone(),
                rust: snake(json_name),
                typed,
                required: required.contains(&json_name.as_str()),
                doc: description(prop),
                default: prop.get("default").cloned(),
            });
        }
        // `customData` first is noisy; keep schema order but move it to the end.
        fields.sort_by_key(|f| u8::from(f.json == "customData"));
        Ok(StructDef {
            name: name.to_string(),
            doc,
            fields,
        })
    }

    fn typed(&mut self, node: &Value, file: &str, pointer: &str) -> Result<Typed> {
        let c = constraints(node);

        if let Some(reference) = node.get("$ref").and_then(Value::as_str) {
            let def_name = reference
                .strip_prefix("#/definitions/")
                .with_context(|| format!("unsupported $ref {reference}"))?;
            let ty = if def_name == "CustomDataType" {
                Ty::Named("CustomData".to_string())
            } else {
                Ty::Named(self.definition_name(def_name)?)
            };
            return Ok(Typed { ty, c });
        }

        let Some(ty_name) = node.get("type").and_then(Value::as_str) else {
            // Only OCPP 2.x `DataTransfer.data` is untyped: "data without specified length
            // or format" (Part 2, P01).
            return Ok(Typed { ty: Ty::AnyJson, c });
        };

        let ty = match ty_name {
            "boolean" => Ty::Bool,
            "integer" => Ty::Int,
            "number" => Ty::Decimal,
            "string" => {
                if node.get("enum").is_some() {
                    let name = self.inline_name(file, pointer)?;
                    let def = self.enum_def(&name, node)?;
                    self.merge_enum(def)?;
                    Ty::Named(name)
                } else if node.get("format").and_then(Value::as_str) == Some("date-time") {
                    Ty::DateTime
                } else {
                    Ty::Str
                }
            }
            "array" => {
                let items = node.get("items").context("array without items")?;
                let inner = self.typed(items, file, &format!("{pointer}[]"))?;
                Ty::List(Box::new(inner))
            }
            "object" => {
                let name = self.inline_name(file, pointer)?;
                let def = self.object_struct(&name, node, file, pointer, description(node))?;
                self.merge_struct(def)?;
                Ty::Named(name)
            }
            other => bail!("unsupported JSON schema type {other:?} at {file}:{pointer}"),
        };
        Ok(Typed { ty, c })
    }

    fn inline_name(&self, file: &str, pointer: &str) -> Result<String> {
        if self.version != VersionId::V1_6 {
            bail!(
                "{}: unexpected inline type at {file}:{pointer} — 2.x schemas are expected to \
                 name every type in `definitions`",
                self.version.dir()
            );
        }
        lookup_v16(&format!("{file}:{pointer}"))
            .map(ToOwned::to_owned)
            .with_context(|| {
                format!("OCPP 1.6 inline type {file}:{pointer} is missing from V16_NAMES")
            })
    }

    fn check_collisions(&self) -> Result<()> {
        for name in self.model.enums.keys() {
            if self.model.structs.contains_key(name) {
                bail!("{name} is generated both as an enum and as a struct");
            }
        }
        for msg in &self.model.messages {
            for n in [Some(&msg.request), msg.response.as_ref()]
                .into_iter()
                .flatten()
            {
                if self.model.structs.contains_key(&n.name)
                    || self.model.enums.contains_key(&n.name)
                {
                    bail!("message payload {} collides with a shared type", n.name);
                }
            }
        }
        Ok(())
    }
}

/// The structural shape of a type, with all constraints removed.
fn erase(t: &Typed) -> Ty {
    match &t.ty {
        Ty::List(inner) => Ty::List(Box::new(Typed {
            ty: erase(inner),
            c: Constraints::default(),
        })),
        other => other.clone(),
    }
}

/// Narrows `into` to the tighter of the two constraint sets.
fn tighten(into: &mut Typed, other: &Typed) {
    fn max_opt<T: Ord + Copy>(a: Option<T>, b: Option<T>) -> Option<T> {
        match (a, b) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (x, y) => x.or(y),
        }
    }
    fn min_opt<T: Ord + Copy>(a: Option<T>, b: Option<T>) -> Option<T> {
        match (a, b) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (x, y) => x.or(y),
        }
    }
    into.c.min_length = max_opt(into.c.min_length, other.c.min_length);
    into.c.min_items = max_opt(into.c.min_items, other.c.min_items);
    into.c.max_length = min_opt(into.c.max_length, other.c.max_length);
    into.c.max_items = min_opt(into.c.max_items, other.c.max_items);
    into.c.minimum = match (into.c.minimum, other.c.minimum) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (x, y) => x.or(y),
    };
    into.c.maximum = match (into.c.maximum, other.c.maximum) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (x, y) => x.or(y),
    };
    if let (Ty::List(a), Ty::List(b)) = (&mut into.ty, &other.ty) {
        tighten(a, b);
    }
}

fn lookup_v16(key: &str) -> Option<&'static str> {
    V16_NAMES.iter().find(|(k, _)| *k == key).map(|(_, v)| *v)
}

fn description(node: &Value) -> Option<String> {
    node.get("description")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

fn constraints(node: &Value) -> Constraints {
    let u32_of = |k: &str| {
        node.get(k)
            .and_then(Value::as_u64)
            .and_then(|v| u32::try_from(v).ok())
    };
    Constraints {
        max_length: u32_of("maxLength"),
        min_length: u32_of("minLength"),
        minimum: node.get("minimum").and_then(Value::as_f64),
        maximum: node.get("maximum").and_then(Value::as_f64),
        min_items: u32_of("minItems"),
        max_items: u32_of("maxItems"),
    }
}
