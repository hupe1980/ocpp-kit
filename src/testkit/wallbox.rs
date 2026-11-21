//! One simulated EVSE that holds an [`Ev`] and speaks OCPP.

use std::string::{String, ToString};
use std::time::Duration;
use std::vec::Vec;

use serde_json::value::RawValue;

use super::ev::{Command, Ev, EvError, EvState, Power, Reading};
use crate::decode;
use crate::engine::IncomingRequest;
use crate::rpc::CallError;
use crate::smart_charging::{ProfileStore, Purpose, RateUnit, Supply};
use crate::types::{DateTime, Decimal};
use crate::version::Version;

/// One message the wallbox would send to its CSMS.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outgoing {
    /// The action.
    pub action: &'static str,
    /// The payload, as JSON text.
    pub payload: String,
}

impl Outgoing {
    fn new(action: &'static str, payload: &impl serde::Serialize) -> Self {
        Self {
            action,
            // A generated type serializes to an object; there is no failure to report.
            payload: serde_json::to_string(payload).unwrap_or_default(),
        }
    }
}

/// A transaction as the wallbox runs it.
#[derive(Clone, Debug)]
struct Tx {
    /// 2.x: generated here. 1.6: assigned by the CSMS in the `StartTransaction` answer.
    id: Option<String>,
    start: DateTime,
    seq: i32,
    /// 1.6: unplugged before the CSMS assigned the id — when, and the closing register.
    held_stop: Option<(DateTime, Decimal)>,
}

/// A 2.1 profile with V2X periods, which the ceiling calculation does not model.
#[cfg(feature = "v2_1")]
#[derive(Clone, Debug)]
struct V2xProfile {
    profile: crate::v2_1::ChargingProfile,
    evse_id: u32,
    /// Installation, or the last `UpdateDynamicSchedule`: where a `Dynamic` profile's fallback
    /// time runs from (K28.FR.13).
    refreshed: DateTime,
}

/// A wallbox: one EVSE with one connector, a vehicle when one is plugged in, and the charging
/// profiles a CSMS installed — on virtual time, without I/O.
///
/// It needs the three version features, which are the default.
///
/// It is the station half of a test of a CSMS or an energy manager. It answers
/// `SetChargingProfile`, `ClearChargingProfile` and (2.1) `UpdateDynamicSchedule` with
/// [`handle`](Self::handle), evaluates the installed profiles with the crate's own composite
/// calculation every [`step`](Self::step), hands the result to the vehicle, and returns what a
/// station would send: `StatusNotification`, `TransactionEvent` with meter values (1.6:
/// `StartTransaction`, `MeterValues`, `StopTransaction`) and `NotifyEVChargingNeeds`. Carrying
/// those to the CSMS under test is the caller's — through [`Sim`](super::Sim), a
/// [`MockStation`](super::MockStation), or its own transport.
///
/// Charging is free: the transaction starts when the vehicle plugs in. A 1.6 transaction id is
/// the one the CSMS assigns; samples before [`answered`](Self::answered) hands it over are
/// skipped, never sent with an invented id.
///
/// ```
/// use std::time::Duration;
/// use ocpp_kit::Version;
/// use ocpp_kit::testkit::{Ev, Wallbox};
/// use ocpp_kit::types::Decimal;
///
/// let mut wallbox = Wallbox::new(Version::V2_0_1, "2026-11-20T18:00:00Z".parse()?);
/// let sent = wallbox.plug_in(Ev::builder().soc(30).build())?;
/// let actions: Vec<_> = sent.iter().map(|o| o.action).collect();
/// assert_eq!(actions, ["StatusNotification", "TransactionEvent", "NotifyEVChargingNeeds"]);
///
/// // The energy manager allows 6 A from now on.
/// let answer = wallbox.handle("SetChargingProfile", r#"{"evseId":1,"chargingProfile":{
///     "id":1,"stackLevel":0,"chargingProfilePurpose":"TxDefaultProfile",
///     "chargingProfileKind":"Relative","chargingSchedule":[{"id":1,"chargingRateUnit":"A",
///     "chargingSchedulePeriod":[{"startPeriod":0,"limit":6}]}]}}"#);
/// assert!(answer.unwrap()?.contains("Accepted"));
///
/// wallbox.advance(Duration::from_secs(3600));
/// assert_eq!(wallbox.ev().unwrap().reading().current, Decimal::from(6));
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Clone, Debug)]
pub struct Wallbox {
    version: Version,
    evse_id: u32,
    now: DateTime,
    step: Duration,
    sample_every: Option<Duration>,
    since_sample: Duration,
    supply: Supply,
    max_current: Decimal,
    profiles: ProfileStore,
    #[cfg(feature = "v2_1")]
    v2x: Vec<V2xProfile>,
    ev: Option<Ev>,
    tx: Option<Tx>,
    transactions: u64,
    last_command: Option<Command>,
    last_state: Option<&'static str>,
    register: Decimal,
    export_register: Decimal,
    at_plug: (Decimal, Decimal),
    out: Vec<Outgoing>,
}

/// The id tag a 1.6 wallbox starts its free-charging transactions with.
pub const ID_TAG: &str = "SIMULATED";

impl Wallbox {
    /// A wallbox speaking `version`, its clock at `now`; EVSE 1, a 1 s step, a sample every
    /// 60 s, 230 V, 32 A.
    ///
    /// # Panics
    ///
    /// If the crate was built without `version`'s feature.
    #[must_use]
    pub fn new(version: Version, now: DateTime) -> Self {
        let compiled = match version {
            Version::V1_6 => cfg!(feature = "v1_6"),
            Version::V2_0_1 => cfg!(feature = "v2_0_1"),
            Version::V2_1 => cfg!(feature = "v2_1"),
        };
        assert!(compiled, "ocpp-kit was built without OCPP {version}");
        Self {
            version,
            evse_id: 1,
            now,
            step: Duration::from_secs(1),
            sample_every: Some(Duration::from_secs(60)),
            since_sample: Duration::ZERO,
            supply: Supply::default(),
            max_current: Decimal::from(32),
            profiles: ProfileStore::new(),
            #[cfg(feature = "v2_1")]
            v2x: Vec::new(),
            ev: None,
            tx: None,
            transactions: 0,
            last_command: None,
            last_state: None,
            register: Decimal::ZERO,
            export_register: Decimal::ZERO,
            at_plug: (Decimal::ZERO, Decimal::ZERO),
            out: Vec::new(),
        }
    }

    /// The EVSE id (the 1.6 connector id). Default 1.
    #[must_use]
    pub fn evse(mut self, id: u32) -> Self {
        self.evse_id = id.max(1);
        self
    }

    /// How finely [`advance`](Self::advance) evaluates profiles and integrates. Default 1 s.
    #[must_use]
    pub fn step(mut self, step: Duration) -> Self {
        self.step = step.max(Duration::from_millis(1));
        self
    }

    /// How often a transaction reports meter values; `None` for never. Default 60 s.
    #[must_use]
    pub fn sample_every(mut self, every: Option<Duration>) -> Self {
        self.sample_every = every.filter(|every| !every.is_zero());
        self
    }

    /// The supply, for converting a profile in W to amperes. Default 230 V.
    #[must_use]
    pub fn supply(mut self, supply: Supply) -> Self {
        self.supply = supply;
        self.profiles.supply = supply;
        self
    }

    /// The most current per phase the wallbox delivers without any profile. Default 32 A.
    #[must_use]
    pub fn max_current(mut self, amps: impl Into<Decimal>) -> Self {
        self.max_current = amps.into();
        self
    }

    /// The wallbox clock.
    #[must_use]
    pub fn now(&self) -> DateTime {
        self.now
    }

    /// The vehicle, when one is plugged in.
    #[must_use]
    pub fn ev(&self) -> Option<&Ev> {
        self.ev.as_ref()
    }

    /// The running transaction's id, once it has one.
    #[must_use]
    pub fn transaction_id(&self) -> Option<&str> {
        self.tx.as_ref().and_then(|tx| tx.id.as_deref())
    }

    /// The installed ceiling profiles.
    #[must_use]
    pub fn profiles(&self) -> &ProfileStore {
        &self.profiles
    }

    /// The command the vehicle was last given.
    #[must_use]
    pub fn command(&self) -> Option<Command> {
        self.last_command
    }

    /// Plugs `ev` in and starts a transaction.
    ///
    /// On 1.6, hand over the previous session's `StartTransaction` answer first: a vehicle
    /// unplugged before it arrived holds its `StopTransaction` until it does.
    ///
    /// # Errors
    ///
    /// [`EvError::AlreadyPlugged`] while another vehicle is plugged in.
    pub fn plug_in(&mut self, mut ev: Ev) -> Result<Vec<Outgoing>, EvError> {
        if self.ev.is_some() {
            return Err(EvError::AlreadyPlugged);
        }
        if !ev.is_plugged() {
            ev.plug_in()?;
        }
        let reading = ev.reading();
        self.at_plug = (reading.imported, reading.exported);
        self.ev = Some(ev);
        self.transactions += 1;
        self.since_sample = Duration::ZERO;
        self.last_command = None;
        let id = (self.version != Version::V1_6).then(|| format!("SIM-{}", self.transactions));
        self.tx = Some(Tx {
            id,
            start: self.now,
            seq: 0,
            held_stop: None,
        });
        self.last_state = Some(self.state_name());
        self.emit_plug_in();
        self.tick();
        Ok(std::mem::take(&mut self.out))
    }

    /// Unplugs the vehicle and ends the transaction. Returns what was sent and the vehicle.
    ///
    /// # Errors
    ///
    /// [`EvError::NotPlugged`] without a vehicle.
    pub fn unplug(&mut self) -> Result<(Vec<Outgoing>, Ev), EvError> {
        let Some(mut ev) = self.ev.take() else {
            return Err(EvError::NotPlugged);
        };
        let reading = ev.reading();
        self.register = self.register_at(&reading).0;
        self.export_register = self.register_at(&reading).1;
        ev.unplug()?;
        self.emit_unplug();
        if let Some(id) = self.tx.as_ref().and_then(|tx| tx.id.clone()) {
            // K08 / 1.6 §3.13.4: a TxProfile ends with its transaction.
            self.profiles.clear_transaction(&id);
            #[cfg(feature = "v2_1")]
            self.v2x
                .retain(|p| p.profile.transaction_id.as_deref() != Some(id.as_str()));
        }
        if self.tx.as_ref().is_some_and(|tx| tx.id.is_some()) {
            self.tx = None;
        }
        self.last_command = None;
        self.last_state = None;
        Ok((std::mem::take(&mut self.out), ev))
    }

    /// Lets `by` pass: profiles are evaluated and the vehicle advanced every step, and what the
    /// station would send meanwhile is returned.
    pub fn advance(&mut self, by: Duration) -> Vec<Outgoing> {
        let mut left = by;
        while !left.is_zero() {
            let step = left.min(self.step);
            self.tick();
            if let Some(ev) = &mut self.ev {
                ev.advance(step);
            }
            self.now = DateTime::from_timestamp(
                self.now
                    .timestamp()
                    .checked_add(jiff::SignedDuration::from_millis(
                        i64::try_from(step.as_millis()).unwrap_or(i64::MAX),
                    ))
                    .unwrap_or(self.now.timestamp()),
            );
            left -= step;
            self.after_step(step);
        }
        self.tick();
        std::mem::take(&mut self.out)
    }

    /// Answers a request from the CSMS: `SetChargingProfile`, `ClearChargingProfile`, and in 2.1
    /// `UpdateDynamicSchedule`. Returns `None` for any other action — it is the caller's.
    ///
    /// A payload that does not decode is a [`CallError`] with the code the specification
    /// prescribes; a profile the wallbox cannot apply is answered `Rejected`.
    pub fn handle(&mut self, action: &str, payload: &str) -> Option<Result<String, CallError>> {
        match self.version {
            #[cfg(feature = "v1_6")]
            Version::V1_6 => v16::handle(self, action, payload),
            #[cfg(feature = "v2_0_1")]
            Version::V2_0_1 => v201::handle(self, action, payload),
            #[cfg(feature = "v2_1")]
            Version::V2_1 => v21::handle(self, action, payload),
            #[allow(unreachable_patterns, clippy::allow_attributes)]
            _ => None,
        }
    }

    /// [`handle`](Self::handle) for a request from the engine or a transport handler.
    pub fn answer(
        &mut self,
        request: &IncomingRequest,
    ) -> Option<Result<Box<RawValue>, CallError>> {
        self.handle(&request.action, request.payload.get())
            .map(|answer| {
                answer.and_then(|text| {
                    RawValue::from_string(text)
                        .map_err(|error| CallError::internal(error.to_string()))
                })
            })
    }

    /// Hands the wallbox the CSMS's answer to one of its own calls. Only the 1.6
    /// `StartTransaction` answer matters: it assigns the transaction id, and a `StopTransaction`
    /// held for it is sent now.
    pub fn answered(&mut self, action: &str, payload: &str) -> Vec<Outgoing> {
        #[cfg(feature = "v1_6")]
        if self.version == Version::V1_6 && action == "StartTransaction" {
            v16::answered(self, payload);
        }
        let _ = (action, payload);
        std::mem::take(&mut self.out)
    }

    /// The registers at `reading`: import and export, in Wh.
    fn register_at(&self, reading: &Reading) -> (Decimal, Decimal) {
        (
            self.register + (reading.imported - self.at_plug.0),
            self.export_register + (reading.exported - self.at_plug.1),
        )
    }

    /// The vehicle's state as the version names it.
    fn state_name(&self) -> &'static str {
        let state = self.ev.as_ref().map_or(EvState::Unplugged, Ev::state);
        let v16 = self.version == Version::V1_6;
        match state {
            EvState::Unplugged => {
                if v16 {
                    "Available"
                } else {
                    "Idle"
                }
            }
            EvState::Connected => {
                if v16 {
                    "Preparing"
                } else {
                    "EVConnected"
                }
            }
            EvState::Charging | EvState::Discharging => "Charging",
            EvState::SuspendedEv | EvState::Faulted => "SuspendedEV",
            _ => "SuspendedEVSE",
        }
    }

    /// Gives the vehicle what the installed profiles allow now, if that changed.
    fn tick(&mut self) {
        if self.ev.is_none() {
            return;
        }
        let command = self.command_now();
        if self.last_command == Some(command) {
            return;
        }
        let changed = self.last_command.is_some();
        self.last_command = Some(command);
        if let Some(ev) = &mut self.ev {
            ev.command(command);
        }
        if changed && self.version != Version::V1_6 {
            self.emit_tx_update("ChargingRateChanged", false);
        }
    }

    fn after_step(&mut self, step: Duration) {
        if self.ev.is_none() {
            return;
        }
        let state = self.state_name();
        if self.last_state != Some(state) {
            self.last_state = Some(state);
            match self.version {
                Version::V1_6 => {
                    #[cfg(feature = "v1_6")]
                    v16::status(self, state);
                }
                _ => self.emit_tx_update("ChargingStateChanged", false),
            }
        }
        if let Some(every) = self.sample_every {
            self.since_sample += step;
            if self.since_sample >= every {
                self.since_sample -= every;
                match self.version {
                    Version::V1_6 => {
                        #[cfg(feature = "v1_6")]
                        v16::meter_values(self);
                    }
                    _ => self.emit_tx_update("MeterValuePeriodic", true),
                }
            }
        }
    }

    /// What the installed profiles allow at this instant.
    fn command_now(&self) -> Command {
        let transaction = self
            .tx
            .as_ref()
            // Before a 1.6 CSMS assigns the id, a TxDefaultProfile still applies; no TxProfile can.
            .map(|tx| (tx.id.as_deref().unwrap_or(""), tx.start));
        let composite =
            self.profiles
                .composite(self.evse_id, self.now, 1, RateUnit::A, transaction);
        let here = composite.periods.iter().find(|p| p.start_period == 0);
        let amps = here
            .and_then(|p| p.limit)
            .map_or(self.max_current, |limit| limit.min(self.max_current))
            .max(Decimal::ZERO);
        let phases = here
            .and_then(|p| p.number_phases)
            .and_then(|n| u8::try_from(n).ok())
            .filter(|n| (1..=3).contains(n));
        let mut command = Command::amps(amps);
        if let Some(n) = phases {
            command = command.phases(n);
        }
        #[cfg(feature = "v2_1")]
        if let Some(v2x) = self.v2x_command(amps, phases) {
            return v2x;
        }
        command
    }

    /// The 2.1 V2X profile leading now, as a command under the `amps` ceiling.
    #[cfg(feature = "v2_1")]
    fn v2x_command(&self, amps: Decimal, phases: Option<u8>) -> Option<Command> {
        use crate::v2_1::{
            ChargingProfileKind, ChargingProfilePurpose, ChargingRateUnit, OperationMode,
        };
        let tx = self.tx.as_ref()?;
        let tx_id = tx.id.as_deref();
        let at = self.now.timestamp().as_second();
        let leading = self
            .v2x
            .iter()
            .filter(|p| p.evse_id == 0 || p.evse_id == self.evse_id)
            .filter(|p| match p.profile.charging_profile_purpose {
                ChargingProfilePurpose::TxProfile => {
                    p.profile.transaction_id.is_none()
                        || p.profile.transaction_id.as_deref() == tx_id
                }
                ChargingProfilePurpose::TxDefaultProfile => true,
                _ => false,
            })
            .filter(|p| {
                p.profile
                    .valid_from
                    .is_none_or(|from| from.timestamp().as_second() <= at)
                    && p.profile
                        .valid_to
                        .is_none_or(|to| at < to.timestamp().as_second())
            })
            .filter_map(|p| {
                let schedule = p.profile.charging_schedule.first()?;
                let anchor = match p.profile.charging_profile_kind {
                    ChargingProfileKind::Dynamic => p.refreshed,
                    ChargingProfileKind::Relative => tx.start,
                    _ => schedule.start_schedule.unwrap_or(p.refreshed),
                }
                .timestamp()
                .as_second();
                let offset = at - anchor;
                // K28.FR.13: past `duration` without an update, the profile no longer applies.
                if offset < 0 || schedule.duration.is_some_and(|d| offset >= i64::from(d)) {
                    return None;
                }
                let period = schedule
                    .charging_schedule_period
                    .iter()
                    .rfind(|period| i64::from(period.start_period) <= offset)?;
                let rank = (
                    p.profile.charging_profile_purpose == ChargingProfilePurpose::TxProfile,
                    p.profile.stack_level,
                );
                Some((rank, schedule.charging_rate_unit.clone(), period))
            })
            .max_by_key(|(rank, _, _)| *rank)?;
        let (_, unit, period) = leading;
        let used = period
            .number_phases
            .and_then(|n| u8::try_from(n).ok())
            .or(phases)
            .unwrap_or(3);
        let watts = |value: Decimal| match unit {
            ChargingRateUnit::A => value
                .checked_mul(self.supply.voltage)
                .and_then(|w| w.checked_mul(Decimal::from(used)))
                .unwrap_or(value),
            _ => value,
        };
        let ceiling_w = amps
            .checked_mul(self.supply.voltage)
            .and_then(|w| w.checked_mul(Decimal::from(used)))
            .unwrap_or(Decimal::ZERO);
        let limit = period
            .limit
            .map_or(ceiling_w, |l| watts(l).min(ceiling_w))
            .max(Decimal::ZERO);
        let mut command = Command::watts(limit);
        if let Some(n) = phases.or_else(|| period.number_phases.and_then(|n| u8::try_from(n).ok()))
        {
            if (1..=3).contains(&n) {
                command = command.phases(n);
            }
        }
        if let Some(discharge) = period.discharge_limit {
            command =
                command.discharge_at_most(watts(discharge).checked_abs().unwrap_or(Decimal::ZERO));
        }
        let mode = period
            .operation_mode
            .clone()
            .unwrap_or(OperationMode::ChargingOnly);
        match mode {
            OperationMode::Idle => Some(Command::watts(0)),
            OperationMode::CentralSetpoint | OperationMode::ExternalSetpoint => {
                // The wire's sign says the direction; the vehicle takes a magnitude.
                let setpoint = watts(period.setpoint.unwrap_or(Decimal::ZERO));
                let magnitude = setpoint.checked_abs().unwrap_or(Decimal::ZERO);
                let target = if setpoint.signum() < 0 {
                    Power::Discharge(magnitude)
                } else {
                    Power::Charge(magnitude)
                };
                Some(command.target(target))
            }
            // ChargingOnly, ExternalLimits, LocalLoadBalancing and the frequency modes: charging
            // under the limit, which is what a station does without a local measurement.
            _ => Some(command),
        }
    }

    fn samples(&self, context: &'static str, full: bool) -> Vec<Sample> {
        let Some(ev) = &self.ev else {
            // Unplugged: the closing register is all there is.
            return vec![Sample::new(
                "Energy.Active.Import.Register",
                None,
                self.register,
                "Wh",
                context,
            )];
        };
        let reading = ev.reading();
        let (import, export) = self.register_at(&reading);
        let mut samples = vec![Sample::new(
            "Energy.Active.Import.Register",
            None,
            import,
            "Wh",
            context,
        )];
        if !full {
            return samples;
        }
        let power = reading.power;
        samples.push(Sample::new(
            "Power.Active.Import",
            None,
            power.max(Decimal::ZERO),
            "W",
            context,
        ));
        for phase in ["L1", "L2", "L3"]
            .into_iter()
            .take(usize::from(reading.phases))
        {
            samples.push(Sample::new(
                "Current.Import",
                Some(phase),
                reading.current,
                "A",
                context,
            ));
        }
        if ev.max_discharge_power().is_some() {
            samples.push(Sample::new(
                "Energy.Active.Export.Register",
                None,
                export,
                "Wh",
                context,
            ));
            samples.push(Sample::new(
                "Power.Active.Export",
                None,
                (-power).max(Decimal::ZERO),
                "W",
                context,
            ));
        }
        samples.push(Sample::new("SoC", None, reading.soc, "Percent", context));
        samples
    }

    fn emit_plug_in(&mut self) {
        match self.version {
            #[cfg(feature = "v1_6")]
            Version::V1_6 => v16::plug_in(self),
            #[cfg(feature = "v2_0_1")]
            Version::V2_0_1 => {
                v201::status(self, "Occupied");
                self.emit_tx_started();
                v201::needs(self);
            }
            #[cfg(feature = "v2_1")]
            Version::V2_1 => {
                v21::status(self, "Occupied");
                self.emit_tx_started();
                v21::needs(self);
            }
            #[allow(unreachable_patterns, clippy::allow_attributes)]
            _ => {}
        }
    }

    fn emit_unplug(&mut self) {
        match self.version {
            #[cfg(feature = "v1_6")]
            Version::V1_6 => v16::unplug(self),
            #[cfg(feature = "v2_0_1")]
            Version::V2_0_1 => {
                self.emit_tx(TxEvent::Ended);
                v201::status(self, "Available");
            }
            #[cfg(feature = "v2_1")]
            Version::V2_1 => {
                self.emit_tx(TxEvent::Ended);
                v21::status(self, "Available");
            }
            #[allow(unreachable_patterns, clippy::allow_attributes)]
            _ => {}
        }
    }

    fn emit_tx_started(&mut self) {
        self.emit_tx(TxEvent::Started);
    }

    fn emit_tx_update(&mut self, trigger: &'static str, full: bool) {
        self.emit_tx(TxEvent::Updated { trigger, full });
    }

    fn emit_tx(&mut self, event: TxEvent) {
        let Some(tx) = &mut self.tx else {
            return;
        };
        let seq = tx.seq;
        tx.seq += 1;
        let id = tx.id.clone().unwrap_or_default();
        let (kind, trigger, state, stopped, samples) = match event {
            TxEvent::Started => (
                "Started",
                "CablePluggedIn",
                "EVConnected",
                None,
                self.samples("Transaction.Begin", false),
            ),
            TxEvent::Updated { trigger, full } => (
                "Updated",
                trigger,
                self.state_name(),
                None,
                if full {
                    self.samples("Sample.Periodic", true)
                } else {
                    Vec::new()
                },
            ),
            TxEvent::Ended => (
                "Ended",
                "EVCommunicationLost",
                "Idle",
                Some("EVDisconnected"),
                self.samples("Transaction.End", false),
            ),
        };
        let event = TxEventIn {
            kind,
            trigger,
            seq,
            id: &id,
            state,
            stopped,
            samples: &samples,
            at: self.now,
            evse: self.evse_id,
        };
        let outgoing = match self.version {
            #[cfg(feature = "v2_0_1")]
            Version::V2_0_1 => v201::tx_event(&event),
            #[cfg(feature = "v2_1")]
            Version::V2_1 => v21::tx_event(&event),
            #[allow(unreachable_patterns, clippy::allow_attributes)]
            _ => return,
        };
        self.out.push(outgoing);
    }

    /// Installs a ceiling profile, replacing any profile with its id, V2X or not.
    fn install(&mut self, profile: crate::smart_charging::Profile) {
        #[cfg(feature = "v2_1")]
        self.v2x.retain(|p| p.profile.id != profile.id);
        self.profiles.install(profile);
    }

    /// Whether a profile of `purpose` (with `transaction`) may be installed now.
    fn accepts(&self, purpose: Purpose, transaction: Option<&str>, evse_id: u32) -> bool {
        if !purpose.settable_by_csms() || (evse_id != 0 && evse_id != self.evse_id) {
            return false;
        }
        if purpose == Purpose::TxProfile {
            // K01.FR.09 / 1.6 §3.13.4: a TxProfile belongs to a running transaction.
            let running = self.transaction_id();
            return evse_id != 0
                && running.is_some()
                && transaction.is_none_or(|id| Some(id) == running);
        }
        true
    }

    fn clear(
        &mut self,
        id: Option<i32>,
        evse_id: Option<u32>,
        purpose: Option<Purpose>,
        stack_level: Option<i32>,
    ) -> bool {
        let mut cleared = self.profiles.clear(id, evse_id, purpose, stack_level);
        #[cfg(feature = "v2_1")]
        {
            let before = self.v2x.len();
            self.v2x.retain(|p| {
                let matches = id.is_none_or(|id| p.profile.id == id)
                    && evse_id.is_none_or(|evse| p.evse_id == evse)
                    && purpose.is_none_or(|purpose| {
                        purpose_from(p.profile.charging_profile_purpose.as_str()) == Some(purpose)
                    })
                    && stack_level.is_none_or(|level| p.profile.stack_level == level);
                !matches
            });
            cleared += before - self.v2x.len();
        }
        cleared > 0
    }
}

#[derive(Clone, Copy)]
enum TxEvent {
    Started,
    Updated { trigger: &'static str, full: bool },
    Ended,
}

/// A transaction event, before it is spelled in a version.
struct TxEventIn<'a> {
    kind: &'static str,
    trigger: &'static str,
    seq: i32,
    id: &'a str,
    state: &'static str,
    stopped: Option<&'static str>,
    samples: &'a [Sample],
    at: DateTime,
    evse: u32,
}

/// One sampled value, before it is spelled in a version.
#[derive(Clone, Debug)]
struct Sample {
    measurand: &'static str,
    phase: Option<&'static str>,
    value: Decimal,
    unit: &'static str,
    context: &'static str,
}

impl Sample {
    fn new(
        measurand: &'static str,
        phase: Option<&'static str>,
        value: Decimal,
        unit: &'static str,
        context: &'static str,
    ) -> Self {
        Self {
            measurand,
            phase,
            value,
            unit,
            context,
        }
    }
}

/// A purpose as any version spells it.
fn purpose_from(text: &str) -> Option<Purpose> {
    Some(match text {
        "ChargePointMaxProfile" | "ChargingStationMaxProfile" => Purpose::ChargingStationMaxProfile,
        "TxDefaultProfile" => Purpose::TxDefaultProfile,
        "TxProfile" => Purpose::TxProfile,
        "ChargingStationExternalConstraints" => Purpose::ChargingStationExternalConstraints,
        "PriorityCharging" => Purpose::PriorityCharging,
        "LocalGeneration" => Purpose::LocalGeneration,
        _ => return None,
    })
}

/// The AC transfer mode for a phase count.
fn ac_mode(phases: u8) -> &'static str {
    match phases {
        1 => "AC_single_phase",
        2 => "AC_two_phase",
        _ => "AC_three_phase",
    }
}

fn json(value: &impl serde::Serialize) -> Result<String, CallError> {
    serde_json::to_string(value).map_err(|error| CallError::internal(error.to_string()))
}

fn int(value: Decimal) -> i32 {
    value
        .round_to(0)
        .and_then(|v| i32::try_from(v.mantissa()).ok())
        .unwrap_or(i32::MAX)
}

#[cfg(feature = "v1_6")]
mod v16 {
    use super::{
        CallError, Decimal, ID_TAG, Outgoing, Sample, String, ToString, Vec, Wallbox, decode, int,
        json, purpose_from,
    };
    use crate::smart_charging::Profile;
    use crate::v1_6 as w;

    pub(super) fn handle(
        wb: &mut Wallbox,
        action: &str,
        payload: &str,
    ) -> Option<Result<String, CallError>> {
        Some(match action {
            "SetChargingProfile" => set(wb, payload),
            "ClearChargingProfile" => clear(wb, payload),
            _ => return None,
        })
    }

    fn set(wb: &mut Wallbox, payload: &str) -> Result<String, CallError> {
        let request: w::SetChargingProfileRequest = decode::from_str(payload)?;
        let status = match (
            u32::try_from(request.connector_id).ok(),
            purpose_from(
                request
                    .cs_charging_profiles
                    .charging_profile_purpose
                    .as_str(),
            ),
        ) {
            (Some(connector), Some(purpose))
                if wb.accepts(
                    purpose,
                    request
                        .cs_charging_profiles
                        .transaction_id
                        .map(|id| id.to_string())
                        .as_deref(),
                    connector,
                ) =>
            {
                match Profile::from_v1_6(&request.cs_charging_profiles, connector) {
                    Ok(profile) => {
                        wb.install(profile);
                        w::ChargingProfileStatus::Accepted
                    }
                    Err(_) => w::ChargingProfileStatus::Rejected,
                }
            }
            _ => w::ChargingProfileStatus::Rejected,
        };
        json(&w::SetChargingProfileResponse::new(status))
    }

    fn clear(wb: &mut Wallbox, payload: &str) -> Result<String, CallError> {
        let request: w::ClearChargingProfileRequest = decode::from_str(payload)?;
        let cleared = wb.clear(
            request.id,
            request.connector_id.and_then(|c| u32::try_from(c).ok()),
            request
                .charging_profile_purpose
                .as_ref()
                .and_then(|p| purpose_from(p.as_str())),
            request.stack_level,
        );
        json(&w::ClearChargingProfileResponse::new(if cleared {
            w::ClearChargingProfileStatus::Accepted
        } else {
            w::ClearChargingProfileStatus::Unknown
        }))
    }

    pub(super) fn status(wb: &mut Wallbox, status: &str) {
        let mut request = w::StatusNotificationRequest::new(
            connector(wb),
            w::ChargePointErrorCode::NoError,
            w::ChargePointStatus::from_wire(status),
        );
        request.timestamp = Some(wb.now);
        wb.out.push(Outgoing::new("StatusNotification", &request));
    }

    /// 1.6 §6.45/§6.49: `meterStart` and `meterStop` are integers in Wh, so every register the
    /// station reports is whole Wh — truncated, as a register counts: a Wh appears once it is
    /// complete, and a billable figure never runs ahead of the energy delivered.
    pub(super) fn whole_wh(register: Decimal) -> i32 {
        int(register.trunc_to(0).unwrap_or(register))
    }

    fn is_register(measurand: &str) -> bool {
        measurand.starts_with("Energy.") && measurand.ends_with(".Register")
    }

    fn connector(wb: &Wallbox) -> i32 {
        i32::try_from(wb.evse_id).unwrap_or(1)
    }

    fn register(wb: &Wallbox) -> Decimal {
        wb.ev
            .as_ref()
            .map_or(wb.register, |ev| wb.register_at(&ev.reading()).0)
    }

    pub(super) fn plug_in(wb: &mut Wallbox) {
        status(wb, "Preparing");
        let start =
            w::StartTransactionRequest::new(connector(wb), ID_TAG, whole_wh(register(wb)), wb.now);
        wb.out.push(Outgoing::new("StartTransaction", &start));
    }

    pub(super) fn answered(wb: &mut Wallbox, payload: &str) {
        let Ok(response) = decode::from_str::<w::StartTransactionResponse>(payload) else {
            return;
        };
        let Some(tx) = wb.tx.as_mut().filter(|tx| tx.id.is_none()) else {
            return;
        };
        tx.id = Some(response.transaction_id.to_string());
        if let Some((at, register)) = tx.held_stop {
            stop(wb, at, register);
            wb.tx = None;
        }
    }

    fn sampled(samples: &[Sample]) -> Vec<w::SampledValue> {
        samples
            .iter()
            .map(|s| {
                // One register value per instant: the same whole Wh `meterStop` carries.
                let value = if is_register(s.measurand) {
                    Decimal::from(whole_wh(s.value))
                } else {
                    s.value
                };
                let mut value = w::SampledValue::new(value.to_string());
                value.measurand = Some(w::Measurand::from_wire(s.measurand));
                value.phase = s.phase.map(w::Phase::from_wire);
                value.unit = Some(w::UnitOfMeasure::from_wire(s.unit));
                value.context = Some(w::ReadingContext::from_wire(s.context));
                value
            })
            .collect()
    }

    pub(super) fn meter_values(wb: &mut Wallbox) {
        // Never with an id the CSMS did not assign: before the answer, the sample is skipped.
        let Some(id) = wb
            .tx
            .as_ref()
            .and_then(|tx| tx.id.as_deref()?.parse::<i32>().ok())
        else {
            return;
        };
        let values = sampled(&wb.samples("Sample.Periodic", true));
        let mut request =
            w::MeterValuesRequest::new(connector(wb), vec![w::MeterValue::new(wb.now, values)]);
        request.transaction_id = Some(id);
        wb.out.push(Outgoing::new("MeterValues", &request));
    }

    /// `StopTransaction` as of `at`, with the register as it stood then.
    fn stop(wb: &mut Wallbox, at: crate::types::DateTime, register: Decimal) {
        let Some(id) = wb
            .tx
            .as_ref()
            .and_then(|tx| tx.id.as_deref()?.parse::<i32>().ok())
        else {
            return;
        };
        let mut request = w::StopTransactionRequest::new(whole_wh(register), at, id);
        request.reason = Some(w::Reason::EVDisconnected);
        request.id_tag = Some(ID_TAG.into());
        wb.out.push(Outgoing::new("StopTransaction", &request));
    }

    pub(super) fn unplug(wb: &mut Wallbox) {
        status(wb, "Finishing");
        let (at, register) = (wb.now, wb.register);
        match wb.tx.as_mut() {
            Some(tx) if tx.id.is_none() => tx.held_stop = Some((at, register)),
            Some(_) => stop(wb, at, register),
            None => {}
        }
        status(wb, "Available");
    }
}

/// The 2.x messages, spelled once for two type sets that share their shape.
macro_rules! wallbox_2x {
    ($module:ident, $version:ident, $v2x:path) => {
        mod $module {
            use super::{
                CallError, FromWire, Outgoing, Sample, String, TxEventIn, Wallbox, decode, json,
                purpose_from,
            };
            use crate::smart_charging::Profile;
            use crate::$version as w;

            pub(super) fn status(wb: &mut Wallbox, status: &str) {
                let request = w::StatusNotificationRequest::new(
                    wb.now,
                    w::ConnectorStatus::from_wire(status),
                    i32::try_from(wb.evse_id).unwrap_or(1),
                    1,
                );
                wb.out.push(Outgoing::new("StatusNotification", &request));
            }

            pub(super) fn sampled(samples: &[Sample]) -> Vec<w::SampledValue> {
                samples
                    .iter()
                    .map(|s| {
                        let mut value = w::SampledValue::new(s.value);
                        value.measurand = Some(w::Measurand::from_wire(s.measurand));
                        value.phase = s.phase.map(w::Phase::from_wire);
                        value.context = Some(w::ReadingContext::from_wire(s.context));
                        let mut unit = w::UnitOfMeasure::new();
                        unit.unit = Some(s.unit.into());
                        value.unit_of_measure = Some(unit);
                        value
                    })
                    .collect()
            }

            pub(super) fn tx_event(event: &TxEventIn<'_>) -> Outgoing {
                let mut info = w::Transaction::new(event.id);
                info.charging_state = Some(w::ChargingState::from_wire(event.state));
                info.stopped_reason = event.stopped.map(w::Reason::from_wire);
                let mut request = w::TransactionEventRequest::new(
                    w::TransactionEventEnum::from_wire(event.kind),
                    event.at,
                    w::TriggerReason::from_wire(event.trigger),
                    event.seq,
                    info,
                );
                if event.kind == "Started" {
                    let mut evse = w::EVSE::new(i32::try_from(event.evse).unwrap_or(1));
                    evse.connector_id = Some(1);
                    request.evse = Some(evse);
                }
                if !event.samples.is_empty() {
                    request.meter_value =
                        Some(vec![w::MeterValue::new(sampled(event.samples), event.at)]);
                }
                Outgoing::new("TransactionEvent", &request)
            }

            pub(super) fn set(wb: &mut Wallbox, payload: &str) -> Result<String, CallError> {
                let request: w::SetChargingProfileRequest = decode::from_str(payload)?;
                let profile = &request.charging_profile;
                let accepted = u32::try_from(request.evse_id).ok().is_some_and(|evse| {
                    purpose_from(profile.charging_profile_purpose.as_str()).is_some_and(|purpose| {
                        wb.accepts(purpose, profile.transaction_id.as_deref(), evse)
                    }) && install(wb, profile, evse)
                });
                json(&w::SetChargingProfileResponse::new(if accepted {
                    w::ChargingProfileStatus::Accepted
                } else {
                    w::ChargingProfileStatus::Rejected
                }))
            }

            fn install(wb: &mut Wallbox, profile: &w::ChargingProfile, evse: u32) -> bool {
                match Profile::from_wire(profile, evse) {
                    Ok(domain) => {
                        wb.install(domain);
                        true
                    }
                    Err(_) => $v2x(wb, profile, evse),
                }
            }

            pub(super) fn clear(wb: &mut Wallbox, payload: &str) -> Result<String, CallError> {
                let request: w::ClearChargingProfileRequest = decode::from_str(payload)?;
                let criteria = request.charging_profile_criteria.as_ref();
                let cleared = wb.clear(
                    request.charging_profile_id,
                    criteria
                        .and_then(|c| c.evse_id)
                        .and_then(|e| u32::try_from(e).ok()),
                    criteria
                        .and_then(|c| c.charging_profile_purpose.as_ref())
                        .and_then(|p| purpose_from(p.as_str())),
                    criteria.and_then(|c| c.stack_level),
                );
                json(&w::ClearChargingProfileResponse::new(if cleared {
                    w::ClearChargingProfileStatus::Accepted
                } else {
                    w::ClearChargingProfileStatus::Unknown
                }))
            }
        }
    };
}

/// `Profile::from_v2_0_1` / `from_v2_1` under one name for the macro.
trait FromWire<P> {
    fn from_wire(
        wire: &P,
        evse: u32,
    ) -> Result<crate::smart_charging::Profile, crate::smart_charging::ConversionError>;
}

#[cfg(feature = "v2_0_1")]
impl FromWire<crate::v2_0_1::ChargingProfile> for crate::smart_charging::Profile {
    fn from_wire(
        wire: &crate::v2_0_1::ChargingProfile,
        evse: u32,
    ) -> Result<Self, crate::smart_charging::ConversionError> {
        Self::from_v2_0_1(wire, evse)
    }
}

#[cfg(feature = "v2_1")]
impl FromWire<crate::v2_1::ChargingProfile> for crate::smart_charging::Profile {
    fn from_wire(
        wire: &crate::v2_1::ChargingProfile,
        evse: u32,
    ) -> Result<Self, crate::smart_charging::ConversionError> {
        Self::from_v2_1(wire, evse)
    }
}

#[cfg(feature = "v2_0_1")]
wallbox_2x!(v201_common, v2_0_1, super::install_v2x_201);

#[cfg(feature = "v2_1")]
wallbox_2x!(v21_common, v2_1, super::v21::install);

#[cfg(feature = "v2_0_1")]
mod v201 {
    use super::v201_common::{clear, set};
    pub(super) use super::v201_common::{status, tx_event};
    use super::{CallError, Outgoing, String, Wallbox, int};
    use crate::v2_0_1 as w;

    pub(super) fn handle(
        wb: &mut Wallbox,
        action: &str,
        payload: &str,
    ) -> Option<Result<String, CallError>> {
        Some(match action {
            "SetChargingProfile" => set(wb, payload),
            "ClearChargingProfile" => clear(wb, payload),
            _ => return None,
        })
    }

    pub(super) fn needs(wb: &mut Wallbox) {
        let Some(ev) = &wb.ev else { return };
        let (min, max) = ev.current_range();
        let mode = w::EnergyTransferMode::from_wire(super::ac_mode(ev.phases()));
        let mut needs = w::ChargingNeeds::new(mode);
        needs.ac_charging_parameters = Some(w::ACChargingParameters::new(
            int(ev.energy_request()),
            int(min),
            int(max),
            int(ev.voltage()),
        ));
        needs.departure_time = ev.departure();
        let request =
            w::NotifyEVChargingNeedsRequest::new(needs, i32::try_from(wb.evse_id).unwrap_or(1));
        wb.out
            .push(Outgoing::new("NotifyEVChargingNeeds", &request));
    }
}

#[cfg(feature = "v2_0_1")]
fn install_v2x_201(_: &mut Wallbox, _: &crate::v2_0_1::ChargingProfile, _: u32) -> bool {
    false
}

#[cfg(feature = "v2_1")]
mod v21 {
    use super::v21_common::{clear, set};
    pub(super) use super::v21_common::{status, tx_event};
    use super::{CallError, Decimal, Outgoing, String, V2xProfile, Wallbox, decode, json};
    use crate::v2_1 as w;

    pub(super) fn handle(
        wb: &mut Wallbox,
        action: &str,
        payload: &str,
    ) -> Option<Result<String, CallError>> {
        Some(match action {
            "SetChargingProfile" => set(wb, payload),
            "ClearChargingProfile" => clear(wb, payload),
            "UpdateDynamicSchedule" => update(wb, payload),
            _ => return None,
        })
    }

    /// A profile `Profile::from_v2_1` refused because it carries V2X periods or is `Dynamic`.
    pub(super) fn install(wb: &mut Wallbox, profile: &w::ChargingProfile, evse: u32) -> bool {
        let v2x = profile.charging_profile_kind == w::ChargingProfileKind::Dynamic
            || profile
                .charging_schedule
                .iter()
                .flat_map(|s| &s.charging_schedule_period)
                .any(|p| {
                    p.operation_mode
                        .as_ref()
                        .is_some_and(|m| *m != w::OperationMode::ChargingOnly)
                        || p.setpoint.is_some()
                        || p.discharge_limit.is_some()
                });
        if !v2x || profile.charging_profile_kind == w::ChargingProfileKind::Recurring {
            return false;
        }
        wb.profiles.clear(Some(profile.id), None, None, None);
        wb.v2x.retain(|p| p.profile.id != profile.id);
        wb.v2x.push(V2xProfile {
            profile: profile.clone(),
            evse_id: evse,
            refreshed: wb.now,
        });
        true
    }

    fn update(wb: &mut Wallbox, payload: &str) -> Result<String, CallError> {
        let request: w::UpdateDynamicScheduleRequest = decode::from_str(payload)?;
        let now = wb.now;
        let target = wb.v2x.iter_mut().find(|p| {
            p.profile.id == request.charging_profile_id
                && p.profile.charging_profile_kind == w::ChargingProfileKind::Dynamic
        });
        let status = match target.and_then(|p| {
            p.refreshed = now;
            p.profile
                .charging_schedule
                .first_mut()?
                .charging_schedule_period
                .first_mut()
        }) {
            Some(period) => {
                let update = &request.schedule_update;
                let set = |field: &mut Option<Decimal>, value: Option<Decimal>| {
                    if value.is_some() {
                        *field = value;
                    }
                };
                set(&mut period.limit, update.limit);
                set(&mut period.discharge_limit, update.discharge_limit);
                set(&mut period.setpoint, update.setpoint);
                w::ChargingProfileStatus::Accepted
            }
            None => w::ChargingProfileStatus::Rejected,
        };
        json(&w::UpdateDynamicScheduleResponse::new(status))
    }

    pub(super) fn needs(wb: &mut Wallbox) {
        let Some(ev) = &wb.ev else { return };
        let (min, max) = ev.current_range();
        let mode = w::EnergyTransferMode::from_wire(if ev.max_discharge_power().is_some() {
            "AC_BPT"
        } else {
            super::ac_mode(ev.phases())
        });
        let mut needs = w::ChargingNeeds::new(mode);
        needs.ac_charging_parameters = Some(w::ACChargingParameters::new(
            ev.energy_request(),
            min,
            max,
            ev.voltage(),
        ));
        if let Some(discharge) = ev.max_discharge_power() {
            let mut v2x = w::V2XChargingParameters::new();
            v2x.max_charge_power = Some(ev.max_charge_power());
            v2x.max_discharge_power = Some(discharge);
            v2x.ev_target_energy_request = Some(ev.energy_request());
            v2x.target_so_c = Some(super::int(ev.target_soc()));
            needs.v2x_charging_parameters = Some(v2x);
        }
        needs.departure_time = ev.departure();
        let mut request =
            w::NotifyEVChargingNeedsRequest::new(i32::try_from(wb.evse_id).unwrap_or(1), needs);
        request.timestamp = Some(wb.now);
        wb.out
            .push(Outgoing::new("NotifyEVChargingNeeds", &request));
    }
}
