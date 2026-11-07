//! Local authorization: the list, the cache, and the rules that decide between them.
//!
//! Blocks C (Authorization) and D (Local Authorization List Management). A Charging Station
//! that is offline still has to answer the driver, and the specification is precise about
//! where the answer comes from: the local authorization list first (it is authoritative and
//! operator-managed), then the authorization cache (it is a memory of what the CSMS said
//! before), and only then the CSMS itself.

use alloc::collections::BTreeMap;
use alloc::string::String;
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

/// An `IdToken` as the specification compares it: by value **and** type.
///
/// C01.FR.25: "Two `IdTokenType` elements are considered to be equal when they have the same
/// value for the fields `idToken.idToken` and `idToken.type`"; `additionalInfo` is not
/// compared.
/// The same RFID UID presented as `ISO14443` and as `Local` is two tokens. 1.6 has only the
/// `idTag`, so its tokens have no type ([`IdToken::tag`]).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct IdToken {
    /// `idToken.idToken` (2.x) or the `idTag` (1.6).
    pub value: String,
    /// `idToken.type` as sent (`ISO14443`, `Central`, `eMAID`, …); `None` on 1.6.
    pub kind: Option<String>,
}

impl IdToken {
    /// A 2.x token of the given type.
    #[must_use]
    pub fn new(value: impl Into<String>, kind: impl Into<String>) -> Self {
        Self {
            value: value.into(),
            kind: Some(kind.into()),
        }
    }

    /// A 1.6 `idTag`, which has no type.
    #[must_use]
    pub fn tag(value: impl Into<String>) -> Self {
        Self {
            value: value.into(),
            kind: None,
        }
    }

    /// Whether this is a `Central` token, which is never cached (C05.FR.02).
    #[must_use]
    pub fn is_central(&self) -> bool {
        self.kind.as_deref() == Some("Central")
    }
}

impl fmt::Display for IdToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.kind {
            Some(kind) => write!(f, "{} ({kind})", self.value),
            None => f.write_str(&self.value),
        }
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
#[must_use = "a refused update left the list unchanged — `VersionMismatch` in particular means \
              the CSMS and the station now disagree about what is on it"]
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
    entries: BTreeMap<IdToken, IdTokenInfo>,
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
        entries: Vec<(IdToken, Option<IdTokenInfo>)>,
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

    /// Looks a token up, by value and type (C01.FR.25).
    #[must_use]
    pub fn get(&self, id_token: &IdToken) -> Option<&IdTokenInfo> {
        self.entries.get(id_token)
    }
}

/// The authorization cache: what the CSMS said, remembered for when it cannot be asked.
///
/// Bounded and least-recently-used, because a Charging Station has finite memory and an
/// unbounded cache is a slow leak. `AuthCacheCtrlr.Enabled` decides whether it is consulted
/// at all.
///
/// Two rules decide what may stay:
///
/// * A `Central` token is never stored, "in any message" (C05.FR.02).
/// * An entry without `cacheExpiryDateTime` expires `AuthCacheLifeTime` seconds after it was
///   last updated (C10.FR.08/10). One with it follows that date (C10.FR.13), reported as
///   `Expired` by [`IdTokenInfo::status_at`].
#[derive(Clone, Debug)]
pub struct AuthorizationCache {
    entries: BTreeMap<IdToken, CacheEntry>,
    capacity: usize,
    clock: u64,
    enabled: bool,
    lifetime: Option<i64>,
}

#[derive(Clone, Debug)]
struct CacheEntry {
    used: u64,
    updated: DateTime,
    info: IdTokenInfo,
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
            lifetime: None,
        }
    }

    /// Sets `AuthCacheCtrlr.LifeTime` (`AuthCacheLifeTime`), in seconds: how long an entry
    /// without its own `cacheExpiryDateTime` stays after it was last updated.
    #[must_use]
    pub fn with_lifetime(mut self, seconds: u32) -> Self {
        self.lifetime = Some(i64::from(seconds));
        self
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

    /// Records what the CSMS answered at `now`. A `Central` token is not stored (C05.FR.02),
    /// and an answer whose `cacheExpiryDateTime` has already passed removes the entry rather
    /// than storing it (C10.FR.13).
    pub fn remember(&mut self, id_token: IdToken, info: IdTokenInfo, now: DateTime) {
        if !self.enabled || id_token.is_central() {
            return;
        }
        if info.expires.is_some_and(|expiry| expiry <= now) {
            self.entries.remove(&id_token);
            return;
        }
        self.clock += 1;
        let used = self.clock;
        self.entries.insert(
            id_token,
            CacheEntry {
                used,
                updated: now,
                info,
            },
        );
        while self.entries.len() > self.capacity {
            if let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.used)
                .map(|(token, _)| token.clone())
            {
                self.entries.remove(&oldest);
            } else {
                break;
            }
        }
    }

    /// Looks a token up at `now`, refreshing its recency. An entry past `AuthCacheLifeTime`
    /// is removed and not returned (C10.FR.10).
    pub fn get(&mut self, id_token: &IdToken, now: DateTime) -> Option<IdTokenInfo> {
        if !self.enabled {
            return None;
        }
        let entry = self.entries.get(id_token)?;
        if entry.info.expires.is_none()
            && self.lifetime.is_some_and(|lifetime| {
                now.timestamp().as_second() - entry.updated.timestamp().as_second() > lifetime
            })
        {
            self.entries.remove(id_token);
            return None;
        }
        self.clock += 1;
        let used = self.clock;
        let entry = self.entries.get_mut(id_token)?;
        entry.used = used;
        Some(entry.info.clone())
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
#[must_use = "this is the authorization decision: discarding it authorizes everyone"]
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
/// All five are named by the specification, and three of them are `Required`. Hard-coding
/// any of them means a station that cannot be configured the way its operator needs — and,
/// worse, one that authorizes locally when it was told not to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
// Five booleans because the specification defines five independent switches; grouping them
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
    /// `LocalAuthListCtrlr.SupportsExpiryDateTime` — whether a local-list entry's
    /// `cacheExpiryDateTime` is honoured.
    ///
    /// `true` (the default): an expired entry is disregarded — online the CSMS is asked
    /// (C14.FR.05), offline it is refused, since it is not an unknown id (C13.FR.05/06). A
    /// station using this default reports the variable as `true`. `false`: the list's expiry
    /// is not checked (C13.FR.02/04, C14.FR.02: "does not exist or is false"). The cache's
    /// own expiry is unaffected either way.
    pub local_list_supports_expiry: bool,
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
            local_list_supports_expiry: true,
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

    /// `LocalAuthListCtrlr.SupportsExpiryDateTime`.
    #[must_use]
    pub const fn local_list_supports_expiry(mut self, enabled: bool) -> Self {
        self.local_list_supports_expiry = enabled;
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
    pub fn decide(&mut self, id_token: &IdToken, online: bool, now: DateTime) -> Decision {
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
                    .get(id_token, now)
                    .map(|info| (LocalSource::Cache, info))
            });

        if let Some((source, info)) = local {
            // Edition 4 (C13/C14): the list's `cacheExpiryDateTime` counts only where the
            // station supports it; the cache's always does.
            let status =
                if source == LocalSource::LocalList && !self.policy.local_list_supports_expiry {
                    info.status
                } else {
                    info.status_at(now)
                };
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

    /// Records what the CSMS answered at `now`, so the next offline period can use it.
    pub fn remember(&mut self, id_token: IdToken, info: IdTokenInfo, now: DateTime) {
        self.cache.remember(id_token, info, now);
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
                alloc::vec![(IdToken::tag("A"), Some(IdTokenInfo::accepted()))]
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
                    (IdToken::tag("B"), Some(IdTokenInfo::accepted())),
                    (IdToken::tag("A"), None)
                ]
            ),
            UpdateStatus::Accepted
        );
        assert!(list.get(&IdToken::tag("A")).is_none());
        assert!(list.get(&IdToken::tag("B")).is_some());
    }

    #[test]
    fn a_full_update_replaces_the_list() {
        let mut list = LocalAuthorizationList::new();
        let _ = list.update(
            UpdateType::Full,
            1,
            alloc::vec![(IdToken::tag("A"), Some(IdTokenInfo::accepted()))],
        );
        let _ = list.update(
            UpdateType::Full,
            2,
            alloc::vec![(IdToken::tag("B"), Some(IdTokenInfo::accepted()))],
        );
        assert!(list.get(&IdToken::tag("A")).is_none());
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
                    (IdToken::tag("A"), Some(IdTokenInfo::accepted())),
                    (IdToken::tag("B"), Some(IdTokenInfo::accepted()))
                ]
            ),
            UpdateStatus::Failed
        );
        assert!(list.is_empty(), "{list:?}");
    }

    fn seeded() -> Authorizer {
        let mut authorizer = Authorizer::new();
        let _ = authorizer.list.update(
            UpdateType::Full,
            1,
            alloc::vec![
                (
                    IdToken::tag("BLOCKED"),
                    Some(IdTokenInfo::new(AuthorizationStatus::Blocked))
                ),
                (
                    IdToken::tag("OLD"),
                    Some(IdTokenInfo::accepted().expiring(at("2024-01-01T00:00:00Z")))
                ),
                (IdToken::tag("LISTED"), Some(IdTokenInfo::accepted())),
            ],
        );
        authorizer.remember(IdToken::tag("CACHED"), IdTokenInfo::accepted(), now());
        authorizer
    }

    /// C13.FR.01: local list entries have "priority over Authorization Cache entries for the
    /// same identifiers", and an expired entry is not an accepted one.
    #[test]
    fn the_list_outranks_the_cache_and_expiry_is_honoured() {
        let mut authorizer = seeded();
        authorizer.remember(
            IdToken::tag("LISTED"),
            IdTokenInfo::new(AuthorizationStatus::Blocked),
            now(),
        );

        let decision = authorizer.decide(&IdToken::tag("LISTED"), false, now());
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

        let decision = authorizer.decide(&IdToken::tag("OLD"), false, now());
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
        assert_eq!(
            authorizer.decide(&IdToken::tag("BLOCKED"), true, now()),
            Decision::AskCsms
        );
        assert_eq!(
            authorizer.decide(&IdToken::tag("NEVER-SEEN"), true, now()),
            Decision::AskCsms
        );
    }

    /// Offline there is nobody to appeal to, so the local answer — including a refusal —
    /// stands.
    #[test]
    fn an_offline_station_honours_a_local_refusal() {
        let mut authorizer = seeded();
        let decision = authorizer.decide(&IdToken::tag("BLOCKED"), false, now());
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
            authorizer.decide(&IdToken::tag("LISTED"), false, now()),
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
        let decision = authorizer.decide(&IdToken::tag("CACHED"), true, now());
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
        assert_eq!(
            strict.decide(&IdToken::tag("CACHED"), true, now()),
            Decision::AskCsms
        );
    }

    /// C13.FR.04 / C15: offline, with `OfflineTxForUnknownIdEnabled`, a token nothing knows
    /// may still charge.
    #[test]
    fn offline_tx_for_unknown_id_is_reported_with_the_decision_c15() {
        let mut authorizer = seeded();
        assert_eq!(
            authorizer.decide(&IdToken::tag("NEVER-SEEN"), false, now()),
            Decision::OfflineUnknown {
                start_anyway: false
            }
        );

        let mut permissive =
            seeded().with_policy(AuthorizationPolicy::default().offline_tx_for_unknown_id(true));
        assert_eq!(
            permissive.decide(&IdToken::tag("NEVER-SEEN"), false, now()),
            Decision::OfflineUnknown { start_anyway: true }
        );
    }

    /// 2.0.1 edition 4 C13/C14: whether a listed token's expiry counts is
    /// `LocalAuthListCtrlr.SupportsExpiryDateTime`'s call.
    #[test]
    fn a_listed_tokens_expiry_counts_only_where_the_station_supports_it() {
        // "OLD" is listed `Accepted`, expiring 2024-01-01; `now()` is later.
        let mut supports = seeded();
        // Offline: expired, and not an unknown id, so refused (C13.FR.05/06).
        assert!(matches!(
            supports.decide(&IdToken::tag("OLD"), false, now()),
            Decision::Local { info, .. } if info.status == AuthorizationStatus::Expired
        ));
        // Online: disregarded, so the CSMS is asked (C14.FR.05).
        assert_eq!(
            supports.decide(&IdToken::tag("OLD"), true, now()),
            Decision::AskCsms
        );

        // Not supported: the date does not count (C13.FR.02/04).
        let mut ignores =
            seeded().with_policy(AuthorizationPolicy::default().local_list_supports_expiry(false));
        assert!(matches!(
            ignores.decide(&IdToken::tag("OLD"), false, now()),
            Decision::Local { info, .. } if info.status == AuthorizationStatus::Accepted
        ));
    }

    /// `LocalAuthListCtrlr.LocalAuthListEnabled` switches the list off without emptying it.
    #[test]
    fn a_disabled_local_list_is_not_consulted() {
        let mut authorizer =
            seeded().with_policy(AuthorizationPolicy::default().local_auth_list_enabled(false));
        assert_eq!(
            authorizer.decide(&IdToken::tag("LISTED"), false, now()),
            Decision::OfflineUnknown {
                start_anyway: false
            }
        );
        assert_eq!(authorizer.list.len(), 3, "the list is still there");
    }

    /// C01.FR.25: one value under two types is two tokens.
    #[test]
    fn tokens_compare_by_value_and_type() {
        let mut list = LocalAuthorizationList::new();
        let _ = list.update(
            UpdateType::Full,
            1,
            alloc::vec![(
                IdToken::new("04A2", "ISO14443"),
                Some(IdTokenInfo::accepted())
            )],
        );
        assert!(list.get(&IdToken::new("04A2", "ISO14443")).is_some());
        assert!(list.get(&IdToken::new("04A2", "Local")).is_none());
        assert!(list.get(&IdToken::tag("04A2")).is_none());
    }

    /// C05.FR.02: a `Central` token is never cached.
    #[test]
    fn a_central_token_is_never_cached() {
        let mut cache = AuthorizationCache::default();
        cache.remember(
            IdToken::new("APP-1", "Central"),
            IdTokenInfo::accepted(),
            now(),
        );
        assert!(cache.is_empty());
        cache.remember(
            IdToken::new("APP-1", "eMAID"),
            IdTokenInfo::accepted(),
            now(),
        );
        assert_eq!(cache.len(), 1);
    }

    /// C10.FR.08/10: without its own expiry, an entry lasts `AuthCacheLifeTime` from its last
    /// update; C10.FR.13: with one, the date decides.
    #[test]
    fn cache_entries_expire_after_the_lifetime_unless_they_carry_a_date() {
        let mut cache = AuthorizationCache::default().with_lifetime(3600);
        let token = IdToken::tag("A");
        cache.remember(
            token.clone(),
            IdTokenInfo::accepted(),
            at("2024-06-01T12:00:00Z"),
        );
        assert!(cache.get(&token, at("2024-06-01T12:59:59Z")).is_some());
        assert!(cache.get(&token, at("2024-06-01T13:00:01Z")).is_none());
        assert!(cache.is_empty(), "removed, not merely hidden");

        let dated = IdToken::tag("B");
        let until = at("2024-06-02T00:00:00Z");
        cache.remember(
            dated.clone(),
            IdTokenInfo::accepted().expiring(until),
            at("2024-06-01T12:00:00Z"),
        );
        let later = cache.get(&dated, at("2024-06-01T18:00:00Z")).unwrap();
        assert_eq!(
            later.status_at(at("2024-06-01T18:00:00Z")),
            AuthorizationStatus::Accepted
        );

        // An answer that is already expired is not stored at all.
        cache.remember(
            IdToken::tag("C"),
            IdTokenInfo::accepted().expiring(at("2024-01-01T00:00:00Z")),
            now(),
        );
        assert!(cache.get(&IdToken::tag("C"), now()).is_none());
    }

    #[test]
    fn the_cache_evicts_the_least_recently_used_token() {
        let mut cache = AuthorizationCache::with_capacity(2);
        cache.remember(IdToken::tag("A"), IdTokenInfo::accepted(), now());
        cache.remember(IdToken::tag("B"), IdTokenInfo::accepted(), now());
        assert!(
            cache.get(&IdToken::tag("A"), now()).is_some(),
            "touching A makes B the oldest"
        );
        cache.remember(IdToken::tag("C"), IdTokenInfo::accepted(), now());
        assert!(cache.get(&IdToken::tag("B"), now()).is_none());
        assert!(cache.get(&IdToken::tag("A"), now()).is_some());
        assert!(cache.get(&IdToken::tag("C"), now()).is_some());
    }

    #[test]
    fn disabling_the_cache_empties_it() {
        let mut cache = AuthorizationCache::default();
        cache.remember(IdToken::tag("A"), IdTokenInfo::accepted(), now());
        cache.set_enabled(false);
        assert!(cache.get(&IdToken::tag("A"), now()).is_none());
        cache.remember(IdToken::tag("B"), IdTokenInfo::accepted(), now());
        assert!(cache.is_empty(), "{cache:?}");
    }
}
