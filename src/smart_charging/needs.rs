//! What the EV needs, from `NotifyEVChargingNeedsRequest` (2.0.1 and 2.1).

use alloc::string::{String, ToString};

use super::{DEFAULT_PHASES, Supply};
use crate::types::{DateTime, Decimal};

/// What the EV needs and can do, as the station relayed it in `NotifyEVChargingNeedsRequest`.
///
/// One view of three parameter sets that use different units and phase conventions:
///
/// * **AC** (`acChargingParameters`, 2.0.1 and 2.1): `energyAmount` in Wh, including
///   preconditioning; `evMinCurrent` / `evMaxCurrent` in **A per phase**; `evMaxVoltage`.
/// * **DC** (`dcChargingParameters`): `evMaxCurrent` / `evMaxVoltage` on the DC side,
///   `evMaxPower`, `evEnergyCapacity`, `energyAmount`, `stateOfCharge`.
/// * **V2X** (2.1 `v2xChargingParameters`): powers in W as **magnitudes**, whatever the
///   direction; when `_L2`/`_L3` values are given, the plain field is phase L1 only and the
///   totals here are the sums (§2.155).
///
/// The number of phases comes from the transfer mode (`AC_single_phase`, `AC_two_phase`,
/// `AC_three_phase`); a bidirectional or DC mode does not say.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct EvNeeds {
    /// The energy transfer the EV asked for, e.g. `AC_three_phase` or `AC_BPT`.
    pub requested: String,
    /// Whether that transfer is bidirectional (`AC_BPT`, `AC_BPT_DER`, `DC_BPT`, `DC_ACDP_BPT`).
    pub bidirectional: bool,
    /// Phases the AC transfer uses, from the transfer mode.
    pub phases: Option<u8>,
    /// When the EV leaves.
    pub departure: Option<DateTime>,
    /// The energy the EV asks for, in Wh: AC/DC `energyAmount`, else V2X `evTargetEnergyRequest`.
    pub energy: Option<Decimal>,
    /// The energy it needs at least, in Wh (V2X `evMinEnergyRequest`).
    pub min_energy: Option<Decimal>,
    /// The energy it can take at most, in Wh (V2X `evMaxEnergyRequest`).
    pub max_energy: Option<Decimal>,
    /// Least current it charges with: AC per phase, in A.
    pub min_current: Option<Decimal>,
    /// Most current it charges with: AC per phase, or DC, in A.
    pub max_current: Option<Decimal>,
    /// Highest voltage it accepts, in V.
    pub max_voltage: Option<Decimal>,
    /// Least power it usefully charges with, in W (V2X).
    pub min_charge: Option<Decimal>,
    /// Most power it charges with, in W: V2X `maxChargePower`, else DC `evMaxPower`. For AC
    /// without either, see [`max_charge_power`](Self::max_charge_power).
    pub max_charge: Option<Decimal>,
    /// Least power it usefully discharges with, in W (V2X).
    pub min_discharge: Option<Decimal>,
    /// Most power it discharges with, in W (V2X).
    pub max_discharge: Option<Decimal>,
    /// Its state of charge, in % (DC).
    pub soc: Option<i32>,
    /// The state of charge it aims for, in % (V2X).
    pub target_soc: Option<i32>,
    /// Its battery capacity, in Wh (DC).
    pub capacity: Option<Decimal>,
}

impl EvNeeds {
    fn new(requested: &str, departure: Option<DateTime>) -> Self {
        Self {
            requested: requested.to_string(),
            bidirectional: matches!(
                requested,
                "AC_BPT" | "AC_BPT_DER" | "DC_BPT" | "DC_ACDP_BPT"
            ),
            phases: match requested {
                "AC_single_phase" => Some(1),
                "AC_two_phase" => Some(2),
                "AC_three_phase" => Some(3),
                _ => None,
            },
            departure,
            ..Self::default()
        }
    }

    /// Reads an OCPP 2.0.1 `ChargingNeeds`.
    #[cfg(feature = "v2_0_1")]
    #[must_use]
    pub fn from_v2_0_1(needs: &crate::v2_0_1::ChargingNeeds) -> Self {
        let mut ev = Self::new(
            needs.requested_energy_transfer.as_str(),
            needs.departure_time,
        );
        if let Some(ac) = &needs.ac_charging_parameters {
            ev.energy = Some(ac.energy_amount.into());
            ev.min_current = Some(ac.ev_min_current.into());
            ev.max_current = Some(ac.ev_max_current.into());
            ev.max_voltage = Some(ac.ev_max_voltage.into());
        }
        if let Some(dc) = &needs.dc_charging_parameters {
            ev.max_current = Some(dc.ev_max_current.into());
            ev.max_voltage = Some(dc.ev_max_voltage.into());
            ev.energy = dc.energy_amount.map(Decimal::from).or(ev.energy);
            ev.max_charge = dc.ev_max_power.map(Decimal::from);
            ev.capacity = dc.ev_energy_capacity.map(Decimal::from);
            ev.soc = dc.state_of_charge;
        }
        ev
    }

    /// Reads an OCPP 2.1 `ChargingNeeds`.
    #[cfg(feature = "v2_1")]
    #[must_use]
    pub fn from_v2_1(needs: &crate::v2_1::ChargingNeeds) -> Self {
        let mut ev = Self::new(
            needs.requested_energy_transfer.as_str(),
            needs.departure_time,
        );
        if let Some(ac) = &needs.ac_charging_parameters {
            ev.energy = Some(ac.energy_amount);
            ev.min_current = Some(ac.ev_min_current);
            ev.max_current = Some(ac.ev_max_current);
            ev.max_voltage = Some(ac.ev_max_voltage);
        }
        if let Some(dc) = &needs.dc_charging_parameters {
            ev.max_current = Some(dc.ev_max_current);
            ev.max_voltage = Some(dc.ev_max_voltage);
            ev.energy = dc.energy_amount.or(ev.energy);
            ev.max_charge = dc.ev_max_power;
            ev.capacity = dc.ev_energy_capacity;
            ev.soc = dc.state_of_charge;
        }
        if let Some(v2x) = &needs.v2x_charging_parameters {
            let sum = |base, l2, l3| total(base, l2, l3);
            ev.max_charge = sum(
                v2x.max_charge_power,
                v2x.max_charge_power_l2,
                v2x.max_charge_power_l3,
            )
            .or(ev.max_charge);
            ev.min_charge = sum(
                v2x.min_charge_power,
                v2x.min_charge_power_l2,
                v2x.min_charge_power_l3,
            );
            ev.max_discharge = sum(
                v2x.max_discharge_power,
                v2x.max_discharge_power_l2,
                v2x.max_discharge_power_l3,
            );
            ev.min_discharge = sum(
                v2x.min_discharge_power,
                v2x.min_discharge_power_l2,
                v2x.min_discharge_power_l3,
            );
            ev.min_current = ev.min_current.or(v2x.min_charge_current);
            ev.max_current = ev.max_current.or(v2x.max_charge_current);
            ev.max_voltage = ev.max_voltage.or(v2x.max_voltage);
            ev.energy = ev.energy.or(v2x.ev_target_energy_request);
            ev.min_energy = v2x.ev_min_energy_request;
            ev.max_energy = v2x.ev_max_energy_request;
            ev.target_soc = v2x.target_so_c;
        }
        ev
    }

    /// The most power the EV charges with, in W.
    ///
    /// [`max_charge`](Self::max_charge) when the EV reported a power; otherwise, for an AC
    /// transfer, `max_current × voltage × phases` at the supply's nominal voltage — three phases
    /// when the transfer mode does not say (K01.FR.49). `None` for DC without a power, where the
    /// DC current says nothing about the AC side.
    #[must_use]
    pub fn max_charge_power(&self, supply: Supply) -> Option<Decimal> {
        if self.max_charge.is_some() {
            return self.max_charge;
        }
        if !self.requested.starts_with("AC") {
            return None;
        }
        let phases = self
            .phases
            .map_or(Decimal::from(DEFAULT_PHASES), Decimal::from);
        self.max_current?
            .checked_mul(supply.voltage)?
            .checked_mul(phases)
    }
}

/// A total over all phases: the plain field alone, or — when `_L2`/`_L3` are given and the plain
/// field is therefore L1 — the sum.
#[cfg(feature = "v2_1")]
fn total(base: Option<Decimal>, l2: Option<Decimal>, l3: Option<Decimal>) -> Option<Decimal> {
    if l2.is_none() && l3.is_none() {
        return base;
    }
    [base, l2, l3]
        .into_iter()
        .flatten()
        .try_fold(Decimal::ZERO, Decimal::checked_add)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(value: i64) -> Decimal {
        Decimal::from(value)
    }

    #[cfg(feature = "v2_0_1")]
    #[test]
    fn ac_needs_read_in_amperes_per_phase_and_watt_hours() {
        let needs: crate::v2_0_1::ChargingNeeds = serde_json::from_str(
            r#"{"requestedEnergyTransfer":"AC_three_phase","departureTime":"2026-11-21T07:00:00Z",
                "acChargingParameters":{"energyAmount":20000,"evMinCurrent":6,"evMaxCurrent":16,
                "evMaxVoltage":400}}"#,
        )
        .unwrap();
        let ev = EvNeeds::from_v2_0_1(&needs);
        assert_eq!(ev.energy, Some(w(20_000)));
        assert_eq!((ev.min_current, ev.max_current), (Some(w(6)), Some(w(16))));
        assert_eq!(ev.phases, Some(3));
        assert!(!ev.bidirectional);
        assert_eq!(ev.departure, Some("2026-11-21T07:00:00Z".parse().unwrap()));
        assert_eq!(ev.max_charge_power(Supply::default()), Some(w(11_040)));

        let single: crate::v2_0_1::ChargingNeeds = serde_json::from_str(
            r#"{"requestedEnergyTransfer":"AC_single_phase","acChargingParameters":{
                "energyAmount":5000,"evMinCurrent":6,"evMaxCurrent":32,"evMaxVoltage":230}}"#,
        )
        .unwrap();
        let single = EvNeeds::from_v2_0_1(&single);
        assert_eq!(single.phases, Some(1));
        assert_eq!(single.max_charge_power(Supply::default()), Some(w(7_360)));
    }

    #[cfg(feature = "v2_0_1")]
    #[test]
    fn dc_needs_carry_soc_capacity_and_power() {
        let needs: crate::v2_0_1::ChargingNeeds = serde_json::from_str(
            r#"{"requestedEnergyTransfer":"DC","dcChargingParameters":{"evMaxCurrent":200,
                "evMaxVoltage":800,"evMaxPower":150000,"stateOfCharge":40,"evEnergyCapacity":77000,
                "energyAmount":30000}}"#,
        )
        .unwrap();
        let ev = EvNeeds::from_v2_0_1(&needs);
        assert_eq!((ev.soc, ev.capacity), (Some(40), Some(w(77_000))));
        assert_eq!(ev.max_charge_power(Supply::default()), Some(w(150_000)));
        assert_eq!(ev.phases, None);

        let no_power: crate::v2_0_1::ChargingNeeds = serde_json::from_str(
            r#"{"requestedEnergyTransfer":"DC","dcChargingParameters":{"evMaxCurrent":200,
                "evMaxVoltage":800}}"#,
        )
        .unwrap();
        assert_eq!(
            EvNeeds::from_v2_0_1(&no_power).max_charge_power(Supply::default()),
            None,
            "a DC current says nothing about AC power"
        );
    }

    #[cfg(feature = "v2_1")]
    #[test]
    fn v2x_needs_read_in_magnitudes_and_sum_their_phases() {
        let needs: crate::v2_1::ChargingNeeds = serde_json::from_str(
            r#"{"requestedEnergyTransfer":"AC_BPT","acChargingParameters":{"energyAmount":18000,
                "evMinCurrent":6,"evMaxCurrent":16,"evMaxVoltage":400},"v2xChargingParameters":{
                "maxChargePower":11000,"maxDischargePower":2000,"maxDischargePower_L2":2000,
                "maxDischargePower_L3":2000,"evTargetEnergyRequest":20000,"targetSoC":80}}"#,
        )
        .unwrap();
        let ev = EvNeeds::from_v2_1(&needs);
        assert!(ev.bidirectional);
        assert_eq!(ev.max_charge, Some(w(11_000)));
        assert_eq!(
            ev.max_discharge,
            Some(w(6_000)),
            "the plain field is L1 when L2/L3 are given"
        );
        assert_eq!(
            ev.energy,
            Some(w(18_000)),
            "the AC energyAmount is the request"
        );
        assert_eq!(ev.target_soc, Some(80));
        assert_eq!(ev.phases, None, "AC_BPT does not name its phases");
        assert_eq!((ev.min_current, ev.max_current), (Some(w(6)), Some(w(16))));
    }
}
