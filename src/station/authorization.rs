//! Local authorization: the list, the cache, and the rules that decide between them.
//!
//! Blocks C (Authorization) and D (Local Authorization List Management). A Charging Station
//! that is offline still has to answer the driver, and the specification is precise about
//! where the answer comes from: the local authorization list first (it is authoritative and
//! operator-managed), then the authorization cache (it is a memory of what the CSMS said
//! before), and only then the CSMS itself.

use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::fmt;

use crate::types::DateTime;

/// The verdict on an `IdToken` (`AuthorizationStatusEnumType`, and 1.6's
/// `AuthorizationStatus`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum AuthorizationStatus {
    /// The token may charge.
    Accepted,
    /// The token is barred.
    Blocked,
    /// The token is already in a transaction elsewhere.
    ConcurrentTx,
    /// The token's `cacheExpiryDateTime` has passed.
    Expired,
    /// The token is not valid.
    Invalid,
    /// The token is not known here.
    Unknown,
}

impl AuthorizationStatus {
    /// The wire spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            AuthorizationStatus::Accepted => "Accepted",
            AuthorizationStatus::Blocked => "Blocked",
            AuthorizationStatus::ConcurrentTx => "ConcurrentTx",
            AuthorizationStatus::Expired => "Expired",
            AuthorizationStatus::Invalid => "Invalid",
            AuthorizationStatus::Unknown => "Unknown",
        }
    }
}

impl fmt::Display for AuthorizationStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What is known about one `IdToken`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IdTokenInfo {
    /// The verdict.
    pub status: AuthorizationStatus,
    /// When this entry stops being usable.
    pub expires: Option<DateTime>,
    /// The group the token belongs to, for "any card of this fleet may stop this
    /// transaction".
    pub group_id: Option<String>,
    /// The language to show messages in.
    pub language: Option<String>,
}

impl IdTokenInfo {
    /// An accepted token with no further constraints.
    #[must_use]
    pub fn accepted() -> Self {
        Self {
            status: AuthorizationStatus::Accepted,
            expires: None,
            group_id: None,
            language: None,
        }
    }

    /// A token with the given verdict.
    #[must_use]
    pub fn new(status: AuthorizationStatus) -> Self {
        Self {
            status,
            expires: None,
            group_id: None,
            language: None,
        }
    }

    /// Sets the expiry.
    #[must_use]
    pub fn expiring(mut self, at: DateTime) -> Self {
        self.expires = Some(at);
        self
    }

    /// Sets the group id.
    #[must_use]
    pub fn in_group(mut self, group_id: impl Into<String>) -> Self {
        self.group_id = Some(group_id.into());
        self
    }

    /// The status as of `now`, downgrading to `Expired` once the expiry has passed.
    #[must_use]
    pub fn status_at(&self, now: DateTime) -> AuthorizationStatus {
        match self.expires {
            Some(expiry) if expiry <= now => AuthorizationStatus::Expired,
            _ => self.status,
        }
    }
}

/// How `SendLocalList` updates the list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UpdateType {
    /// Replace the whole list.
    Full,
    /// Apply the entries as a patch; an entry without `idTokenInfo` is a deletion.
    Differential,
}

/// The result of a `SendLocalList` (`UpdateStatusEnumType`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum UpdateStatus {
    /// Applied.
    Accepted,
    /// Refused.
    Failed,
    /// The station does not do local lists.
    NotSupported,
    /// A differential update arrived with a version that is not greater than the current one.
    VersionMismatch,
}

/// The operator-managed local authorization list.
///
/// Versioned: `GetLocalListVersion` reports [`version`](Self::version), and a differential
/// update whose version does not advance is refused (D01) — which is what stops two
/// out-of-order updates from silently reordering the list.
#[derive(Clone, Debug, Default)]
pub struct LocalAuthorizationList {
    version: i32,
    entries: BTreeMap<String, IdTokenInfo>,
    limit: Option<usize>,
}

impl LocalAuthorizationList {
    /// An empty list at version 0.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// An empty list that refuses to hold more than `limit` entries
    /// (`LocalAuthListCtrlr.Entries`).
    #[must_use]
    pub fn bounded(limit: usize) -> Self {
        Self {
            limit: Some(limit),
            ..Self::default()
        }
    }

    /// The list version, as `GetLocalListVersion` reports it. `0` means "no list".
    #[must_use]
    pub const fn version(&self) -> i32 {
        self.version
    }

    /// How many tokens the list holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the list is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Applies a `SendLocalList`.
    ///
    /// `entries` maps an `idToken` to its info; `None` deletes the token, which is only
    /// meaningful in a differential update.
    pub fn update(
        &mut self,
        update: UpdateType,
        version: i32,
        entries: Vec<(String, Option<IdTokenInfo>)>,
    ) -> UpdateStatus {
        if update == UpdateType::Differential && version <= self.version {
            return UpdateStatus::VersionMismatch;
        }
        let projected = match update {
            UpdateType::Full => entries.iter().filter(|(_, info)| info.is_some()).count(),
            UpdateType::Differential => {
                let added = entries
                    .iter()
                    .filter(|(token, info)| info.is_some() && !self.entries.contains_key(token))
                    .count();
                let removed = entries
                    .iter()
                    .filter(|(token, info)| info.is_none() && self.entries.contains_key(token))
                    .count();
                self.entries.len() + added - removed
            }
        };
        if self.limit.is_some_and(|limit| projected > limit) {
            return UpdateStatus::Failed;
        }

        if update == UpdateType::Full {
            self.entries.clear();
        }
        for (token, info) in entries {
            match info {
                Some(info) => {
                    self.entries.insert(token, info);
                }
                None => {
                    self.entries.remove(&token);
                }
            }
        }
        self.version = version;
        UpdateStatus::Accepted
    }

    /// Empties the list and resets its version, as a `SendLocalList(Full)` with no entries
    /// does.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.version = 0;
    }

    /// Looks a token up.
    #[must_use]
    pub fn get(&self, id_token: &str) -> Option<&IdTokenInfo> {
        self.entries.get(id_token)
    }
}

/// The authorization cache: what the CSMS said, remembered for when it cannot be asked.
///
/// Bounded and least-recently-used, because a Charging Station has finite memory and an
/// unbounded cache is a slow leak. `AuthCacheCtrlr.Enabled` decides whether it is consulted
/// at all.
#[derive(Clone, Debug)]
pub struct AuthorizationCache {
    entries: BTreeMap<String, (u64, IdTokenInfo)>,
    capacity: usize,
    clock: u64,
    enabled: bool,
}

impl Default for AuthorizationCache {
    fn default() -> Self {
        Self::with_capacity(512)
    }
}

impl AuthorizationCache {
    /// A cache holding at most `capacity` tokens.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            entries: BTreeMap::new(),
            capacity: capacity.max(1),
            clock: 0,
            enabled: true,
        }
    }

    /// Turns the cache on or off (`AuthCacheCtrlr.Enabled`).
    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
        if !enabled {
            self.entries.clear();
        }
    }

    /// Whether the cache is in use.
    #[must_use]
    pub const fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// How many tokens are cached.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the cache is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Records what the CSMS answered.
    pub fn remember(&mut self, id_token: impl Into<String>, info: IdTokenInfo) {
        if !self.enabled {
            return;
        }
        self.clock += 1;
        let stamp = self.clock;
        self.entries.insert(id_token.into(), (stamp, info));
        while self.entries.len() > self.capacity {
            if let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, (stamp, _))| *stamp)
                .map(|(token, _)| token.clone())
            {
                self.entries.remove(&oldest);
            } else {
                break;
            }
        }
    }

    /// Looks a token up, refreshing its recency.
    pub fn get(&mut self, id_token: &str) -> Option<IdTokenInfo> {
        if !self.enabled {
            return None;
        }
        self.clock += 1;
        let stamp = self.clock;
        let entry = self.entries.get_mut(id_token)?;
        entry.0 = stamp;
        Some(entry.1.clone())
    }

    /// Empties the cache, as `ClearCache` does.
    pub fn clear(&mut self) {
        self.entries.clear();
    }
}

/// Where a local authorization decision came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LocalSource {
    /// The operator-managed local list, which C13.FR.01 gives priority over the cache.
    LocalList,
    /// The cache of previous CSMS answers.
    Cache,
}

/// What to do about one `IdToken`.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Decision {
    /// Something local answered. Act on `info.status` without contacting the CSMS.
    Local {
        /// Which of the two answered.
        source: LocalSource,
        /// The verdict, with `status` already evaluated against the current time.
        info: IdTokenInfo,
    },
    /// Send an `Authorize` (2.x) or `Authorize.req` (1.6) and wait for the answer.
    AskCsms,
    /// The station is offline and neither the list nor the cache knows this token.
    ///
    /// C15 / C13.FR.04: `AuthCtrlr.OfflineTxForUnknownIdEnabled` decides what happens, and
    /// the answer is carried here so the caller does not have to look it up again.
    OfflineUnknown {
        /// Whether to start the transaction anyway and reconcile when the CSMS returns.
        start_anyway: bool,
    },
}

/// The `AuthCtrlr` and `LocalAuthListCtrlr` variables that decide where an answer may come
/// from.
///
/// All four are named by the specification, and three of them are `Required`. Hard-coding
/// any of them means a station that cannot be configured the way its operator needs — and,
/// worse, one that authorizes locally when it was told not to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
// Four booleans because the specification defines four independent switches; grouping them
// into an enum would invent combinations it does not have.
#[allow(clippy::struct_excessive_bools)]
pub struct AuthorizationPolicy {
    /// `LocalAuthListCtrlr.LocalAuthListEnabled` — whether the local list is consulted.
    pub local_auth_list_enabled: bool,
    /// `AuthCtrlr.LocalAuthorizeOffline` — "whether the Charging Station, **when Offline**,
    /// will start a transaction for locally-authorized identifiers".
    pub local_authorize_offline: bool,
    /// `AuthCtrlr.LocalPreAuthorize` — "whether the Charging Station, **when online**, will
    /// start a transaction for locally-authorized identifiers without waiting for or
    /// requesting an `AuthorizeResponse` from the CSMS".
    pub local_pre_authorize: bool,
    /// `AuthCtrlr.OfflineTxForUnknownIdEnabled` — whether an identifier neither the list nor
    /// the cache knows may still start a transaction while offline (C13.FR.04, C15).
    pub offline_tx_for_unknown_id: bool,
}

impl Default for AuthorizationPolicy {
    /// The conservative configuration: authorize locally while offline, because that is what
    /// the local list is *for*, but never skip the CSMS while it is reachable and never let
    /// an unknown token charge.
    fn default() -> Self {
        Self {
            local_auth_list_enabled: true,
            local_authorize_offline: true,
            local_pre_authorize: false,
            offline_tx_for_unknown_id: false,
        }
    }
}

impl AuthorizationPolicy {
    /// `LocalAuthListCtrlr.LocalAuthListEnabled`.
    #[must_use]
    pub const fn local_auth_list_enabled(mut self, enabled: bool) -> Self {
        self.local_auth_list_enabled = enabled;
        self
    }

    /// `AuthCtrlr.LocalAuthorizeOffline`.
    #[must_use]
    pub const fn local_authorize_offline(mut self, enabled: bool) -> Self {
        self.local_authorize_offline = enabled;
        self
    }

    /// `AuthCtrlr.LocalPreAuthorize`.
    #[must_use]
    pub const fn local_pre_authorize(mut self, enabled: bool) -> Self {
        self.local_pre_authorize = enabled;
        self
    }

    /// `AuthCtrlr.OfflineTxForUnknownIdEnabled`.
    #[must_use]
    pub const fn offline_tx_for_unknown_id(mut self, enabled: bool) -> Self {
        self.offline_tx_for_unknown_id = enabled;
        self
    }
}

/// Ties the list and the cache together with the lookup order the specification prescribes.
///
/// The order is C13.FR.01's: the local list first — it is operator-managed and authoritative,
/// and has "priority over Authorization Cache entries for the same identifiers" — then the
/// cache, then the CSMS.
///
/// What may be *used* depends on whether the station is online, and that is the part most
/// implementations flatten:
///
/// | | offline | online |
/// |---|---|---|
/// | Local list / cache says `Accepted` | used if `LocalAuthorizeOffline` | used if `LocalPreAuthorize`, otherwise the CSMS is asked |
/// | Local list / cache says anything else | refuse | **ask the CSMS anyway** (C10: "if the IdToken is not known, **or the IdToken is not Accepted**, the Charging Station sends an AuthorizeRequest") |
/// | Nothing local knows it | `OfflineTxForUnknownIdEnabled` decides | ask the CSMS |
#[derive(Clone, Debug, Default)]
pub struct Authorizer {
    /// The operator's list.
    pub list: LocalAuthorizationList,
    /// The cache of CSMS answers.
    pub cache: AuthorizationCache,
    /// Which of the local sources may be used, and when.
    pub policy: AuthorizationPolicy,
}

impl Authorizer {
    /// An authorizer with the default policy.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Replaces the policy.
    #[must_use]
    pub fn with_policy(mut self, policy: AuthorizationPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Decides what to do with a token.
    ///
    /// `online` says whether the CSMS is reachable right now.
    pub fn decide(&mut self, id_token: &str, online: bool, now: DateTime) -> Decision {
        // Whether a local answer may be acted on at all, per the two variables whose entire
        // purpose is to answer that question.
        let may_use_local = if online {
            self.policy.local_pre_authorize
        } else {
            self.policy.local_authorize_offline
        };

        let local = self
            .policy
            .local_auth_list_enabled
            .then(|| self.list.get(id_token).cloned())
            .flatten()
            .map(|info| (LocalSource::LocalList, info))
            .or_else(|| {
                self.cache
                    .get(id_token)
                    .map(|info| (LocalSource::Cache, info))
            });

        if let Some((source, info)) = local {
            let status = info.status_at(now);
            if status == AuthorizationStatus::Accepted {
                if may_use_local {
                    return Decision::Local {
                        source,
                        info: IdTokenInfo { status, ..info },
                    };
                }
            } else if !online {
                // Offline there is nobody to appeal to, so a local refusal stands.
                return Decision::Local {
                    source,
                    info: IdTokenInfo { status, ..info },
                };
            }
            // Online and not `Accepted`: C10 sends an AuthorizeRequest rather than refusing
            // on the strength of a local entry the CSMS may since have changed.
            if online {
                return Decision::AskCsms;
            }
        }

        if online {
            Decision::AskCsms
        } else {
            Decision::OfflineUnknown {
                start_anyway: self.policy.offline_tx_for_unknown_id,
            }
        }
    }

    /// Records what the CSMS answered, so the next offline period can use it.
    pub fn remember(&mut self, id_token: &str, info: IdTokenInfo) {
        self.cache.remember(id_token.to_string(), info);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(text: &str) -> DateTime {
        DateTime::parse(text).unwrap()
    }

    fn now() -> DateTime {
        at("2024-06-01T12:00:00Z")
    }

    #[test]
    fn a_differential_update_must_advance_the_version() {
        let mut list = LocalAuthorizationList::new();
        assert_eq!(
            list.update(
                UpdateType::Full,
                5,
                alloc::vec![("A".into(), Some(IdTokenInfo::accepted()))]
            ),
            UpdateStatus::Accepted
        );
        assert_eq!(list.version(), 5);
        assert_eq!(
            list.update(UpdateType::Differential, 5, alloc::vec![]),
            UpdateStatus::VersionMismatch
        );
        assert_eq!(
            list.update(
                UpdateType::Differential,
                6,
                alloc::vec![
                    ("B".into(), Some(IdTokenInfo::accepted())),
                    ("A".into(), None)
                ]
            ),
            UpdateStatus::Accepted
        );
        assert!(list.get("A").is_none());
        assert!(list.get("B").is_some());
    }

    #[test]
    fn a_full_update_replaces_the_list() {
        let mut list = LocalAuthorizationList::new();
        list.update(
            UpdateType::Full,
            1,
            alloc::vec![("A".into(), Some(IdTokenInfo::accepted()))],
        );
        list.update(
            UpdateType::Full,
            2,
            alloc::vec![("B".into(), Some(IdTokenInfo::accepted()))],
        );
        assert!(list.get("A").is_none());
        assert_eq!(list.len(), 1);
    }

    #[test]
    fn a_bounded_list_refuses_to_overflow() {
        let mut list = LocalAuthorizationList::bounded(1);
        assert_eq!(
            list.update(
                UpdateType::Full,
                1,
                alloc::vec![
                    ("A".into(), Some(IdTokenInfo::accepted())),
                    ("B".into(), Some(IdTokenInfo::accepted()))
                ]
            ),
            UpdateStatus::Failed
        );
        assert!(list.is_empty());
    }

    fn seeded() -> Authorizer {
        let mut authorizer = Authorizer::new();
        authorizer.list.update(
            UpdateType::Full,
            1,
            alloc::vec![
                (
                    "BLOCKED".into(),
                    Some(IdTokenInfo::new(AuthorizationStatus::Blocked))
                ),
                (
                    "OLD".into(),
                    Some(IdTokenInfo::accepted().expiring(at("2024-01-01T00:00:00Z")))
                ),
                ("LISTED".into(), Some(IdTokenInfo::accepted())),
            ],
        );
        authorizer.remember("CACHED", IdTokenInfo::accepted());
        authorizer
    }

    /// C13.FR.01: local list entries have "priority over Authorization Cache entries for the
    /// same identifiers", and an expired entry is not an accepted one.
    #[test]
    fn the_list_outranks_the_cache_and_expiry_is_honoured() {
        let mut authorizer = seeded();
        authorizer.remember("LISTED", IdTokenInfo::new(AuthorizationStatus::Blocked));

        let decision = authorizer.decide("LISTED", false, now());
        assert!(
            matches!(
                &decision,
                Decision::Local {
                    source: LocalSource::LocalList,
                    info
                } if info.status == AuthorizationStatus::Accepted
            ),
            "{decision:?}"
        );

        let decision = authorizer.decide("OLD", false, now());
        assert!(
            matches!(&decision, Decision::Local { info, .. }
                if info.status == AuthorizationStatus::Expired),
            "{decision:?}"
        );
    }

    /// C10, step 3: "If the `IdToken` is not known, **or the `IdToken` is not Accepted**, the
    /// Charging Station sends an `AuthorizeRequest`." A local entry that says `Blocked` is a
    /// reason to ask, not a reason to refuse — the CSMS may have changed its mind since the
    /// list was last synchronised, and it is the authority.
    #[test]
    fn an_online_station_asks_the_csms_about_a_locally_refused_token_c10() {
        let mut authorizer = seeded();
        assert_eq!(authorizer.decide("BLOCKED", true, now()), Decision::AskCsms);
        assert_eq!(
            authorizer.decide("NEVER-SEEN", true, now()),
            Decision::AskCsms
        );
    }

    /// Offline there is nobody to appeal to, so the local answer — including a refusal —
    /// stands.
    #[test]
    fn an_offline_station_honours_a_local_refusal() {
        let mut authorizer = seeded();
        let decision = authorizer.decide("BLOCKED", false, now());
        assert!(
            matches!(&decision, Decision::Local { info, .. }
                if info.status == AuthorizationStatus::Blocked),
            "{decision:?}"
        );
    }

    /// `AuthCtrlr.LocalAuthorizeOffline` is a `Required` variable whose entire job is to say
    /// whether an offline station may answer from the list at all. Hard-coding it to `true`
    /// means a station that authorizes when its operator configured it not to.
    #[test]
    fn local_authorize_offline_gates_the_local_answer() {
        let mut authorizer =
            seeded().with_policy(AuthorizationPolicy::default().local_authorize_offline(false));
        assert_eq!(
            authorizer.decide("LISTED", false, now()),
            Decision::OfflineUnknown {
                start_anyway: false
            }
        );
    }

    /// C06's prerequisites are `AuthCacheEnabled = true` **and** `LocalPreAuthorize = true`,
    /// in an online scenario — so the cache is not an offline-only structure.
    #[test]
    fn local_pre_authorize_lets_the_cache_answer_while_online_c06() {
        let mut authorizer =
            seeded().with_policy(AuthorizationPolicy::default().local_pre_authorize(true));
        let decision = authorizer.decide("CACHED", true, now());
        assert!(
            matches!(
                &decision,
                Decision::Local {
                    source: LocalSource::Cache,
                    info
                } if info.status == AuthorizationStatus::Accepted
            ),
            "{decision:?}"
        );

        // Without it, the CSMS is asked even though the cache knows the token.
        let mut strict = seeded();
        assert_eq!(strict.decide("CACHED", true, now()), Decision::AskCsms);
    }

    /// C13.FR.04 / C15: offline, with `OfflineTxForUnknownIdEnabled`, a token nothing knows
    /// may still charge.
    #[test]
    fn offline_tx_for_unknown_id_is_reported_with_the_decision_c15() {
        let mut authorizer = seeded();
        assert_eq!(
            authorizer.decide("NEVER-SEEN", false, now()),
            Decision::OfflineUnknown {
                start_anyway: false
            }
        );

        let mut permissive =
            seeded().with_policy(AuthorizationPolicy::default().offline_tx_for_unknown_id(true));
        assert_eq!(
            permissive.decide("NEVER-SEEN", false, now()),
            Decision::OfflineUnknown { start_anyway: true }
        );
    }

    /// `LocalAuthListCtrlr.LocalAuthListEnabled` switches the list off without emptying it.
    #[test]
    fn a_disabled_local_list_is_not_consulted() {
        let mut authorizer =
            seeded().with_policy(AuthorizationPolicy::default().local_auth_list_enabled(false));
        assert_eq!(
            authorizer.decide("LISTED", false, now()),
            Decision::OfflineUnknown {
                start_anyway: false
            }
        );
        assert_eq!(authorizer.list.len(), 3, "the list is still there");
    }

    #[test]
    fn the_cache_evicts_the_least_recently_used_token() {
        let mut cache = AuthorizationCache::with_capacity(2);
        cache.remember("A", IdTokenInfo::accepted());
        cache.remember("B", IdTokenInfo::accepted());
        assert!(cache.get("A").is_some(), "touching A makes B the oldest");
        cache.remember("C", IdTokenInfo::accepted());
        assert!(cache.get("B").is_none());
        assert!(cache.get("A").is_some());
        assert!(cache.get("C").is_some());
    }

    #[test]
    fn disabling_the_cache_empties_it() {
        let mut cache = AuthorizationCache::default();
        cache.remember("A", IdTokenInfo::accepted());
        cache.set_enabled(false);
        assert!(cache.get("A").is_none());
        cache.remember("B", IdTokenInfo::accepted());
        assert!(cache.is_empty());
    }
}
