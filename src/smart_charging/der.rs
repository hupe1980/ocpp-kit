//! DER control (OCPP 2.1 block R): what a station reports, which control is in effect, and its
//! alarm.
//!
//! A bidirectional station is a distributed energy resource, and a network operator's grid code
//! reaches it as DER controls — volt-watt and frequency-watt curves, a frequency droop, trip
//! curves, a cap on discharge (block R §2.7). The station executes them; this module does not.
//! What a planner needs is to **read** them: a discharge that a DER control can lower or stop
//! is a discharge it must not count on. Three things make that easy to get wrong from the
//! generated types alone, and each is handled here:
//!
//! * **Which control is in effect is a rule, not a field** (§2.9, R04.FR.09–11). Per control
//!   type, a scheduled control between its `startTime` and `startTime + duration` overrides the
//!   default; among several, the lowest `priority` value wins; a superseded control is never
//!   executed. A report lists all of them. [`DerControls::effective_at`] applies the rule.
//! * **A report arrives in parts** (R04.FR.31/32): one `ReportDERControlRequest` per part, each
//!   with the request's `requestId`, `tbc` on all but the last. [`DerReport`] collects exactly
//!   one report and refuses a stray part, a part after the last, and a report that keeps
//!   growing.
//! * **An alarm has one level** (R05.FR.03/04): when control B takes over from A the station
//!   reports B and never reports A's end. [`DerAlarmState`] holds one alarm, not a set.
//!
//! [`Effect`] classifies every control type by what it can do to active power, and
//! [`DerControls::limiting_at`] answers the planner's question. A control type the crate does
//! not know counts as limiting: assuming it harmless is the one guess that can cost a planned
//! discharge.
//!
//! [`check`] refuses a `SetDERControlRequest` the station must reject (R04.FR.13, 15, 17 and
//! 50–56), before it is sent. The settings themselves stay the generated types — they *are* the
//! setting, field for field, with exact decimals.
//!
//! # What is not here
//!
//! Executing curves, trips and ramps: that needs grid measurements this crate never sees, and it
//! is the inverter's job. And no watt figure from `LimitMaxDischarge.pctMaxDischargePower`: the
//! specification gives the field two readings — §2.64 makes it the threshold above which the
//! power-monitoring trip curve applies, use cases R01/R02 use it as the discharge cap itself — so
//! any number computed from it is a guess about which reading the station took.
//!
//! ```
//! use ocpp_kit::smart_charging::der::{DerReport, Effect};
//! use ocpp_kit::types::DateTime;
//! use ocpp_kit::v2_1::{DERControl, ReportDERControlRequest};
//!
//! // `GetDERControl { requestId: 7 }` was answered in two parts.
//! let first: ReportDERControlRequest = serde_json::from_str(r#"{
//!     "requestId": 7, "tbc": true,
//!     "curve": [
//!         {"id": "vw-default", "curveType": "VoltWatt", "isDefault": true, "isSuperseded": false,
//!          "curve": {"priority": 1, "yUnit": "PctMaxW",
//!                    "curveData": [{"x": 106, "y": 100}, {"x": 110, "y": 0}]}},
//!         {"id": "vw-event", "curveType": "VoltWatt", "isDefault": false, "isSuperseded": false,
//!          "curve": {"priority": 0, "yUnit": "PctMaxW", "startTime": "2026-11-20T17:00:00Z",
//!                    "duration": 3600, "curveData": [{"x": 104, "y": 100}, {"x": 108, "y": 0}]}}
//!     ]}"#)?;
//! let last: ReportDERControlRequest = serde_json::from_str(r#"{
//!     "requestId": 7,
//!     "fixedPFInject": [
//!         {"id": "pf", "isDefault": true, "isSuperseded": false,
//!          "fixedPF": {"priority": 0, "displacement": 0.95, "excitation": false}}
//!     ]}"#)?;
//!
//! let mut report = DerReport::new(7);
//! report.add(&first)?;
//! report.add(&last)?;
//! let controls = report.finish()?;
//!
//! let evening: DateTime = "2026-11-20T17:30:00Z".parse()?;
//! let volt_watt = controls.effective(&DERControl::VoltWatt, evening).unwrap();
//! assert_eq!(volt_watt.id, "vw-event"); // scheduled beats default while it runs
//! assert_eq!(volt_watt.effect(), Effect::Lowers);
//!
//! // The power factor is in effect too, but it only takes reactive headroom.
//! let limiting: Vec<_> = controls.limiting_at(evening).iter().map(|c| c.id.as_str()).collect();
//! assert_eq!(limiting, ["vw-event"]);
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

use crate::types::{DateTime, Decimal};
use crate::v2_1::{self, DERControl, DERUnit};

/// What a control type can do to the active power a station charges or discharges with.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Effect {
    /// Lowers active power: `FreqDroop`, `FreqWatt` (P(f)), `VoltWatt` (P(U)),
    /// `LimitMaxDischarge` (Table 161: "Limit Active Power").
    Lowers,
    /// Stops it: every trip and momentary-cessation curve, and `PowerMonitoringMustTrip`. A trip
    /// curve says "after how many seconds a DER must disconnect" (§2.7).
    Stops,
    /// Slows changes to it: `Gradients` (ramp rates) and `EnterService` (delay and ramp after a
    /// trip).
    Slows,
    /// Only reactive power: `VoltVar`, `WattVar`, `WattPF`, `FixedPFAbsorb`, `FixedPFInject`,
    /// `FixedVar`. Whether reactive power costs active headroom depends on the inverter's
    /// apparent-power rating, which OCPP does not carry, so these are not counted as limiting.
    Reactive,
    /// A control type this crate does not know, kept by lenient decoding.
    Unknown,
}

impl Effect {
    /// Whether a control with this effect can lower or stop active power — the controls a planner
    /// must not plan around. `Unknown` counts: assuming it harmless is the costly guess.
    #[must_use]
    pub const fn limits_active_power(self) -> bool {
        matches!(self, Self::Lowers | Self::Stops | Self::Unknown)
    }
}

/// What a control type can do to active power.
#[must_use]
pub fn effect(kind: &DERControl) -> Effect {
    match kind {
        DERControl::FreqDroop
        | DERControl::FreqWatt
        | DERControl::VoltWatt
        | DERControl::LimitMaxDischarge => Effect::Lowers,
        DERControl::HFMustTrip
        | DERControl::HFMayTrip
        | DERControl::HVMustTrip
        | DERControl::HVMomCess
        | DERControl::HVMayTrip
        | DERControl::LFMustTrip
        | DERControl::LVMustTrip
        | DERControl::LVMomCess
        | DERControl::LVMayTrip
        | DERControl::PowerMonitoringMustTrip => Effect::Stops,
        DERControl::Gradients | DERControl::EnterService => Effect::Slows,
        DERControl::VoltVar
        | DERControl::WattVar
        | DERControl::WattPF
        | DERControl::FixedPFAbsorb
        | DERControl::FixedPFInject
        | DERControl::FixedVar => Effect::Reactive,
        DERControl::UnknownValue(_) => Effect::Unknown,
    }
}

/// The types that carry their own field in a `SetDERControlRequest` and their own array in a
/// report; every other type is a `curve` (R04.FR.16).
fn has_own_field(kind: &DERControl) -> bool {
    matches!(
        kind,
        DERControl::EnterService
            | DERControl::FreqDroop
            | DERControl::FixedPFAbsorb
            | DERControl::FixedPFInject
            | DERControl::FixedVar
            | DERControl::Gradients
            | DERControl::LimitMaxDischarge
    )
}

/// A control's setting, exactly as the station reported it.
#[derive(Clone, Debug, PartialEq)]
pub enum Setting {
    /// Every curve type: `VoltWatt`, `FreqWatt`, the trip curves, …
    Curve(v2_1::DERCurve),
    /// `EnterService`.
    EnterService(v2_1::EnterService),
    /// `FixedPFAbsorb`.
    FixedPfAbsorb(v2_1::FixedPF),
    /// `FixedPFInject`.
    FixedPfInject(v2_1::FixedPF),
    /// `FixedVar`.
    FixedVar(v2_1::FixedVar),
    /// `FreqDroop`.
    FreqDroop(v2_1::FreqDroop),
    /// `Gradients`.
    Gradient(v2_1::Gradient),
    /// `LimitMaxDischarge`.
    ///
    /// No cap is derived from `pctMaxDischargePower`: §2.64 defines it as the threshold above
    /// which the `powerMonitoringMustTrip` curve applies, while use cases R01/R02 use it as the
    /// discharge cap itself. Read the field, and the station's documentation for which reading
    /// it implements.
    LimitMaxDischarge(v2_1::LimitMaxDischarge),
}

impl Setting {
    /// The setting's `priority`; 0 is the highest.
    #[must_use]
    pub fn priority(&self) -> i32 {
        match self {
            Self::Curve(s) => s.priority,
            Self::EnterService(s) => s.priority,
            Self::FixedPfAbsorb(s) | Self::FixedPfInject(s) => s.priority,
            Self::FixedVar(s) => s.priority,
            Self::FreqDroop(s) => s.priority,
            Self::Gradient(s) => s.priority,
            Self::LimitMaxDischarge(s) => s.priority,
        }
    }

    /// When a scheduled setting starts. `EnterService` and `Gradients` have none.
    #[must_use]
    pub fn start(&self) -> Option<DateTime> {
        match self {
            Self::Curve(s) => s.start_time,
            Self::FixedPfAbsorb(s) | Self::FixedPfInject(s) => s.start_time,
            Self::FixedVar(s) => s.start_time,
            Self::FreqDroop(s) => s.start_time,
            Self::LimitMaxDischarge(s) => s.start_time,
            Self::EnterService(_) | Self::Gradient(_) => None,
        }
    }

    /// How long a scheduled setting lasts, in seconds.
    #[must_use]
    pub fn duration(&self) -> Option<Decimal> {
        match self {
            Self::Curve(s) => s.duration,
            Self::FixedPfAbsorb(s) | Self::FixedPfInject(s) => s.duration,
            Self::FixedVar(s) => s.duration,
            Self::FreqDroop(s) => s.duration,
            Self::LimitMaxDischarge(s) => s.duration,
            Self::EnterService(_) | Self::Gradient(_) => None,
        }
    }
}

/// One DER control as a station reported it.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct DerControl {
    /// The control's id (`controlId` of the `SetDERControlRequest` that installed it).
    pub id: String,
    /// The control type.
    pub kind: DERControl,
    /// A default control, rather than a scheduled one. `EnterService` and `Gradients` exist only
    /// as defaults (R04.FR.14).
    pub default: bool,
    /// Superseded by a control of higher priority: never executed (R04.FR.09).
    pub superseded: bool,
    /// The setting.
    pub setting: Setting,
    /// The scheduled window in epoch milliseconds: `None` start = from always, `None` end =
    /// open-ended.
    window: (Option<i64>, Option<i64>),
}

impl DerControl {
    fn new(
        id: &str,
        kind: DERControl,
        default: bool,
        superseded: bool,
        setting: Setting,
    ) -> Result<Self, DerError> {
        let start = setting.start().map(|at| at.timestamp().as_millisecond());
        let end = match setting.duration() {
            None => None,
            Some(duration) => {
                let millis = duration
                    .checked_mul(Decimal::from(1000))
                    .and_then(|ms| ms.round_to(0))
                    .filter(|ms| ms.signum() >= 0)
                    .map(Decimal::mantissa)
                    .ok_or_else(|| DerError::BadDuration(id.into()))?;
                match start {
                    Some(start) => Some(
                        start
                            .checked_add(millis)
                            .ok_or_else(|| DerError::BadDuration(id.into()))?,
                    ),
                    // A duration without a start: in effect from receipt, which the report does
                    // not say. Open-ended is the reading that never drops a control.
                    None => None,
                }
            }
        };
        Ok(Self {
            id: id.into(),
            kind,
            default,
            superseded,
            setting,
            window: (start, end),
        })
    }

    /// The setting's `priority`; 0 is the highest.
    #[must_use]
    pub fn priority(&self) -> i32 {
        self.setting.priority()
    }

    /// What this control can do to active power.
    #[must_use]
    pub fn effect(&self) -> Effect {
        effect(&self.kind)
    }

    /// Whether `at` lies inside the control's window — always for a default, from `startTime`
    /// until `startTime + duration` for a scheduled one. Says nothing about superseding or
    /// priority; see [`DerControls::effective_at`].
    #[must_use]
    pub fn is_active_at(&self, at: DateTime) -> bool {
        if self.default {
            return true;
        }
        let at = at.timestamp().as_millisecond();
        self.window.0.is_none_or(|start| start <= at) && self.window.1.is_none_or(|end| at < end)
    }
}

/// The parts of one `GetDERControl` answer, collected (R04.FR.31/32).
///
/// Bound to one `requestId`; complete when a part arrives without `tbc = true`. A part is taken
/// whole or not at all.
#[derive(Clone, Debug)]
pub struct DerReport {
    request_id: i32,
    limit: usize,
    complete: bool,
    controls: Vec<DerControl>,
}

impl DerReport {
    /// How many controls a report may hold unless [`with_limit`](Self::with_limit) says
    /// otherwise — two orders of magnitude above 22 control types with a handful of priorities.
    pub const DEFAULT_LIMIT: usize = 1024;

    /// A collector for the report answering `GetDERControlRequest { requestId }`.
    #[must_use]
    pub fn new(request_id: i32) -> Self {
        Self {
            request_id,
            limit: Self::DEFAULT_LIMIT,
            complete: false,
            controls: Vec::new(),
        }
    }

    /// The most controls the report may hold.
    #[must_use]
    pub fn with_limit(mut self, limit: usize) -> Self {
        self.limit = limit;
        self
    }

    /// Whether the last part has arrived.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.complete
    }

    /// Adds one part. Returns whether the report is now complete.
    ///
    /// # Errors
    ///
    /// [`DerError`] for a part with another `requestId`, a part after the last, a curve whose
    /// type has its own array, a control id seen twice, a bad duration, or more controls than
    /// the limit. The collector is unchanged.
    pub fn add(&mut self, part: &v2_1::ReportDERControlRequest) -> Result<bool, DerError> {
        if self.complete {
            return Err(DerError::Complete);
        }
        if part.request_id != self.request_id {
            return Err(DerError::RequestId {
                expected: self.request_id,
                got: part.request_id,
            });
        }
        let added = controls_in(part)?;
        if self.controls.len() + added.len() > self.limit {
            return Err(DerError::TooManyControls { limit: self.limit });
        }
        for (index, control) in added.iter().enumerate() {
            let seen = |other: &DerControl| other.id == control.id;
            if self.controls.iter().any(seen) || added[..index].iter().any(seen) {
                return Err(DerError::DuplicateId(control.id.clone()));
            }
        }
        self.controls.extend(added);
        self.complete = part.tbc != Some(true);
        Ok(self.complete)
    }

    /// The collected controls.
    ///
    /// # Errors
    ///
    /// [`DerError::Incomplete`] when the last part has not arrived: a partial report is not the
    /// station's state.
    pub fn finish(self) -> Result<DerControls, DerError> {
        if !self.complete {
            return Err(DerError::Incomplete);
        }
        Ok(DerControls {
            controls: self.controls,
        })
    }
}

/// Every control one report part carries, checked one by one (R04.FR.16, durations).
fn controls_in(part: &v2_1::ReportDERControlRequest) -> Result<Vec<DerControl>, DerError> {
    let mut added = Vec::new();
    for c in part.curve.iter().flatten() {
        if has_own_field(&c.curve_type) {
            return Err(DerError::WrongArray {
                id: c.id.clone(),
                kind: c.curve_type.clone(),
            });
        }
        added.push(DerControl::new(
            &c.id,
            c.curve_type.clone(),
            c.is_default,
            c.is_superseded,
            Setting::Curve(c.curve.clone()),
        )?);
    }
    // EnterService and Gradients exist only as defaults (R04.FR.14) and their report types
    // carry neither flag.
    for c in part.enter_service.iter().flatten() {
        let setting = Setting::EnterService(c.enter_service.clone());
        added.push(DerControl::new(
            &c.id,
            DERControl::EnterService,
            true,
            false,
            setting,
        )?);
    }
    for c in part.gradient.iter().flatten() {
        let setting = Setting::Gradient(c.gradient.clone());
        added.push(DerControl::new(
            &c.id,
            DERControl::Gradients,
            true,
            false,
            setting,
        )?);
    }
    for (list, kind, wrap) in [
        (
            &part.fixed_pf_absorb,
            DERControl::FixedPFAbsorb,
            Setting::FixedPfAbsorb as fn(v2_1::FixedPF) -> Setting,
        ),
        (
            &part.fixed_pf_inject,
            DERControl::FixedPFInject,
            Setting::FixedPfInject,
        ),
    ] {
        for c in list.iter().flatten() {
            let setting = wrap(c.fixed_pf.clone());
            added.push(DerControl::new(
                &c.id,
                kind.clone(),
                c.is_default,
                c.is_superseded,
                setting,
            )?);
        }
    }
    for c in part.fixed_var.iter().flatten() {
        let setting = Setting::FixedVar(c.fixed_var.clone());
        added.push(DerControl::new(
            &c.id,
            DERControl::FixedVar,
            c.is_default,
            c.is_superseded,
            setting,
        )?);
    }
    for c in part.freq_droop.iter().flatten() {
        let setting = Setting::FreqDroop(c.freq_droop.clone());
        added.push(DerControl::new(
            &c.id,
            DERControl::FreqDroop,
            c.is_default,
            c.is_superseded,
            setting,
        )?);
    }
    for c in part.limit_max_discharge.iter().flatten() {
        let setting = Setting::LimitMaxDischarge(c.limit_max_discharge.clone());
        added.push(DerControl::new(
            &c.id,
            DERControl::LimitMaxDischarge,
            c.is_default,
            c.is_superseded,
            setting,
        )?);
    }
    Ok(added)
}

/// A station's DER controls, from a complete report and the start/stop notifications since.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DerControls {
    controls: Vec<DerControl>,
}

impl DerControls {
    /// Every control, in report order.
    pub fn iter(&self) -> impl Iterator<Item = &DerControl> {
        self.controls.iter()
    }

    /// How many controls there are.
    #[must_use]
    pub fn len(&self) -> usize {
        self.controls.len()
    }

    /// Whether there are none.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.controls.is_empty()
    }

    /// The control with this id.
    #[must_use]
    pub fn get(&self, id: &str) -> Option<&DerControl> {
        self.controls.iter().find(|control| control.id == id)
    }

    /// The control of type `kind` in effect at `at` (§2.9, R04.FR.09–11): the non-superseded
    /// scheduled control whose window holds `at` with the lowest `priority` value, else the
    /// non-superseded default with the lowest `priority` value. On a tie, the first reported.
    #[must_use]
    pub fn effective(&self, kind: &DERControl, at: DateTime) -> Option<&DerControl> {
        let best = |default: bool| {
            self.controls
                .iter()
                .filter(|c| c.kind == *kind && c.default == default && !c.superseded)
                .filter(|c| c.is_active_at(at))
                .fold(None, |best: Option<&DerControl>, c| match best {
                    Some(b) if b.priority() <= c.priority() => Some(b),
                    _ => Some(c),
                })
        };
        best(false).or_else(|| best(true))
    }

    /// One control per type, the one in effect at `at`; types with none in effect are absent.
    #[must_use]
    pub fn effective_at(&self, at: DateTime) -> Vec<&DerControl> {
        let mut kinds: Vec<&DERControl> = Vec::new();
        for control in &self.controls {
            if !kinds.contains(&&control.kind) {
                kinds.push(&control.kind);
            }
        }
        kinds
            .into_iter()
            .filter_map(|kind| self.effective(kind, at))
            .collect()
    }

    /// The controls in effect at `at` that can lower or stop active power — what a planner must
    /// not count on. Includes control types the crate does not know.
    #[must_use]
    pub fn limiting_at(&self, at: DateTime) -> Vec<&DerControl> {
        self.effective_at(at)
            .into_iter()
            .filter(|control| control.effect().limits_active_power())
            .collect()
    }

    /// Applies a `NotifyDERStartStopRequest` (R04.FR.20–22): a start marks the controls it
    /// supersedes, an end removes the control (R04.FR.10).
    pub fn apply(&mut self, notice: &v2_1::NotifyDERStartStopRequest) -> StartStop {
        if notice.started {
            let mut superseded = Vec::new();
            for id in notice.superseded_ids.iter().flatten() {
                if let Some(control) = self.controls.iter_mut().find(|c| c.id == *id) {
                    control.superseded = true;
                    superseded.push(id.clone());
                }
            }
            if self.get(&notice.control_id).is_none() {
                return StartStop::UnknownControl(notice.control_id.clone());
            }
            StartStop::Started { superseded }
        } else {
            match self.controls.iter().position(|c| c.id == notice.control_id) {
                Some(index) => StartStop::Ended(Box::new(self.controls.remove(index))),
                None => StartStop::UnknownControl(notice.control_id.clone()),
            }
        }
    }
}

impl<'a> IntoIterator for &'a DerControls {
    type Item = &'a DerControl;
    type IntoIter = core::slice::Iter<'a, DerControl>;

    fn into_iter(self) -> Self::IntoIter {
        self.controls.iter()
    }
}

/// What a `NotifyDERStartStopRequest` did.
#[derive(Clone, Debug, PartialEq)]
#[must_use = "an unknown control means the inventory is stale: ask the station again"]
pub enum StartStop {
    /// The control started; these known controls are now superseded.
    Started {
        /// The ids marked superseded.
        superseded: Vec<String>,
    },
    /// The control ended and was removed.
    Ended(Box<DerControl>),
    /// The station named a control this inventory does not have. Superseded ids it named were
    /// still applied; send `GetDERControl` to resynchronize.
    UnknownControl(String),
}

/// The alarm a station has raised (R05).
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct DerAlarm {
    /// The control that is taking over.
    pub kind: DERControl,
    /// What caused it, when the station said.
    pub fault: Option<v2_1::GridEventFault>,
    /// When the station raised it.
    pub since: DateTime,
    /// Free text from the station.
    pub extra_info: Option<String>,
}

/// A station's one DER alarm.
///
/// "There are no multiple levels of alarm" (R05.FR.04): when another control takes over, the
/// station reports the new one and never ends the first, so this holds one alarm, not a set.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DerAlarmState {
    current: Option<DerAlarm>,
}

impl DerAlarmState {
    /// No alarm.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The alarm currently raised.
    #[must_use]
    pub fn current(&self) -> Option<&DerAlarm> {
        self.current.as_ref()
    }

    /// Applies a `NotifyDERAlarmRequest`.
    pub fn apply(&mut self, notice: &v2_1::NotifyDERAlarmRequest) -> AlarmChange {
        if notice.alarm_ended == Some(true) {
            return match &self.current {
                Some(alarm) if alarm.kind == notice.control_type => self
                    .current
                    .take()
                    .map_or(AlarmChange::Unmatched, AlarmChange::Cleared),
                _ => AlarmChange::Unmatched,
            };
        }
        let alarm = DerAlarm {
            kind: notice.control_type.clone(),
            fault: notice.grid_event_fault.clone(),
            since: notice.timestamp,
            extra_info: notice.extra_info.clone(),
        };
        match self.current.take() {
            None => {
                self.current = Some(alarm);
                AlarmChange::Raised
            }
            // A station retries a message it got no answer to: the same alarm again is not a
            // new occurrence, and keeps its original start.
            Some(previous) if previous.kind == alarm.kind && previous.fault == alarm.fault => {
                self.current = Some(previous);
                AlarmChange::Repeated
            }
            Some(previous) => {
                self.current = Some(alarm);
                AlarmChange::Replaced(previous)
            }
        }
    }
}

/// What a `NotifyDERAlarmRequest` did.
#[derive(Clone, Debug, PartialEq, Eq)]
#[must_use = "an unmatched end means the alarm state and the station disagree"]
pub enum AlarmChange {
    /// An alarm was raised.
    Raised,
    /// The raised alarm was reported again, unchanged.
    Repeated,
    /// Another control took over (R05.FR.03); this was the alarm before.
    Replaced(DerAlarm),
    /// The alarm ended (R05.FR.02).
    Cleared(DerAlarm),
    /// An end for a control that is not the one raised, or with no alarm raised. Nothing changed.
    Unmatched,
}

/// A rule a `SetDERControlRequest` breaks.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum DerRule {
    /// A default control with a `startTime` or `duration` (R04.FR.13).
    DefaultWithTime,
    /// A scheduled `EnterService` or `Gradients`, which exist only as defaults (R04.FR.15).
    DefaultOnly,
    /// Not exactly one control field, or not the one the type maps to (R04.FR.17).
    FieldMismatch,
    /// A curve with a `yUnit` its type does not allow (R04.FR.50–56).
    WrongYUnit {
        /// The control type whose curve it is.
        kind: DERControl,
        /// The unit it had.
        unit: DERUnit,
    },
}

impl DerRule {
    /// The requirement the request breaks, e.g. `"R04.FR.13"`.
    #[must_use]
    pub fn requirement(&self) -> &'static str {
        match self {
            Self::DefaultWithTime => "R04.FR.13",
            Self::DefaultOnly => "R04.FR.15",
            Self::FieldMismatch => "R04.FR.17",
            Self::WrongYUnit { kind, .. } => {
                y_unit_rule(kind).map_or("R04.FR.50", |(rule, _)| rule)
            }
        }
    }
}

impl fmt::Display for DerRule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let rule = self.requirement();
        match self {
            Self::DefaultWithTime => {
                write!(
                    f,
                    "a default DER control has no startTime or duration ({rule})"
                )
            }
            Self::DefaultOnly => {
                write!(
                    f,
                    "EnterService and Gradients exist only as defaults ({rule})"
                )
            }
            Self::FieldMismatch => write!(
                f,
                "the request must carry exactly the one field its controlType maps to ({rule})"
            ),
            Self::WrongYUnit { kind, unit } => {
                write!(f, "a {kind} curve cannot use yUnit {unit} ({rule})")
            }
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for DerRule {}

/// The `yUnit`s a curve of `kind` may use, and the requirement that says so (R04.FR.50–56).
fn y_unit_rule(kind: &DERControl) -> Option<(&'static str, &'static [DERUnit])> {
    const POWER: &[DERUnit] = &[DERUnit::PctMaxW, DERUnit::PctWAvail];
    const VAR: &[DERUnit] = &[DERUnit::PctMaxVar, DERUnit::PctVarAvail];
    const NONE: &[DERUnit] = &[DERUnit::NotApplicable];
    Some(match kind {
        DERControl::FreqWatt => ("R04.FR.50", POWER),
        DERControl::HFMustTrip
        | DERControl::HFMayTrip
        | DERControl::HVMustTrip
        | DERControl::HVMomCess
        | DERControl::HVMayTrip
        | DERControl::LFMustTrip
        | DERControl::LVMustTrip
        | DERControl::LVMomCess
        | DERControl::LVMayTrip => ("R04.FR.51", NONE),
        DERControl::VoltVar => ("R04.FR.52", VAR),
        DERControl::VoltWatt => ("R04.FR.53", POWER),
        DERControl::WattPF => ("R04.FR.54", NONE),
        DERControl::WattVar => ("R04.FR.55", VAR),
        DERControl::LimitMaxDischarge => ("R04.FR.56", NONE),
        _ => return None,
    })
}

/// Refuses a `SetDERControlRequest` that the station must reject: a default with a schedule
/// (R04.FR.13), a scheduled `EnterService`/`Gradients` (R04.FR.15), a field that does not match
/// `controlType` (R04.FR.17), a curve unit the type does not allow (R04.FR.50–56).
///
/// # Errors
///
/// The first [`DerRule`] the request breaks.
pub fn check(request: &v2_1::SetDERControlRequest) -> Result<(), DerRule> {
    let kind = &request.control_type;
    let present = [
        ("curve", request.curve.is_some()),
        ("enterService", request.enter_service.is_some()),
        ("fixedPFAbsorb", request.fixed_pf_absorb.is_some()),
        ("fixedPFInject", request.fixed_pf_inject.is_some()),
        ("fixedVar", request.fixed_var.is_some()),
        ("freqDroop", request.freq_droop.is_some()),
        ("gradient", request.gradient.is_some()),
        ("limitMaxDischarge", request.limit_max_discharge.is_some()),
    ];
    let expected = match kind {
        DERControl::EnterService => "enterService",
        DERControl::FixedPFAbsorb => "fixedPFAbsorb",
        DERControl::FixedPFInject => "fixedPFInject",
        DERControl::FixedVar => "fixedVar",
        DERControl::FreqDroop => "freqDroop",
        DERControl::Gradients => "gradient",
        DERControl::LimitMaxDischarge => "limitMaxDischarge",
        _ => "curve",
    };
    let mut set = present.iter().filter(|(_, set)| *set);
    if set.next().map(|(name, _)| *name) != Some(expected) || set.next().is_some() {
        return Err(DerRule::FieldMismatch);
    }
    if !request.is_default && matches!(kind, DERControl::EnterService | DERControl::Gradients) {
        return Err(DerRule::DefaultOnly);
    }
    let setting = request
        .curve
        .clone()
        .map(Setting::Curve)
        .or_else(|| request.fixed_pf_absorb.clone().map(Setting::FixedPfAbsorb))
        .or_else(|| request.fixed_pf_inject.clone().map(Setting::FixedPfInject))
        .or_else(|| request.fixed_var.clone().map(Setting::FixedVar))
        .or_else(|| request.freq_droop.clone().map(Setting::FreqDroop))
        .or_else(|| {
            request
                .limit_max_discharge
                .clone()
                .map(Setting::LimitMaxDischarge)
        });
    if request.is_default
        && setting
            .as_ref()
            .is_some_and(|s| s.start().is_some() || s.duration().is_some())
    {
        return Err(DerRule::DefaultWithTime);
    }
    let curve = match (&request.curve, &request.limit_max_discharge) {
        (Some(curve), _) => Some(curve),
        (None, Some(limit)) => limit.power_monitoring_must_trip.as_ref(),
        (None, None) => None,
    };
    if let (Some(curve), Some((_, allowed))) = (curve, y_unit_rule(kind)) {
        if !allowed.contains(&curve.y_unit) {
            return Err(DerRule::WrongYUnit {
                kind: kind.clone(),
                unit: curve.y_unit.clone(),
            });
        }
    }
    Ok(())
}

/// Why a DER report could not be collected.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum DerError {
    /// A part answering another `GetDERControl` (R04.FR.31).
    RequestId {
        /// The collector's `requestId`.
        expected: i32,
        /// The part's.
        got: i32,
    },
    /// A part after the last one (R04.FR.32).
    Complete,
    /// The last part has not arrived.
    Incomplete,
    /// A curve whose type has its own array (R04.FR.16).
    WrongArray {
        /// The control's id.
        id: String,
        /// Its type.
        kind: DERControl,
    },
    /// Two controls with one id.
    DuplicateId(String),
    /// More controls than the collector's limit.
    TooManyControls {
        /// The limit.
        limit: usize,
    },
    /// A negative duration, or a start plus duration that cannot be represented.
    BadDuration(String),
}

impl fmt::Display for DerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // `{:?}` quotes and escapes: ids came from a station.
        match self {
            Self::RequestId { expected, got } => write!(
                f,
                "report part for requestId {got}, expected {expected} (R04.FR.31)"
            ),
            Self::Complete => f.write_str("report part after the last one (R04.FR.32)"),
            Self::Incomplete => f.write_str("the report's last part has not arrived"),
            Self::WrongArray { id, kind } => write!(
                f,
                "control {id:?} of type {kind} arrived as a curve; it has its own field (R04.FR.16)"
            ),
            Self::DuplicateId(id) => write!(f, "control id {id:?} reported twice"),
            Self::TooManyControls { limit } => {
                write!(f, "report holds more than {limit} controls")
            }
            Self::BadDuration(id) => write!(
                f,
                "control {id:?} has a negative or unrepresentable duration"
            ),
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for DerError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(text: &str) -> DateTime {
        text.parse().unwrap()
    }

    fn part(json: &str) -> v2_1::ReportDERControlRequest {
        serde_json::from_str(json).unwrap()
    }

    fn curve(id: &str, kind: &str, default: bool, superseded: bool, extra: &str) -> String {
        let unit = match kind {
            "VoltVar" | "WattVar" => "PctMaxVar",
            "VoltWatt" | "FreqWatt" => "PctMaxW",
            _ => "Not_Applicable",
        };
        format!(
            r#"{{"id":"{id}","curveType":"{kind}","isDefault":{default},"isSuperseded":{superseded},
               "curve":{{"yUnit":"{unit}","curveData":[{{"x":1,"y":2}}]{extra}}}}}"#
        )
    }

    fn controls(curves: &[String]) -> DerControls {
        let mut report = DerReport::new(1);
        let json = format!(r#"{{"requestId":1,"curve":[{}]}}"#, curves.join(","));
        assert!(report.add(&part(&json)).unwrap());
        report.finish().unwrap()
    }

    #[test]
    fn a_running_scheduled_control_beats_the_default_and_the_default_returns_after_it() {
        let set = controls(&[
            curve("default", "VoltWatt", true, false, r#","priority":1"#),
            curve(
                "event",
                "VoltWatt",
                false,
                false,
                r#","priority":0,"startTime":"2026-11-20T14:00:00Z","duration":3600"#,
            ),
        ]);
        let kind = DERControl::VoltWatt;
        assert_eq!(
            set.effective(&kind, at("2026-11-20T14:30:00Z")).unwrap().id,
            "event"
        );
        assert_eq!(
            set.effective(&kind, at("2026-11-20T16:00:00Z")).unwrap().id,
            "default"
        );
        // The window is half-open: the end instant belongs to the default again.
        assert_eq!(
            set.effective(&kind, at("2026-11-20T15:00:00Z")).unwrap().id,
            "default"
        );
        assert_eq!(
            set.effective(&kind, at("2026-11-20T13:59:59Z")).unwrap().id,
            "default"
        );
    }

    #[test]
    fn the_lowest_priority_value_wins_and_a_superseded_control_is_never_in_effect() {
        let set = controls(&[
            curve("low", "FreqWatt", true, true, r#","priority":2"#),
            curve("high", "FreqWatt", true, false, r#","priority":1"#),
            curve("tie", "FreqWatt", true, false, r#","priority":1"#),
        ]);
        let effective = set.effective(&DERControl::FreqWatt, at("2026-11-20T00:00:00Z"));
        assert_eq!(effective.unwrap().id, "high");

        // A superseded scheduled control inside its window falls back to the default.
        let set = controls(&[
            curve("default", "VoltWatt", true, false, r#","priority":3"#),
            curve(
                "gone",
                "VoltWatt",
                false,
                true,
                r#","priority":0,"startTime":"2026-11-20T00:00:00Z","duration":60"#,
            ),
        ]);
        let effective = set.effective(&DERControl::VoltWatt, at("2026-11-20T00:00:30Z"));
        assert_eq!(effective.unwrap().id, "default");
    }

    #[test]
    fn only_controls_that_lower_or_stop_active_power_are_limiting() {
        let mut report = DerReport::new(4);
        report
            .add(&part(&format!(
                r#"{{"requestId":4,"tbc":true,"curve":[{},{}]}}"#,
                curve("vv", "VoltVar", true, false, r#","priority":0"#),
                curve("trip", "HVMustTrip", true, false, r#","priority":0"#),
            )))
            .unwrap();
        report
            .add(&part(
                r#"{"requestId":4,"freqDroop":[{"id":"droop","isDefault":true,"isSuperseded":false,
                    "freqDroop":{"priority":0,"overFreq":50.2,"underFreq":49.8,"overDroop":5,
                    "underDroop":5,"responseTime":1}}]}"#,
            ))
            .unwrap();
        let set = report.finish().unwrap();
        let ids: Vec<_> = set
            .limiting_at(at("2026-11-20T00:00:00Z"))
            .into_iter()
            .map(|c| c.id.as_str())
            .collect();
        assert_eq!(ids, ["trip", "droop"]);
        assert_eq!(set.effective_at(at("2026-11-20T00:00:00Z")).len(), 3);
    }

    #[test]
    fn an_unknown_control_type_counts_as_limiting() {
        assert_eq!(
            effect(&DERControl::from_wire("VendorCurve")),
            Effect::Unknown
        );
        assert!(Effect::Unknown.limits_active_power());
        assert!(!Effect::Reactive.limits_active_power());
        assert!(!Effect::Slows.limits_active_power());
        let set = controls(&[curve("v", "VendorCurve", true, false, r#","priority":0"#)]);
        assert_eq!(set.limiting_at(at("2026-11-20T00:00:00Z")).len(), 1);
    }

    #[test]
    fn a_stray_late_or_malformed_part_is_refused_and_changes_nothing() {
        let mut report = DerReport::new(7);
        let other = part(r#"{"requestId":8}"#);
        assert_eq!(
            report.add(&other),
            Err(DerError::RequestId {
                expected: 7,
                got: 8
            })
        );
        let wrong_array = part(&format!(
            r#"{{"requestId":7,"tbc":true,"curve":[{}]}}"#,
            curve("x", "FreqDroop", true, false, r#","priority":0"#)
        ));
        assert!(matches!(
            report.add(&wrong_array),
            Err(DerError::WrongArray { .. })
        ));
        let negative = part(&format!(
            r#"{{"requestId":7,"tbc":true,"curve":[{},{}]}}"#,
            curve("fine", "VoltVar", true, false, r#","priority":0"#),
            curve(
                "neg",
                "VoltVar",
                false,
                false,
                r#","priority":0,"startTime":"2026-11-20T00:00:00Z","duration":-5"#
            ),
        ));
        assert_eq!(
            report.add(&negative),
            Err(DerError::BadDuration("neg".into()))
        );
        let twice = part(&format!(
            r#"{{"requestId":7,"tbc":true,"curve":[{0},{0}]}}"#,
            curve("dup", "VoltVar", true, false, r#","priority":0"#)
        ));
        assert_eq!(report.add(&twice), Err(DerError::DuplicateId("dup".into())));
        assert!(!report.is_complete());
        assert_eq!(report.clone().finish(), Err(DerError::Incomplete));

        // Nothing above was taken: an empty last part completes an empty report.
        assert_eq!(report.add(&part(r#"{"requestId":7}"#)), Ok(true));
        assert_eq!(
            report.add(&part(r#"{"requestId":7}"#)),
            Err(DerError::Complete)
        );
        assert!(report.finish().unwrap().is_empty());
    }

    #[test]
    fn the_collector_is_bounded() {
        let mut report = DerReport::new(1).with_limit(2);
        let three = part(&format!(
            r#"{{"requestId":1,"curve":[{},{},{}]}}"#,
            curve("a", "VoltVar", true, false, r#","priority":0"#),
            curve("b", "VoltWatt", true, false, r#","priority":0"#),
            curve("c", "WattVar", true, false, r#","priority":0"#),
        ));
        assert_eq!(
            report.add(&three),
            Err(DerError::TooManyControls { limit: 2 })
        );
        assert!(!report.is_complete());
    }

    #[test]
    fn a_limit_max_discharge_control_is_carried_as_reported() {
        let mut report = DerReport::new(1);
        report
            .add(&part(
                r#"{"requestId":1,"limitMaxDischarge":[{"id":"cap","isDefault":true,
                    "isSuperseded":false,"limitMaxDischarge":{"priority":0,"pctMaxDischargePower":50.0}}]}"#,
            ))
            .unwrap();
        let set = report.finish().unwrap();
        let control = set.get("cap").unwrap();
        assert_eq!(control.effect(), Effect::Lowers);
        let Setting::LimitMaxDischarge(setting) = &control.setting else {
            panic!("not a LimitMaxDischarge setting");
        };
        // The station's own number at its own scale; nothing derived from it.
        assert_eq!(setting.pct_max_discharge_power.unwrap().to_string(), "50.0");
    }

    #[test]
    fn start_stop_notices_keep_the_inventory_current() {
        let mut set = controls(&[
            curve(
                "a",
                "VoltWatt",
                false,
                false,
                r#","priority":1,"startTime":"2026-11-20T00:00:00Z","duration":7200"#,
            ),
            curve(
                "b",
                "VoltWatt",
                false,
                false,
                r#","priority":0,"startTime":"2026-11-20T01:00:00Z","duration":600"#,
            ),
        ]);
        let started = v2_1::NotifyDERStartStopRequest::new("b", true, at("2026-11-20T01:00:00Z"))
            .with_superseded_ids(["a", "missing"]);
        assert_eq!(
            set.apply(&started),
            StartStop::Started {
                superseded: vec!["a".into()]
            }
        );
        assert!(set.get("a").unwrap().superseded);
        let ended = v2_1::NotifyDERStartStopRequest::new("b", false, at("2026-11-20T01:10:00Z"));
        assert!(matches!(set.apply(&ended), StartStop::Ended(control) if control.id == "b"));
        assert!(set.get("b").is_none());
        assert_eq!(set.apply(&ended), StartStop::UnknownControl("b".into()));
    }

    fn alarm(
        kind: DERControl,
        fault: Option<v2_1::GridEventFault>,
        ended: bool,
    ) -> v2_1::NotifyDERAlarmRequest {
        let mut notice = v2_1::NotifyDERAlarmRequest::new(kind, at("2026-11-20T12:00:00Z"));
        notice.grid_event_fault = fault;
        notice.alarm_ended = ended.then_some(true);
        notice
    }

    #[test]
    fn an_alarm_has_one_level() {
        use v2_1::GridEventFault::{OverFrequency, OverVoltage};
        let mut state = DerAlarmState::new();
        assert_eq!(
            state.apply(&alarm(DERControl::VoltWatt, Some(OverVoltage), false)),
            AlarmChange::Raised
        );
        assert_eq!(state.current().unwrap().kind, DERControl::VoltWatt);
        assert_eq!(
            state.apply(&alarm(DERControl::VoltWatt, Some(OverVoltage), false)),
            AlarmChange::Repeated
        );
        let replaced = state.apply(&alarm(DERControl::FreqDroop, Some(OverFrequency), false));
        assert!(
            matches!(replaced, AlarmChange::Replaced(previous) if previous.kind == DERControl::VoltWatt)
        );
        // R05.FR.04: the end of the first is never reported; one for it changes nothing.
        assert_eq!(
            state.apply(&alarm(DERControl::VoltWatt, None, true)),
            AlarmChange::Unmatched
        );
        assert_eq!(state.current().unwrap().kind, DERControl::FreqDroop);
        assert!(matches!(
            state.apply(&alarm(DERControl::FreqDroop, Some(OverFrequency), true)),
            AlarmChange::Cleared(_)
        ));
        assert!(state.current().is_none());
        assert_eq!(
            state.apply(&alarm(DERControl::FreqDroop, None, true)),
            AlarmChange::Unmatched
        );
    }

    fn set_request(json: &str) -> v2_1::SetDERControlRequest {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn check_refuses_what_r04_forbids_and_names_the_rule() {
        let default_with_time = set_request(
            r#"{"isDefault":true,"controlId":"a","controlType":"FreqDroop","freqDroop":{"priority":0,
                "overFreq":50.2,"underFreq":49.8,"overDroop":5,"underDroop":5,"responseTime":1,
                "startTime":"2026-11-20T00:00:00Z"}}"#,
        );
        assert_eq!(
            check(&default_with_time).unwrap_err().requirement(),
            "R04.FR.13"
        );
        let scheduled_enter_service = set_request(
            r#"{"isDefault":false,"controlId":"a","controlType":"EnterService","enterService":{
                "priority":0,"highVoltage":253,"lowVoltage":207,"highFreq":50.1,"lowFreq":47.5}}"#,
        );
        assert_eq!(check(&scheduled_enter_service), Err(DerRule::DefaultOnly));
        let droop_as_curve = set_request(
            r#"{"isDefault":true,"controlId":"a","controlType":"FreqDroop","curve":{"priority":0,
                "yUnit":"PctMaxW","curveData":[{"x":1,"y":2}]}}"#,
        );
        assert_eq!(check(&droop_as_curve), Err(DerRule::FieldMismatch));
        let two_fields = set_request(
            r#"{"isDefault":true,"controlId":"a","controlType":"Gradients",
                "gradient":{"priority":0,"gradient":10,"softGradient":0},
                "curve":{"priority":0,"yUnit":"PctMaxW","curveData":[{"x":1,"y":2}]}}"#,
        );
        assert_eq!(check(&two_fields).unwrap_err().requirement(), "R04.FR.17");
        let none = set_request(r#"{"isDefault":true,"controlId":"a","controlType":"VoltVar"}"#);
        assert_eq!(check(&none), Err(DerRule::FieldMismatch));
        let wrong_unit = set_request(
            r#"{"isDefault":true,"controlId":"a","controlType":"VoltWatt","curve":{"priority":0,
                "yUnit":"PctMaxVar","curveData":[{"x":1,"y":2}]}}"#,
        );
        let error = check(&wrong_unit).unwrap_err();
        assert_eq!(error.requirement(), "R04.FR.53");
        assert!(error.to_string().contains("VoltWatt"));
        let monitoring = set_request(
            r#"{"isDefault":true,"controlId":"a","controlType":"LimitMaxDischarge",
                "limitMaxDischarge":{"priority":0,"pctMaxDischargePower":50,
                "powerMonitoringMustTrip":{"priority":0,"yUnit":"PctMaxW","curveData":[{"x":1,"y":2}]}}}"#,
        );
        assert_eq!(check(&monitoring).unwrap_err().requirement(), "R04.FR.56");
    }

    #[test]
    fn check_accepts_the_specifications_own_examples() {
        // R04 scenario #1: a default frequency droop without startTime or duration.
        let droop = set_request(
            r#"{"isDefault":true,"controlId":"droop","controlType":"FreqDroop","freqDroop":{
                "priority":0,"overFreq":50.2,"underFreq":49.8,"overDroop":5,"underDroop":5,
                "responseTime":1}}"#,
        );
        assert_eq!(check(&droop), Ok(()));
        // R01: a default VoltWatt curve and a discharge limit of 50 %.
        let volt_watt = set_request(
            r#"{"isDefault":true,"controlId":"vw","controlType":"VoltWatt","curve":{"priority":0,
                "yUnit":"PctMaxW","curveData":[{"x":106,"y":100},{"x":110,"y":0}]}}"#,
        );
        assert_eq!(check(&volt_watt), Ok(()));
        let limit = set_request(
            r#"{"isDefault":true,"controlId":"lmd","controlType":"LimitMaxDischarge",
                "limitMaxDischarge":{"priority":0,"pctMaxDischargePower":50.0}}"#,
        );
        assert_eq!(check(&limit), Ok(()));
        // A scheduled trip curve with its window.
        let trip = set_request(
            r#"{"isDefault":false,"controlId":"hv","controlType":"HVMustTrip","curve":{"priority":0,
                "yUnit":"Not_Applicable","startTime":"2026-11-20T00:00:00Z","duration":3600,
                "curveData":[{"x":0.16,"y":120}]}}"#,
        );
        assert_eq!(check(&trip), Ok(()));
    }
}
