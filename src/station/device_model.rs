//! The OCPP 2.x device model: components, variables, attributes and monitors.
//!
//! The device model is the part of OCPP 2.x that replaced 1.6's flat configuration keys, and
//! it is where most of the work of `GetVariables`, `SetVariables`, `GetBaseReport`,
//! `GetReport`, `SetVariableMonitoring` and `NotifyEvent` lives. The registry here is
//! version-neutral — 2.0.1 and 2.1 model it identically — and converts into either version's
//! generated types.
//!
//! ```
//! use ocpp_kit::station::device_model::{
//!     Attribute, DataType, DeviceModel, Mutability, SetStatus, VariableSpec,
//! };
//!
//! let mut model = DeviceModel::with_defaults();
//! model.declare(
//!     "SampleCtrlr",
//!     VariableSpec::new("Enabled", DataType::Boolean)
//!         .mutability(Mutability::ReadWrite)
//!         .value("true"),
//! );
//!
//! assert_eq!(model.set("SampleCtrlr", "Enabled", Attribute::Actual, "false"), SetStatus::Accepted);
//! // A value the declared type cannot hold is rejected, not stored.
//! assert_eq!(model.set("SampleCtrlr", "Enabled", Attribute::Actual, "yes"), SetStatus::Rejected);
//! ```

use alloc::borrow::ToOwned;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

/// Which attribute of a variable is meant.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Attribute {
    /// The value in effect. The default when a request omits the attribute.
    #[default]
    Actual,
    /// The value the operator wants to reach.
    Target,
    /// The lower bound of the accepted range.
    MinSet,
    /// The upper bound of the accepted range.
    MaxSet,
}

impl Attribute {
    /// The wire spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Attribute::Actual => "Actual",
            Attribute::Target => "Target",
            Attribute::MinSet => "MinSet",
            Attribute::MaxSet => "MaxSet",
        }
    }

    /// Parses a wire value.
    #[must_use]
    pub fn from_wire(value: &str) -> Option<Self> {
        Some(match value {
            "Actual" => Attribute::Actual,
            "Target" => Attribute::Target,
            "MinSet" => Attribute::MinSet,
            "MaxSet" => Attribute::MaxSet,
            _ => return None,
        })
    }
}

impl fmt::Display for Attribute {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Whether a variable can be written.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mutability {
    /// Reads only.
    #[default]
    ReadOnly,
    /// Writes only — passwords and keys, which are never reported back.
    WriteOnly,
    /// Reads and writes.
    ReadWrite,
}

impl Mutability {
    /// The wire spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Mutability::ReadOnly => "ReadOnly",
            Mutability::WriteOnly => "WriteOnly",
            Mutability::ReadWrite => "ReadWrite",
        }
    }

    /// Whether the value may be reported.
    #[must_use]
    pub const fn readable(self) -> bool {
        !matches!(self, Mutability::WriteOnly)
    }

    /// Whether the value may be written.
    #[must_use]
    pub const fn writable(self) -> bool {
        !matches!(self, Mutability::ReadOnly)
    }
}

/// The `DataEnumType` of a variable — what its string value has to parse as.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum DataType {
    /// Free text.
    #[default]
    String,
    /// A decimal number.
    Decimal,
    /// A whole number.
    Integer,
    /// An RFC 3339 timestamp.
    DateTime,
    /// `true` or `false`.
    Boolean,
    /// One of `values_list`.
    OptionList,
    /// A comma-separated subset of `values_list`.
    SequenceList,
    /// A comma-separated list of values from `values_list`, in any order.
    MemberList,
}

impl DataType {
    /// The wire spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            DataType::String => "string",
            DataType::Decimal => "decimal",
            DataType::Integer => "integer",
            DataType::DateTime => "dateTime",
            DataType::Boolean => "boolean",
            DataType::OptionList => "OptionList",
            DataType::SequenceList => "SequenceList",
            DataType::MemberList => "MemberList",
        }
    }

    /// Parses a `DataEnumType` value, as the schemas and the appendix spell it.
    #[must_use]
    pub fn from_wire(value: &str) -> Option<Self> {
        Some(match value {
            "string" => DataType::String,
            "decimal" => DataType::Decimal,
            "integer" => DataType::Integer,
            "dateTime" => DataType::DateTime,
            "boolean" => DataType::Boolean,
            "OptionList" => DataType::OptionList,
            "SequenceList" => DataType::SequenceList,
            "MemberList" => DataType::MemberList,
            _ => return None,
        })
    }

    /// Whether `value` is a valid instance of this type, given the declared `values_list`.
    #[must_use]
    pub fn accepts(self, value: &str, values_list: &[String]) -> bool {
        match self {
            DataType::String => true,
            DataType::Boolean => matches!(value, "true" | "false"),
            DataType::Integer => value.parse::<i64>().is_ok(),
            DataType::Decimal => value.parse::<f64>().is_ok_and(f64::is_finite),
            DataType::DateTime => crate::types::DateTime::parse(value).is_ok(),
            DataType::OptionList => {
                values_list.is_empty() || values_list.iter().any(|v| v == value)
            }
            DataType::SequenceList | DataType::MemberList => {
                values_list.is_empty()
                    || value
                        .split(',')
                        .map(str::trim)
                        .all(|item| values_list.iter().any(|v| v == item))
            }
        }
    }
}

/// Identifies one component instance.
#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ComponentKey {
    /// Component name, e.g. `OCPPCommCtrlr`.
    pub name: String,
    /// Instance discriminator, when several instances exist.
    pub instance: Option<String>,
    /// EVSE the component belongs to.
    pub evse: Option<u32>,
    /// Connector within the EVSE.
    pub connector: Option<u32>,
}

impl ComponentKey {
    /// A component with no instance and no EVSE.
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            instance: None,
            evse: None,
            connector: None,
        }
    }

    /// Adds an instance discriminator.
    #[must_use]
    pub fn instance(mut self, instance: impl Into<String>) -> Self {
        self.instance = Some(instance.into());
        self
    }

    /// Binds the component to an EVSE, and optionally to a connector.
    #[must_use]
    pub fn evse(mut self, evse: u32, connector: Option<u32>) -> Self {
        self.evse = Some(evse);
        self.connector = connector;
        self
    }
}

impl From<&str> for ComponentKey {
    fn from(name: &str) -> Self {
        ComponentKey::new(name)
    }
}

impl fmt::Display for ComponentKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.name)?;
        if let Some(instance) = &self.instance {
            write!(f, "({instance})")?;
        }
        if let Some(evse) = self.evse {
            write!(f, "@evse{evse}")?;
            if let Some(connector) = self.connector {
                write!(f, ".{connector}")?;
            }
        }
        Ok(())
    }
}

/// Identifies one variable within a component.
#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct VariableKey {
    /// Variable name, e.g. `HeartbeatInterval`.
    pub name: String,
    /// Instance discriminator.
    pub instance: Option<String>,
}

impl VariableKey {
    /// A variable with no instance.
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            instance: None,
        }
    }

    /// Adds an instance discriminator — how `MessageAttempts[TransactionEvent]` is modelled.
    #[must_use]
    pub fn instance(mut self, instance: impl Into<String>) -> Self {
        self.instance = Some(instance.into());
        self
    }
}

impl From<&str> for VariableKey {
    fn from(name: &str) -> Self {
        VariableKey::new(name)
    }
}

impl fmt::Display for VariableKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.name)?;
        if let Some(instance) = &self.instance {
            write!(f, "[{instance}]")?;
        }
        Ok(())
    }
}

/// How a variable is declared.
#[derive(Clone, Debug)]
pub struct VariableSpec {
    key: VariableKey,
    data_type: DataType,
    mutability: Mutability,
    unit: Option<String>,
    values_list: Vec<String>,
    min_limit: Option<f64>,
    max_limit: Option<f64>,
    persistent: bool,
    constant: bool,
    /// Whether a write only takes effect after a reboot — the 2.x equivalent of 1.6's
    /// `RebootRequired` configuration status.
    reboot_required: bool,
    /// The attribute types this variable supports, whether or not they have a value.
    ///
    /// B07.FR.11: "All attribute types of a variable, that are supported by the Charging
    /// Station, SHALL be reported, **even if they have no value (are unset)**" — so support
    /// and value are two different facts and the model has to keep them apart.
    supported: BTreeSet<Attribute>,
    values: BTreeMap<Attribute, String>,
}

impl VariableSpec {
    /// Declares a variable.
    #[must_use]
    pub fn new(name: impl Into<String>, data_type: DataType) -> Self {
        Self {
            key: VariableKey::new(name),
            data_type,
            mutability: Mutability::ReadOnly,
            unit: None,
            values_list: Vec::new(),
            min_limit: None,
            max_limit: None,
            persistent: true,
            constant: false,
            reboot_required: false,
            // Every variable has an Actual; the rest are declared as they are added.
            supported: BTreeSet::from([Attribute::Actual]),
            values: BTreeMap::new(),
        }
    }

    /// Adds an instance discriminator.
    #[must_use]
    pub fn instance(mut self, instance: impl Into<String>) -> Self {
        self.key.instance = Some(instance.into());
        self
    }

    /// Sets whether the variable can be written.
    #[must_use]
    pub fn mutability(mut self, mutability: Mutability) -> Self {
        self.mutability = mutability;
        self
    }

    /// Sets the unit, e.g. `s` or `Wh`.
    #[must_use]
    pub fn unit(mut self, unit: impl Into<String>) -> Self {
        self.unit = Some(unit.into());
        self
    }

    /// Restricts the accepted values.
    #[must_use]
    pub fn values(mut self, values: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.values_list = values.into_iter().map(Into::into).collect();
        self
    }

    /// Restricts a numeric variable to a range.
    #[must_use]
    pub fn limits(mut self, min: Option<f64>, max: Option<f64>) -> Self {
        self.min_limit = min;
        self.max_limit = max;
        self
    }

    /// Marks the variable as one a write only takes effect on after a reboot.
    #[must_use]
    pub fn reboot_required(mut self) -> Self {
        self.reboot_required = true;
        self
    }

    /// Marks the variable as constant — reported, never written, never persisted.
    #[must_use]
    pub fn constant(mut self) -> Self {
        self.constant = true;
        self.mutability = Mutability::ReadOnly;
        self
    }

    /// Sets the `Actual` value.
    #[must_use]
    pub fn value(mut self, value: impl Into<String>) -> Self {
        self.values.insert(Attribute::Actual, value.into());
        self
    }

    /// Sets one attribute's value, declaring it supported.
    #[must_use]
    pub fn attribute(mut self, attribute: Attribute, value: impl Into<String>) -> Self {
        self.supported.insert(attribute);
        self.values.insert(attribute, value.into());
        self
    }

    /// Declares an attribute type supported without giving it a value (B07.FR.11).
    ///
    /// A `MaxSet` a station honours but has not been given is still something the CSMS needs
    /// to know it may write.
    #[must_use]
    pub fn supports(mut self, attribute: Attribute) -> Self {
        self.supported.insert(attribute);
        self
    }

    /// The attribute types this variable supports.
    pub fn supported(&self) -> impl Iterator<Item = Attribute> + '_ {
        self.supported.iter().copied()
    }
}

/// The result of a `GetVariables` entry (`GetVariableStatusEnumType`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum GetStatus {
    /// The value is returned.
    Accepted,
    /// No such component.
    UnknownComponent,
    /// No such variable in that component.
    UnknownVariable,
    /// The variable exists but not that attribute.
    NotSupportedAttributeType,
    /// The variable is write-only.
    Rejected,
}

/// The result of a `SetVariables` entry (`SetVariableStatusEnumType`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum SetStatus {
    /// Stored and in effect.
    Accepted,
    /// Stored, but only in effect after a reboot.
    RebootRequired,
    /// No such component.
    UnknownComponent,
    /// No such variable in that component.
    UnknownVariable,
    /// The variable exists but not that attribute.
    NotSupportedAttributeType,
    /// The variable is read-only, or the value does not satisfy its declared type or limits.
    Rejected,
}

/// One line of a `NotifyReport` / `GetBaseReport` inventory.
#[derive(Clone, Debug, PartialEq)]
pub struct ReportDatum {
    /// Which component.
    pub component: ComponentKey,
    /// Which variable.
    pub variable: VariableKey,
    /// The variable's attributes and their values. A write-only variable reports its
    /// attributes with no value.
    pub attributes: Vec<(Attribute, Mutability, Option<String>)>,
    /// Whether the variable can be written.
    pub mutability: Mutability,
    /// The declared type.
    pub data_type: DataType,
    /// The unit, if any.
    pub unit: Option<String>,
    /// The accepted values, if restricted.
    pub values_list: Vec<String>,
    /// The lower numeric bound, if any.
    pub min_limit: Option<f64>,
    /// The upper numeric bound, if any.
    pub max_limit: Option<f64>,
    /// Whether the value survives a reboot.
    pub persistent: bool,
    /// Whether the value can never change.
    pub constant: bool,
}

/// Which slice of the inventory a report covers (`ReportBaseEnumType`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ReportBase {
    /// Every component, variable and attribute, with all characteristics.
    FullInventory,
    /// Only what the Charging Station supports configuring.
    ConfigurationInventory,
    /// Only what is needed to summarise the station.
    SummaryInventory,
}

/// The registry of components and variables.
#[derive(Clone, Debug, Default)]
pub struct DeviceModel {
    components: BTreeMap<ComponentKey, BTreeMap<VariableKey, VariableSpec>>,
}

impl DeviceModel {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A registry pre-populated with the standard controllers and the variables an OCPP 2.x
    /// Charging Station must expose to pass the Core certification profile.
    ///
    /// Values are placeholders; override them with [`declare`](Self::declare).
    #[must_use]
    pub fn with_defaults() -> Self {
        let mut model = Self::new();
        for (component, spec) in defaults() {
            model.declare(component, spec);
        }
        model
    }

    /// Declares every variable the specification associates with one standardized component.
    ///
    /// The appendix's list is a *vocabulary*, not a requirement — a Charging Station
    /// implements the part of it that it actually has. Declaring a component from the
    /// catalogue gets the names, data types and units right for free; override the ones you
    /// mean to support with [`declare`](Self::declare) and remove the rest.
    ///
    /// Returns how many variables were declared, or `None` if the component is not one the
    /// specification standardizes.
    ///
    /// The appendix gives names, data types and units. It does *not* give mutability, so a
    /// variable declared this way starts read-only until you say otherwise.
    ///
    /// ```
    /// use ocpp_kit::station::device_model::{
    ///     Attribute, DataType, DeviceModel, Mutability, SetStatus, VariableSpec,
    /// };
    ///
    /// let mut model = DeviceModel::new();
    /// let declared = model.declare_standard("OCPPCommCtrlr").expect("a standard component");
    /// assert!(declared > 0);
    ///
    /// // Supported but not yet set, so it reads back empty (B06.FR.13) and refuses a write.
    /// assert_eq!(
    ///     model.get("OCPPCommCtrlr", "HeartbeatInterval", Attribute::Actual),
    ///     Ok(String::new()),
    /// );
    /// assert_eq!(
    ///     model.set("OCPPCommCtrlr", "HeartbeatInterval", Attribute::Actual, "300"),
    ///     SetStatus::Rejected,
    /// );
    ///
    /// // Say which ones the station actually lets the CSMS write.
    /// model.declare(
    ///     "OCPPCommCtrlr",
    ///     VariableSpec::new("HeartbeatInterval", DataType::Integer)
    ///         .mutability(Mutability::ReadWrite)
    ///         .value("300"),
    /// );
    /// assert_eq!(
    ///     model.set("OCPPCommCtrlr", "HeartbeatInterval", Attribute::Actual, "600"),
    ///     SetStatus::Accepted,
    /// );
    /// // …and the declared type is still enforced.
    /// assert_eq!(
    ///     model.set("OCPPCommCtrlr", "HeartbeatInterval", Attribute::Actual, "soon"),
    ///     SetStatus::Rejected,
    /// );
    /// ```
    pub fn declare_standard(&mut self, component: &str) -> Option<usize> {
        let standard = crate::standard::components::component(component)?;
        let key = ComponentKey::new(standard.name);
        let mut declared = 0;
        for variable in standard.variables {
            let data_type = variable
                .data_type
                .and_then(DataType::from_wire)
                .unwrap_or(DataType::String);
            let mut spec = VariableSpec::new(variable.name, data_type);
            if let Some(instance) = variable.instance {
                spec = spec.instance(instance);
            }
            if let Some(unit) = variable.unit {
                spec = spec.unit(unit);
            }
            // The appendix's `(Attribute)` suffix names an attribute of the variable, not a
            // variable of its own, so it becomes a *supported* attribute rather than a new
            // entry. Supported and unset, not supported with an empty value: B07.FR.11 keeps
            // those apart, `report` lists what is supported, and B06.FR.13 makes an unset one
            // read back as an empty string anyway.
            match variable.attribute.and_then(Attribute::from_wire) {
                Some(attribute) if attribute != Attribute::Actual => {
                    if let Some(existing) = self
                        .components
                        .get_mut(&key)
                        .and_then(|variables| variables.get_mut(&spec.key))
                    {
                        existing.supported.insert(attribute);
                        continue;
                    }
                    spec = spec.supports(attribute);
                }
                _ => {}
            }
            self.declare(key.clone(), spec);
            declared += 1;
        }
        Some(declared)
    }

    /// Declares (or replaces) a variable.
    pub fn declare(&mut self, component: impl Into<ComponentKey>, spec: VariableSpec) {
        self.components
            .entry(component.into())
            .or_default()
            .insert(spec.key.clone(), spec);
    }

    /// How many variables are declared.
    #[must_use]
    pub fn len(&self) -> usize {
        self.components.values().map(BTreeMap::len).sum()
    }

    /// Whether the registry is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Every declared component.
    pub fn components(&self) -> impl Iterator<Item = &ComponentKey> {
        self.components.keys()
    }

    /// Reads one attribute.
    pub fn get(
        &self,
        component: impl Into<ComponentKey>,
        variable: impl Into<VariableKey>,
        attribute: Attribute,
    ) -> Result<String, GetStatus> {
        let component = component.into();
        let variable = variable.into();
        let Some(variables) = self.components.get(&component) else {
            return Err(GetStatus::UnknownComponent);
        };
        let Some(spec) = variables.get(&variable) else {
            return Err(GetStatus::UnknownVariable);
        };
        // B06.FR.09 — a WriteOnly variable is `Rejected`, not returned.
        if !spec.mutability.readable() {
            return Err(GetStatus::Rejected);
        }
        // B06.FR.08 reserves `NotSupportedAttributeType` for an attribute type that is
        // *unknown* for this variable. Support and value are two facts (B07.FR.11), so an
        // attribute that is supported but unset is not "not supported".
        if !spec.supported.contains(&attribute) {
            return Err(GetStatus::NotSupportedAttributeType);
        }
        // B06.FR.13 names this case exactly: "the Charging Station has no attributeValue for
        // the requested attributeType … SHALL return an empty string as attributeValue. Note:
        // this can happen, for example, when the attributeType Target has not yet been set,
        // even though it is supported."
        Ok(spec.values.get(&attribute).cloned().unwrap_or_default())
    }

    /// Writes one attribute, enforcing mutability, the declared type and the declared limits.
    pub fn set(
        &mut self,
        component: impl Into<ComponentKey>,
        variable: impl Into<VariableKey>,
        attribute: Attribute,
        value: &str,
    ) -> SetStatus {
        let component = component.into();
        let variable = variable.into();
        let Some(variables) = self.components.get_mut(&component) else {
            return SetStatus::UnknownComponent;
        };
        let Some(spec) = variables.get_mut(&variable) else {
            return SetStatus::UnknownVariable;
        };
        if !spec.mutability.writable() || spec.constant {
            return SetStatus::Rejected;
        }
        // B05.FR.06 — as for reads, `NotSupportedAttributeType` means the attribute type is
        // unknown for this variable, not that it happens to have no value yet.
        if !spec.supported.contains(&attribute) {
            return SetStatus::NotSupportedAttributeType;
        }
        if !spec.data_type.accepts(value, &spec.values_list) {
            return SetStatus::Rejected;
        }
        if matches!(spec.data_type, DataType::Integer | DataType::Decimal) {
            if let Ok(number) = value.parse::<f64>() {
                if spec.min_limit.is_some_and(|min| number < min)
                    || spec.max_limit.is_some_and(|max| number > max)
                {
                    return SetStatus::Rejected;
                }
            }
        }
        spec.values.insert(attribute, value.to_owned());
        if spec.reboot_required {
            SetStatus::RebootRequired
        } else {
            SetStatus::Accepted
        }
    }

    /// Builds an inventory report.
    ///
    /// `component_filter` and `variable_filter` implement `GetReport`'s
    /// `componentVariable` criteria; pass `None` for `GetBaseReport`.
    #[must_use]
    pub fn report(
        &self,
        base: ReportBase,
        component_filter: Option<&str>,
        variable_filter: Option<&str>,
    ) -> Vec<ReportDatum> {
        let mut out = Vec::new();
        for (component, variables) in &self.components {
            if component_filter.is_some_and(|name| name != component.name) {
                continue;
            }
            for (variable, spec) in variables {
                if variable_filter.is_some_and(|name| name != variable.name) {
                    continue;
                }
                let include = match base {
                    ReportBase::FullInventory => true,
                    // B07.FR.07: "all component-variables that can be set by the operator".
                    ReportBase::ConfigurationInventory => spec.mutability.writable(),
                    // B07.FR.09 is specific, and it is not "the interesting ones": the
                    // availability of the Charging Station, its EVSEs and its Connectors,
                    // plus the condition variables of anything in an abnormal state.
                    ReportBase::SummaryInventory => is_summary_variable(&variable.name),
                };
                if !include {
                    continue;
                }
                out.push(ReportDatum {
                    component: component.clone(),
                    variable: variable.clone(),
                    // B07.FR.11: every *supported* attribute type is reported, with or
                    // without a value; B07.FR.03: a WriteOnly variable's value is not.
                    attributes: spec
                        .supported
                        .iter()
                        .map(|attribute| {
                            let value = spec
                                .mutability
                                .readable()
                                .then(|| spec.values.get(attribute).cloned())
                                .flatten();
                            (*attribute, spec.mutability, value)
                        })
                        .collect(),
                    mutability: spec.mutability,
                    data_type: spec.data_type,
                    unit: spec.unit.clone(),
                    values_list: spec.values_list.clone(),
                    min_limit: spec.min_limit,
                    max_limit: spec.max_limit,
                    persistent: spec.persistent,
                    constant: spec.constant,
                });
            }
        }
        out
    }

    /// Splits a report into `NotifyReport`-sized pages.
    ///
    /// A full inventory does not fit in one message, so 2.x pages it with `seqNo` and `tbc`.
    /// Each returned page is `(seq_no, to_be_continued, data)`.
    #[must_use]
    pub fn paginate(data: &[ReportDatum], per_page: usize) -> Vec<(i32, bool, Vec<ReportDatum>)> {
        let per_page = per_page.max(1);
        let total = data.len().div_ceil(per_page).max(1);
        let mut pages = Vec::with_capacity(total);
        let mut chunks = data.chunks(per_page).peekable();
        let mut seq = 0;
        if chunks.peek().is_none() {
            return alloc::vec![(0, false, Vec::new())];
        }
        while let Some(chunk) = chunks.next() {
            pages.push((seq, chunks.peek().is_some(), chunk.to_vec()));
            seq += 1;
        }
        pages
    }
}

/// Whether a variable belongs in a `SummaryInventory` report (B07.FR.09).
///
/// The specification enumerates them: `AvailabilityState` for the Charging Station, each EVSE
/// and each Connector, and — "for all Components in an abnormal State" — the `Active`,
/// `Problem`, `Tripped`, `Overload` and `Fallback` variables. A summary that instead guessed
/// from mutability would report the heartbeat interval and omit the one thing the CSMS asked
/// for.
fn is_summary_variable(name: &str) -> bool {
    matches!(
        name,
        "AvailabilityState" | "Active" | "Problem" | "Tripped" | "Overload" | "Fallback"
    )
}

/// The standard controllers and the variables a Core-profile station must expose.
///
/// Trimmed to the ones the protocol itself reads — the engine's timeouts, the retry
/// schedule, the heartbeat interval, the security profile — plus the identifying constants.
#[allow(clippy::too_many_lines)]
fn defaults() -> Vec<(ComponentKey, VariableSpec)> {
    use DataType::{Boolean, Integer, String as Str};
    let seconds = |name: &str| VariableSpec::new(name, Integer).unit("s");
    alloc::vec![
        // --- OCPPCommCtrlr: everything the RPC layer is parameterised by ----------
        (
            ComponentKey::new("OCPPCommCtrlr"),
            seconds("HeartbeatInterval")
                .mutability(Mutability::ReadWrite)
                .value("300"),
        ),
        (
            ComponentKey::new("OCPPCommCtrlr"),
            seconds("MessageTimeout").instance("Default").value("30"),
        ),
        (
            ComponentKey::new("OCPPCommCtrlr"),
            VariableSpec::new("MessageAttempts", Integer)
                .instance("TransactionEvent")
                .mutability(Mutability::ReadWrite)
                .value("3"),
        ),
        (
            ComponentKey::new("OCPPCommCtrlr"),
            seconds("MessageAttemptInterval")
                .instance("TransactionEvent")
                .mutability(Mutability::ReadWrite)
                .value("60"),
        ),
        (
            ComponentKey::new("OCPPCommCtrlr"),
            seconds("WebSocketPingInterval")
                .mutability(Mutability::ReadWrite)
                .value("60"),
        ),
        (
            ComponentKey::new("OCPPCommCtrlr"),
            seconds("RetryBackOffWaitMinimum")
                .mutability(Mutability::ReadWrite)
                .value("10"),
        ),
        (
            ComponentKey::new("OCPPCommCtrlr"),
            seconds("RetryBackOffRandomRange")
                .mutability(Mutability::ReadWrite)
                .value("10"),
        ),
        (
            ComponentKey::new("OCPPCommCtrlr"),
            VariableSpec::new("RetryBackOffRepeatTimes", Integer)
                .mutability(Mutability::ReadWrite)
                .value("3"),
        ),
        (
            ComponentKey::new("OCPPCommCtrlr"),
            seconds("OfflineThreshold")
                .mutability(Mutability::ReadWrite)
                .value("600"),
        ),
        (
            ComponentKey::new("OCPPCommCtrlr"),
            VariableSpec::new("QueueAllMessages", Boolean)
                .mutability(Mutability::ReadWrite)
                .value("false"),
        ),
        (
            ComponentKey::new("OCPPCommCtrlr"),
            VariableSpec::new("NetworkConfigurationPriority", Str)
                .mutability(Mutability::ReadWrite)
                .value("0"),
        ),
        (
            ComponentKey::new("OCPPCommCtrlr"),
            VariableSpec::new("NetworkProfileConnectionAttempts", Integer)
                .mutability(Mutability::ReadWrite)
                .value("3"),
        ),
        // --- SecurityCtrlr -------------------------------------------------------
        (
            ComponentKey::new("SecurityCtrlr"),
            VariableSpec::new("SecurityProfile", Integer)
                .limits(Some(1.0), Some(3.0))
                .value("1"),
        ),
        (
            ComponentKey::new("SecurityCtrlr"),
            VariableSpec::new("BasicAuthPassword", Str).mutability(Mutability::WriteOnly),
        ),
        (
            ComponentKey::new("SecurityCtrlr"),
            VariableSpec::new("Identity", Str).constant(),
        ),
        (
            ComponentKey::new("SecurityCtrlr"),
            VariableSpec::new("OrganizationName", Str).mutability(Mutability::ReadWrite),
        ),
        // --- TxCtrlr -------------------------------------------------------------
        (
            ComponentKey::new("TxCtrlr"),
            VariableSpec::new("TxStartPoint", DataType::MemberList)
                .mutability(Mutability::ReadWrite)
                .values([
                    "ParkingBayOccupancy",
                    "EVConnected",
                    "Authorized",
                    "DataSigned",
                    "PowerPathClosed",
                    "EnergyTransfer"
                ])
                .value("PowerPathClosed"),
        ),
        (
            ComponentKey::new("TxCtrlr"),
            VariableSpec::new("TxStopPoint", DataType::MemberList)
                .mutability(Mutability::ReadWrite)
                .values([
                    "ParkingBayOccupancy",
                    "EVConnected",
                    "Authorized",
                    "DataSigned",
                    "PowerPathClosed",
                    "EnergyTransfer"
                ])
                .value("EVConnected"),
        ),
        (
            ComponentKey::new("TxCtrlr"),
            VariableSpec::new("StopTxOnInvalidId", Boolean)
                .mutability(Mutability::ReadWrite)
                .value("true"),
        ),
        // --- AuthCtrlr / AuthCacheCtrlr / LocalAuthListCtrlr ---------------------
        (
            ComponentKey::new("AuthCtrlr"),
            VariableSpec::new("AuthorizeRemoteStart", Boolean)
                .mutability(Mutability::ReadWrite)
                .value("false"),
        ),
        (
            ComponentKey::new("AuthCtrlr"),
            VariableSpec::new("LocalPreAuthorize", Boolean)
                .mutability(Mutability::ReadWrite)
                .value("false"),
        ),
        (
            ComponentKey::new("AuthCacheCtrlr"),
            VariableSpec::new("Enabled", Boolean)
                .mutability(Mutability::ReadWrite)
                .value("true"),
        ),
        (
            ComponentKey::new("LocalAuthListCtrlr"),
            VariableSpec::new("Enabled", Boolean)
                .mutability(Mutability::ReadWrite)
                .value("true"),
        ),
        (
            ComponentKey::new("LocalAuthListCtrlr"),
            VariableSpec::new("Entries", Integer).value("0"),
        ),
        // --- ClockCtrlr ----------------------------------------------------------
        (
            ComponentKey::new("ClockCtrlr"),
            VariableSpec::new("TimeSource", DataType::SequenceList)
                .mutability(Mutability::ReadWrite)
                .values([
                    "Heartbeat",
                    "NTP",
                    "GPS",
                    "RealTimeClock",
                    "MobileNetwork",
                    "RadioTimeTransmitter"
                ])
                .value("Heartbeat"),
        ),
        (
            ComponentKey::new("ClockCtrlr"),
            VariableSpec::new("DateTime", DataType::DateTime),
        ),
        // --- DeviceDataCtrlr -----------------------------------------------------
        (
            ComponentKey::new("DeviceDataCtrlr"),
            VariableSpec::new("ItemsPerMessage", Integer)
                .instance("GetReport")
                .value("25"),
        ),
        (
            ComponentKey::new("DeviceDataCtrlr"),
            VariableSpec::new("BytesPerMessage", Integer)
                .instance("GetReport")
                .value("65536"),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mutability_and_type_are_enforced_on_write() {
        let mut model = DeviceModel::with_defaults();
        assert_eq!(
            model.set(
                "OCPPCommCtrlr",
                "HeartbeatInterval",
                Attribute::Actual,
                "600"
            ),
            SetStatus::Accepted
        );
        assert_eq!(
            model
                .get("OCPPCommCtrlr", "HeartbeatInterval", Attribute::Actual)
                .unwrap(),
            "600"
        );

        // Not an integer.
        assert_eq!(
            model.set(
                "OCPPCommCtrlr",
                "HeartbeatInterval",
                Attribute::Actual,
                "soon"
            ),
            SetStatus::Rejected
        );
        // Read-only.
        assert_eq!(
            model.set("SecurityCtrlr", "Identity", Attribute::Actual, "CS-1"),
            SetStatus::Rejected
        );
        // Outside the declared limits.
        assert_eq!(
            model.set("SecurityCtrlr", "SecurityProfile", Attribute::Actual, "9"),
            SetStatus::Rejected
        );
        assert_eq!(
            model.set("Nope", "Whatever", Attribute::Actual, "1"),
            SetStatus::UnknownComponent
        );
        assert_eq!(
            model.set("SecurityCtrlr", "Whatever", Attribute::Actual, "1"),
            SetStatus::UnknownVariable
        );
    }

    /// B07.FR.11 makes "supported" and "has a value" two different facts, and the read and
    /// write paths have to agree with the report about which is which. B05.FR.06 and
    /// B06.FR.08 both reserve `NotSupportedAttributeType` for an attribute type that is
    /// *unknown* for the variable; B06.FR.13 says a supported attribute with no value yet
    /// reads back as an empty string — naming `Target` as the example.
    #[test]
    fn a_supported_attribute_with_no_value_is_readable_and_writable_b06_fr_13() {
        let mut model = DeviceModel::new();
        model.declare(
            "OCPPCommCtrlr",
            VariableSpec::new("HeartbeatInterval", DataType::Integer)
                .mutability(Mutability::ReadWrite)
                .value("300")
                .supports(Attribute::Target),
        );

        // The report says `Target` is supported and has no value.
        let data = model.report(ReportBase::FullInventory, None, None);
        let entry = data
            .iter()
            .find(|d| d.variable.name == "HeartbeatInterval")
            .expect("declared");
        let target = entry
            .attributes
            .iter()
            .find(|(attribute, _, _)| *attribute == Attribute::Target)
            .expect("B07.FR.11: a supported attribute is reported even when unset");
        assert_eq!(target.2, None, "…and it is reported without a value");

        // So reading it must not claim it is unsupported (B06.FR.08), and B06.FR.13 makes
        // the answer an empty string rather than an error.
        assert_eq!(
            model.get("OCPPCommCtrlr", "HeartbeatInterval", Attribute::Target),
            Ok(String::new())
        );

        // And writing it must not claim it is unsupported either (B05.FR.06).
        assert_eq!(
            model.set(
                "OCPPCommCtrlr",
                "HeartbeatInterval",
                Attribute::Target,
                "60"
            ),
            SetStatus::Accepted
        );
        assert_eq!(
            model.get("OCPPCommCtrlr", "HeartbeatInterval", Attribute::Target),
            Ok("60".to_owned())
        );

        // An attribute type the variable never declared is the case those two rules are for.
        assert_eq!(
            model.get("OCPPCommCtrlr", "HeartbeatInterval", Attribute::MinSet),
            Err(GetStatus::NotSupportedAttributeType)
        );
        assert_eq!(
            model.set("OCPPCommCtrlr", "HeartbeatInterval", Attribute::MinSet, "1"),
            SetStatus::NotSupportedAttributeType
        );
    }

    #[test]
    fn a_write_only_variable_is_never_read_back() {
        let mut model = DeviceModel::with_defaults();
        assert_eq!(
            model.set(
                "SecurityCtrlr",
                "BasicAuthPassword",
                Attribute::Actual,
                "0123456789abcdef"
            ),
            SetStatus::Accepted
        );
        assert_eq!(
            model.get("SecurityCtrlr", "BasicAuthPassword", Attribute::Actual),
            Err(GetStatus::Rejected)
        );
        let report = model.report(
            ReportBase::FullInventory,
            Some("SecurityCtrlr"),
            Some("BasicAuthPassword"),
        );
        assert_eq!(
            report[0].attributes[0].2, None,
            "the value is withheld from reports too"
        );
    }

    #[test]
    fn instances_distinguish_variables_of_the_same_name() {
        let model = DeviceModel::with_defaults();
        let attempts = model
            .get(
                "OCPPCommCtrlr",
                VariableKey::new("MessageAttempts").instance("TransactionEvent"),
                Attribute::Actual,
            )
            .unwrap();
        assert_eq!(attempts, "3");
        assert_eq!(
            model.get("OCPPCommCtrlr", "MessageAttempts", Attribute::Actual),
            Err(GetStatus::UnknownVariable)
        );
    }

    #[test]
    fn a_configuration_inventory_reports_only_writable_variables() {
        let model = DeviceModel::with_defaults();
        let full = model.report(ReportBase::FullInventory, None, None);
        let config = model.report(ReportBase::ConfigurationInventory, None, None);
        assert!(config.len() < full.len());
        assert!(config.iter().all(|datum| datum.mutability.writable()));
    }

    #[test]
    fn the_standard_catalogue_declares_a_component_with_its_types_and_units() {
        let mut model = DeviceModel::new();
        let declared = model
            .declare_standard("OCPPCommCtrlr")
            .expect("a standard component");
        assert!(
            declared >= 15,
            "the appendix lists at least 15 OCPPCommCtrlr variables"
        );

        // Types and units come from the appendix, not from a hand-written table.
        let report = model.report(
            ReportBase::FullInventory,
            Some("OCPPCommCtrlr"),
            Some("HeartbeatInterval"),
        );
        assert_eq!(report[0].data_type, DataType::Integer);
        assert_eq!(report[0].unit.as_deref(), Some("s"));

        assert!(model.declare_standard("NotAComponent").is_none());
    }

    #[test]
    fn a_physical_component_gets_its_types_from_the_standardized_variable_list() {
        let mut model = DeviceModel::new();
        model
            .declare_standard("EVSE")
            .expect("EVSE is standardized");
        // The physical-component tables have no Type column; chapter 4 supplies it.
        let report = model.report(ReportBase::FullInventory, Some("EVSE"), Some("ACVoltage"));
        assert_eq!(report[0].data_type, DataType::Decimal);
        assert_eq!(report[0].unit.as_deref(), Some("V"));
    }

    /// B07.FR.09 names exactly what a summary contains: availability and condition. Choosing
    /// by mutability instead — as this once did — reports `HeartbeatInterval` and omits
    /// `AvailabilityState`, which is the one thing the CSMS asked for.
    #[test]
    fn a_summary_inventory_reports_availability_and_condition_b07_fr_09() {
        let mut model = DeviceModel::new();
        model.declare(
            ComponentKey::new("ChargingStation"),
            VariableSpec::new("AvailabilityState", DataType::OptionList).value("Available"),
        );
        model.declare(
            ComponentKey::new("Connector").evse(1, Some(1)),
            VariableSpec::new("Problem", DataType::Boolean).value("false"),
        );
        model.declare(
            ComponentKey::new("OCPPCommCtrlr"),
            VariableSpec::new("HeartbeatInterval", DataType::Integer)
                .mutability(Mutability::ReadWrite)
                .value("300"),
        );

        let summary = model.report(ReportBase::SummaryInventory, None, None);
        let names: Vec<&str> = summary
            .iter()
            .map(|datum| datum.variable.name.as_str())
            .collect();
        assert!(names.contains(&"AvailabilityState"), "{names:?}");
        assert!(names.contains(&"Problem"), "{names:?}");
        assert!(
            !names.contains(&"HeartbeatInterval"),
            "a configuration variable is not a summary: {names:?}"
        );

        // …and it is still in the full and configuration inventories.
        let full = model.report(ReportBase::FullInventory, None, None);
        assert_eq!(full.len(), 3);
    }

    /// B07.FR.11: "All attribute types of a variable, that are supported by the Charging
    /// Station, SHALL be reported, even if they have no value (are unset)." Reporting only
    /// the ones that happen to hold a value tells the CSMS a `MaxSet` it may write does not
    /// exist.
    #[test]
    fn a_supported_but_unset_attribute_is_still_reported_b07_fr_11() {
        let mut model = DeviceModel::new();
        model.declare(
            ComponentKey::new("SmartChargingCtrlr"),
            VariableSpec::new("Entries", DataType::Integer)
                .mutability(Mutability::ReadWrite)
                .value("10")
                .supports(Attribute::MaxSet),
        );

        let report = model.report(ReportBase::FullInventory, None, None);
        let attributes = &report[0].attributes;
        let max_set = attributes
            .iter()
            .find(|(attribute, _, _)| *attribute == Attribute::MaxSet)
            .expect("MaxSet is supported, so it is reported");
        assert_eq!(max_set.2, None, "…with no value, which is the point");
        assert!(
            attributes
                .iter()
                .any(|(attribute, _, value)| *attribute == Attribute::Actual
                    && value.as_deref() == Some("10"))
        );
    }

    #[test]
    fn reports_are_paginated_with_a_to_be_continued_flag() {
        let model = DeviceModel::with_defaults();
        let pages = DeviceModel::paginate(&model.report(ReportBase::FullInventory, None, None), 10);
        assert!(pages.len() > 1);
        assert_eq!(pages[0].0, 0);
        assert!(pages[0].1, "every page but the last is `tbc`");
        assert!(!pages.last().unwrap().1);
        let total: usize = pages.iter().map(|(_, _, data)| data.len()).sum();
        assert_eq!(
            total,
            model.report(ReportBase::FullInventory, None, None).len()
        );
    }
}
