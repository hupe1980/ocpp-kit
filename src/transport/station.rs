//! The Charging Station side of the transport.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use http::Uri;
use tokio::net::TcpStream;
use tokio::sync::{broadcast, mpsc, watch};

use crate::decode::DecodeOptions;
use crate::engine::{
    Backoff, BootState, CallFailure, Engine, EngineConfig, MemStore, MessageStore, Role,
};
use crate::types::Identity;
use crate::version::{Subprotocol, Version};

use super::TransportError;
use super::connection::{Driver, Ended, Event, Handle, Handler, Keepalive, SessionState, Shared};
use super::network_profile::{NetworkProfile, NetworkProfiles, ProfileCycler};
use super::security::{BasicAuthPassword, SecurityProfile, basic_auth_header};
use super::stream::MaybeTls;
use super::ws::handshake::{ClientRequest, client_handshake, percent_encode};
use super::ws::{Config as WsConfig, Role as WsRole, WebSocket, WsCodec};

/// Builds a [`Station`].
pub struct StationBuilder<S: MessageStore = MemStore> {
    identity: Option<Identity>,
    url: Option<String>,
    versions: Subprotocol,
    profile: SecurityProfile,
    password: Option<BasicAuthPassword>,
    #[cfg(feature = "rustls")]
    tls: Option<super::tls::ClientTls>,
    keepalive: Keepalive,
    backoff: Backoff,
    reconnect: bool,
    decode: DecodeOptions,
    engine: Option<EngineConfig>,
    handler: Option<Arc<dyn Handler>>,
    store: S,
    ws_config: WsConfig,
    #[cfg(feature = "compression")]
    compression: bool,
    profiles: Option<NetworkProfiles>,
}

impl Default for StationBuilder<MemStore> {
    fn default() -> Self {
        Self {
            identity: None,
            url: None,
            versions: Subprotocol::default(),
            profile: SecurityProfile::BasicAuth,
            password: None,
            #[cfg(feature = "rustls")]
            tls: None,
            keepalive: Keepalive::default(),
            backoff: Backoff::default(),
            reconnect: true,
            decode: DecodeOptions::strict(),
            engine: None,
            handler: None,
            store: MemStore::new(),
            ws_config: WsConfig::default(),
            // Part 4 §3.4 Table 2: optional for a Charging Station, but recommended — it is
            // the cheapest way to cut a fleet's mobile data bill.
            #[cfg(feature = "compression")]
            compression: true,
            profiles: None,
        }
    }
}

impl<S: MessageStore + Send + 'static> StationBuilder<S> {
    /// The Charging Station identity. It becomes the last path segment of the WebSocket URL
    /// and, under profiles 1 and 2, the HTTP Basic user name.
    pub fn identity(mut self, identity: &str) -> Result<Self, TransportError> {
        self.identity = Some(
            Identity::new(identity)
                .map_err(|error| TransportError::Configuration(error.to_string()))?,
        );
        Ok(self)
    }

    /// The CSMS endpoint *without* the identity, e.g. `wss://csms.example.com/ocpp`.
    ///
    /// The identity is appended, percent-encoded, as Part 4 §3.1 prescribes.
    #[must_use]
    pub fn url(mut self, url: impl Into<String>) -> Self {
        self.url = Some(url.into());
        self
    }

    /// The versions to offer, most preferred first.
    ///
    /// Part 4 §3.2 recommends that a 2.1 station also offer 2.0.1, which the default does.
    #[must_use]
    pub fn versions(mut self, versions: impl IntoIterator<Item = Version>) -> Self {
        self.versions = Subprotocol::new(versions);
        self
    }

    /// The security profile to use.
    #[must_use]
    pub fn security_profile(mut self, profile: SecurityProfile) -> Self {
        self.profile = profile;
        self
    }

    /// The HTTP Basic password, for profiles 1 and 2.
    #[must_use]
    pub fn password(mut self, password: BasicAuthPassword) -> Self {
        self.password = Some(password);
        self
    }

    /// The TLS configuration, for profiles 2 and 3.
    #[cfg(feature = "rustls")]
    #[must_use]
    pub fn tls(mut self, tls: super::tls::ClientTls) -> Self {
        self.tls = Some(tls);
        self
    }

    /// How often to send a WebSocket ping — the `WebSocketPingInterval` variable.
    ///
    /// Part 4 §5.3: ping/pong keeps the connection alive and can stand in for most
    /// `Heartbeat`s, but not for the clock synchronisation the `Heartbeat` response carries,
    /// so the engine keeps sending real heartbeats on their own interval.
    ///
    /// A ping that is never answered ends the session after
    /// [`Keepalive::timeout`](super::Keepalive::timeout), which is what turns a mobile
    /// network's silently dropped connection into a reconnect instead of a hang. Use
    /// [`keepalive`](Self::keepalive) to change that timeout.
    #[must_use]
    pub fn ping_interval(mut self, interval: Option<Duration>) -> Self {
        self.keepalive.interval = interval;
        self
    }

    /// The full WebSocket keepalive policy: ping interval and pong deadline.
    #[must_use]
    pub fn keepalive(mut self, keepalive: Keepalive) -> Self {
        self.keepalive = keepalive;
        self
    }

    /// The reconnect schedule (Part 4 §5.4).
    #[must_use]
    pub fn backoff(mut self, backoff: Backoff) -> Self {
        self.backoff = backoff;
        self
    }

    /// Whether to reconnect at all. On by default.
    #[must_use]
    pub fn reconnect(mut self, reconnect: bool) -> Self {
        self.reconnect = reconnect;
        self
    }

    /// How forgiving to be about payloads the CSMS sends.
    ///
    /// [`DecodeOptions::max_payload_size`] is carried down to the WebSocket layer too, so an
    /// oversized message is refused as it arrives rather than after it has been buffered — and
    /// a compressed one is measured as it inflates. A station talks to one CSMS, so unlike a
    /// server it has no reason to keep the two limits apart.
    #[must_use]
    pub fn decode_options(mut self, options: DecodeOptions) -> Self {
        self.ws_config.max_message_size = options.max_payload_size;
        self.ws_config.max_frame_size = options.max_payload_size;
        self.decode = options;
        self
    }

    /// Whether to offer RFC 7692 `permessage-deflate`.
    ///
    /// On by default with the `compression` feature. Part 4 §3.4 makes it optional for a
    /// Charging Station and recommends it; §3.4 also says a station must **not** close the
    /// connection when the CSMS declines, and this implementation does not.
    #[cfg(feature = "compression")]
    #[must_use]
    pub fn compression(mut self, enabled: bool) -> Self {
        self.compression = enabled;
        self
    }

    /// Overrides the engine configuration. The role and version are set by the builder.
    #[must_use]
    pub fn engine_config(mut self, config: EngineConfig) -> Self {
        self.engine = Some(config);
        self
    }

    /// The handler for requests the CSMS sends.
    #[must_use]
    pub fn handler(mut self, handler: impl Handler) -> Self {
        self.handler = Some(Arc::new(handler));
        self
    }

    /// The station's network configuration slots, in priority order.
    ///
    /// This is the 2.x model: numbered slots, a `NetworkConfigurationPriority` order and a
    /// `NetworkProfileConnectionAttempts` budget per slot. Use it instead of
    /// [`url`](Self::url) when the station has a fallback CSMS — which is what makes
    /// migrating to a new CSMS (use case B10) a configuration change rather than a
    /// reflash.
    #[must_use]
    pub fn network_profiles(mut self, profiles: NetworkProfiles) -> Self {
        self.profiles = Some(profiles);
        self
    }

    /// A durable message store, so queued `TransactionEvent`s survive a power cut.
    pub fn store<T: MessageStore + Send + 'static>(self, store: T) -> StationBuilder<T> {
        StationBuilder {
            identity: self.identity,
            url: self.url,
            versions: self.versions,
            profile: self.profile,
            password: self.password,
            #[cfg(feature = "rustls")]
            tls: self.tls,
            keepalive: self.keepalive,
            backoff: self.backoff,
            reconnect: self.reconnect,
            decode: self.decode,
            engine: self.engine,
            handler: self.handler,
            store,
            ws_config: self.ws_config,
            #[cfg(feature = "compression")]
            compression: self.compression,
            profiles: self.profiles,
        }
    }

    /// Builds the station.
    pub fn build(self) -> Result<Station<S>, TransportError> {
        let identity = self
            .identity
            .ok_or_else(|| TransportError::Configuration("identity is required".into()))?;

        // A station configured with a single URL is just a one-slot network configuration, so
        // there is one code path for both.
        let profiles = if let Some(profiles) = self.profiles {
            profiles
        } else {
            {
                let url = self.url.ok_or_else(|| {
                    TransportError::Configuration(
                        "either `url` or `network_profiles` is required".into(),
                    )
                })?;
                let mut profile = NetworkProfile::new(0, url).security_profile(self.profile);
                if let Some(password) = self.password {
                    profile = profile.password(password);
                }
                #[cfg(feature = "rustls")]
                if let Some(tls) = self.tls {
                    profile = profile.tls(tls);
                }
                NetworkProfiles::new([profile])
            }
        };

        let preferred = *self.versions.offered().first().ok_or_else(|| {
            TransportError::Configuration("at least one version must be offered".into())
        })?;

        // Every slot is validated now, not when the station first fails over to it — a
        // fallback profile that turns out to be unusable is worse than no fallback at all.
        let mut endpoints = Vec::with_capacity(profiles.configured_slots().len());
        for slot in profiles.configured_slots() {
            let profile = profiles.get(slot).expect("configured");
            endpoints.push(Endpoint::for_profile(profile, &identity, &self.versions)?);
        }

        let mut engine_config = self
            .engine
            .unwrap_or_else(|| EngineConfig::new(Role::ChargingStation, preferred));
        engine_config.role = Role::ChargingStation;
        engine_config.version = preferred;
        engine_config.backoff = self.backoff;

        Ok(Station {
            connector: Connector {
                identity,
                versions: self.versions,
                profiles,
                endpoints: endpoints.into_iter().map(|e| (e.slot, e)).collect(),
                ws_config: self.ws_config,
                #[cfg(feature = "compression")]
                compression: self.compression,
            },
            keepalive: self.keepalive,
            backoff: self.backoff,
            reconnect: self.reconnect,
            decode: self.decode,
            engine_config,
            handler: self
                .handler
                .unwrap_or_else(|| Arc::new(super::connection::NotImplemented)),
            store: self.store,
        })
    }
}

/// Everything needed to open one connection, separate from the session state so the
/// reconnect loop can own it while the engine owns the store.
struct Connector {
    identity: Identity,
    versions: Subprotocol,
    profiles: NetworkProfiles,
    /// One validated endpoint per configuration slot.
    endpoints: BTreeMap<i32, Endpoint>,
    ws_config: WsConfig,
    #[cfg(feature = "compression")]
    compression: bool,
}

/// A Charging Station client.
pub struct Station<S: MessageStore = MemStore> {
    connector: Connector,
    keepalive: Keepalive,
    backoff: Backoff,
    reconnect: bool,
    decode: DecodeOptions,
    engine_config: EngineConfig,
    handler: Arc<dyn Handler>,
    store: S,
}

impl Station<MemStore> {
    /// Starts building a station.
    #[must_use]
    pub fn builder() -> StationBuilder<MemStore> {
        StationBuilder::default()
    }
}

impl<S: MessageStore + Send + 'static> Station<S> {
    /// Runs the station on the current task until it is shut down.
    ///
    /// The engine — and therefore the offline queue and the boot state — survives every
    /// reconnect, which is what Part 4 §5.4 requires: a station does not repeat
    /// `BootNotification` on a mere reconnect.
    pub async fn run(self) -> Result<(), TransportError> {
        let (handle, task) = self.into_parts()?;
        drop(handle);
        task.await
    }

    /// Spawns the station and returns a handle for calling the CSMS.
    pub fn spawn(self) -> Result<Handle, TransportError> {
        let (handle, task) = self.into_parts()?;
        tokio::spawn(async move {
            if let Err(error) = task.await {
                #[cfg(feature = "tracing")]
                tracing::error!(%error, "charging station stopped");
                let _ = error;
            }
        });
        Ok(handle)
    }

    fn into_parts(
        self,
    ) -> Result<
        (
            Handle,
            impl std::future::Future<Output = Result<(), TransportError>>,
        ),
        TransportError,
    > {
        let (commands_tx, mut commands_rx) = mpsc::channel(64);
        let (events_tx, _) = broadcast::channel(256);
        let (state_tx, state_rx) = watch::channel(SessionState {
            connected: false,
            version: self.engine_config.version,
            boot: BootState::Idle,
            queued: 0,
        });
        let shared = Arc::new(Shared {
            identity: self.connector.identity.clone(),
            remote: None,
            decode: self.decode.clone(),
            commands: commands_tx,
            events: events_tx,
            state: state_rx,
        });
        let handle = Handle::new(shared.clone());

        let engine = Engine::with_store(self.engine_config, self.store)?;
        let mut driver = Driver::new(engine, shared.clone(), self.handler, state_tx);
        let connector = self.connector;
        let keepalive = self.keepalive;
        let backoff = self.backoff;
        // The configured default, restored whenever the active profile does not override it
        // (Part 4 §4.1.1: `NetworkConnectionProfile.messageTimeout`) — otherwise one slot's
        // timeout would leak into every slot the station later fails over to.
        let default_call_timeout = driver.engine.config().call_timeout;
        let reconnect = self.reconnect;

        let task = async move {
            let mut attempt = 0u32;
            let mut cycler = ProfileCycler::new(&connector.profiles);
            let mut announced = None;

            loop {
                let (profile, _) = connector.profile_at(cycler.index());
                let slot = profile.slot();
                // `OCPPCommCtrlr.ActiveNetworkProfile` — report the slot the station is on,
                // once per change rather than once per attempt.
                if announced != Some(slot) {
                    announced = Some(slot);
                    let _ = shared.events.send(Event::NetworkProfileSelected {
                        configuration_slot: slot,
                        url: profile.url().to_owned(),
                    });
                }
                // Part 4 §4.1.1: `NetworkConnectionProfile.messageTimeout` overrides the
                // default while this profile is active.
                let message_timeout = profile.message_timeout;

                match connector.connect(cycler.index()).await {
                    Ok((socket, version)) => {
                        attempt = 0;
                        cycler.succeeded();
                        driver
                            .engine
                            .set_call_timeout(message_timeout.unwrap_or(default_call_timeout));
                        // A write that failed, a peer that broke the WebSocket protocol, a
                        // clean close: these all end the *connection*, not the station.
                        // Part 4 §5.4 says to reconnect when the connection is lost and does
                        // not carve out the ways in which it can be lost — so only a drain
                        // the application asked for stops the loop.
                        let ended = driver
                            .run(socket, &mut commands_rx, version, keepalive)
                            .await;
                        if matches!(ended, Ended::Closed(_)) || !reconnect {
                            driver.abandon(&CallFailure::Disconnected);
                            return Ok(());
                        }
                        #[cfg(feature = "tracing")]
                        if let Ended::Disconnected(reason) = &ended {
                            tracing::info!(%reason, slot, "the session ended; reconnecting");
                        }
                    }
                    Err(error) => {
                        if !reconnect {
                            return Err(error);
                        }
                        #[cfg(feature = "tracing")]
                        tracing::warn!(%error, attempt, slot, "connection to the CSMS failed");
                        let _ = &error;
                        // `NetworkProfileConnectionAttempts`: after this profile's share of
                        // failures, move to the next slot in NetworkConfigurationPriority.
                        cycler.failed();
                    }
                }

                if !reconnect {
                    return Ok(());
                }
                let delay = backoff.delay(attempt, jitter());
                let _ = shared.events.send(Event::Reconnecting { attempt, delay });
                attempt = attempt.saturating_add(1);
                tokio::time::sleep(delay).await;
            }
        };
        Ok((handle, task))
    }
}

impl Connector {
    /// The profile at `index` of the priority list.
    fn profile_at(&self, index: usize) -> (&NetworkProfile, &Endpoint) {
        let profile = self.profiles.at(index);
        let endpoint = self
            .endpoints
            .get(&profile.slot)
            .expect("validated at build time");
        (profile, endpoint)
    }

    /// Opens one connection over the profile at `index` and performs the OCPP-J handshake.
    async fn connect(
        &self,
        index: usize,
    ) -> Result<(WebSocket<MaybeTls>, Version), TransportError> {
        let (profile, endpoint) = self.profile_at(index);
        let versions = profile.versions.as_ref().unwrap_or(&self.versions);

        let tcp = TcpStream::connect((endpoint.host.as_str(), endpoint.port)).await?;
        tcp.set_nodelay(true)?;

        let mut stream = if endpoint.secure {
            #[cfg(feature = "rustls")]
            {
                let tls = profile.tls.as_ref().expect("checked by the builder");
                let name = super::tls::ClientTls::server_name(&endpoint.host)
                    .map_err(|error| TransportError::Configuration(error.to_string()))?;
                MaybeTls::ClientTls(Box::new(tls.connector().connect(name, tcp).await?))
            }
            #[cfg(not(feature = "rustls"))]
            {
                return Err(TransportError::Configuration(
                    "TLS needs the `rustls` feature".into(),
                ));
            }
        } else {
            MaybeTls::Plain(tcp)
        };

        let authorization = if profile.security.uses_basic_auth() {
            let password = profile.password.as_ref().expect("checked by the builder");
            Some(basic_auth_header(&self.identity, password))
        } else {
            None
        };
        #[allow(unused_mut)]
        let mut extensions: Option<String> = None;
        #[cfg(feature = "compression")]
        if self.compression {
            extensions = Some(super::ws::deflate::DeflateParams::default().client_offer());
        }

        let handshake = client_handshake(
            &mut stream,
            &ClientRequest {
                host: &endpoint.authority,
                path: &endpoint.path,
                subprotocols: &versions.header_value(),
                authorization: authorization.as_deref(),
                extensions: extensions.as_deref(),
            },
        )
        .await?;

        // Part 4 §3.1.2 — the server selects exactly one of the offered subprotocols, and a
        // handshake without the header is a negotiation failure, not a 1.6 fallback.
        let version = versions.accept(handshake.subprotocol.as_deref())?;

        #[allow(unused_mut)]
        let mut codec = WsCodec::new(WsRole::Client, self.ws_config);
        #[cfg(feature = "compression")]
        if extensions.is_some() {
            // §3.4: a CSMS that declines compression is served uncompressed, and the station
            // must not close the connection over it.
            match super::ws::deflate::accept_response(handshake.extensions.as_deref()) {
                Ok(Some(params)) => codec = codec.with_deflate(params),
                Ok(None) => {}
                Err(error) => {
                    return Err(TransportError::Configuration(format!(
                        "permessage-deflate negotiation failed: {error}"
                    )));
                }
            }
        }

        Ok((super::ws::attach(stream, codec, &handshake.rest), version))
    }
}

/// Where to connect, and under which identity.
#[derive(Clone, Debug)]
struct Endpoint {
    /// The configuration slot this endpoint belongs to.
    slot: i32,
    host: String,
    port: u16,
    secure: bool,
    /// The `Host` header value: host, plus the port when it is not the default.
    authority: String,
    /// The request target, including the identity path segment.
    path: String,
    /// The full URL, which is what a configuration error quotes back.
    url: String,
}

impl Endpoint {
    /// Validates one configuration slot: the URL, and that its credentials match its
    /// security profile.
    fn for_profile(
        profile: &NetworkProfile,
        identity: &Identity,
        default_versions: &Subprotocol,
    ) -> Result<Self, TransportError> {
        let endpoint = Self::parse(profile.slot, &profile.url, identity)?;
        let slot = profile.slot;
        let security = profile.security;

        if endpoint.secure != security.is_transport_encrypted() {
            return Err(TransportError::Configuration(format!(
                "configuration slot {slot}: {security} and the URL {} disagree about TLS",
                endpoint.url
            )));
        }
        if security.uses_basic_auth() && profile.password.is_none() {
            return Err(TransportError::Configuration(format!(
                "configuration slot {slot}: {security} needs a password"
            )));
        }
        #[cfg(feature = "rustls")]
        if security.is_transport_encrypted() && profile.tls.is_none() {
            return Err(TransportError::Configuration(format!(
                "configuration slot {slot}: {security} needs a TLS configuration"
            )));
        }
        #[cfg(not(feature = "rustls"))]
        if security.is_transport_encrypted() {
            return Err(TransportError::Configuration(
                "TLS needs the `rustls` feature".into(),
            ));
        }
        if profile
            .versions
            .as_ref()
            .unwrap_or(default_versions)
            .offered()
            .is_empty()
        {
            return Err(TransportError::Configuration(format!(
                "configuration slot {slot}: at least one version must be offered"
            )));
        }
        Ok(endpoint)
    }

    /// Splits a `ws://` / `wss://` base URL and appends the identity.
    fn parse(slot: i32, base: &str, identity: &Identity) -> Result<Self, TransportError> {
        let uri: Uri = base
            .parse()
            .map_err(|_| TransportError::Url(base.to_owned()))?;
        let scheme = uri.scheme_str().unwrap_or("ws");
        let secure = match scheme {
            "ws" | "http" => false,
            "wss" | "https" => true,
            _ => return Err(TransportError::Url(base.to_owned())),
        };
        let host = uri
            .host()
            .ok_or_else(|| TransportError::Url(base.to_owned()))?
            .to_owned();
        let port = uri.port_u16().unwrap_or(if secure { 443 } else { 80 });
        // Part 4 §3.1: the identity is the last path segment.
        let path = format!(
            "{}/{}",
            uri.path().trim_end_matches('/'),
            percent_encode(identity.as_str())
        );
        let authority = if uri.port().is_some() {
            format!("{host}:{port}")
        } else {
            host.clone()
        };
        let url = format!("{}://{authority}{path}", if secure { "wss" } else { "ws" });
        Ok(Self {
            slot,
            host,
            port,
            secure,
            authority,
            path,
            url,
        })
    }
}

/// A jitter value in `0.0 ..= 1.0` for the reconnect back-off (Part 4 §5.4).
///
/// Drawn from the operating system rather than the clock. The whole point of §5.4's random
/// range is that a fleet coming back after a CSMS restart does not arrive together — and a
/// fleet is exactly the population whose clocks are synchronised to the same NTP source, so
/// clock-derived "randomness" is correlated precisely when it must not be.
fn jitter() -> f64 {
    let mut bytes = [0u8; 4];
    if getrandom::fill(&mut bytes).is_err() {
        return 0.5;
    }
    f64::from(u32::from_le_bytes(bytes)) / f64::from(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_identity_becomes_the_last_path_segment() {
        let identity = Identity::new("CS-0001").unwrap();
        let endpoint = Endpoint::parse(0, "wss://csms.example.com/ocpp", &identity).unwrap();
        assert_eq!(endpoint.url, "wss://csms.example.com/ocpp/CS-0001");
        assert_eq!(endpoint.port, 443);
        assert!(endpoint.secure);

        let endpoint = Endpoint::parse(0, "ws://localhost:9000/", &identity).unwrap();
        assert_eq!(endpoint.url, "ws://localhost:9000/CS-0001");
        assert_eq!(endpoint.port, 9000);
        assert!(!endpoint.secure);
    }

    #[test]
    fn identities_that_need_escaping_still_produce_one_segment() {
        let identity = Identity::new("station/one").unwrap();
        let endpoint = Endpoint::parse(0, "ws://host/ocpp", &identity).unwrap();
        assert_eq!(endpoint.url, "ws://host/ocpp/station%2Fone");
    }

    #[test]
    fn the_builder_refuses_contradictory_security_settings() {
        let error = Station::builder()
            .identity("CS-0001")
            .unwrap()
            .url("ws://host/ocpp")
            .security_profile(SecurityProfile::TlsBasicAuth)
            .build()
            .err()
            .expect("a TLS profile on a ws:// URL is a contradiction");
        assert!(matches!(error, TransportError::Configuration(_)), "{error}");

        let error = Station::builder()
            .identity("CS-0001")
            .unwrap()
            .url("ws://host/ocpp")
            .security_profile(SecurityProfile::BasicAuth)
            .build()
            .err()
            .expect("profile 1 needs a password");
        assert!(error.to_string().contains("needs a password"), "{error}");
    }

    #[test]
    fn every_configuration_slot_is_validated_at_build_time() {
        use super::super::network_profile::{NetworkProfile, NetworkProfiles};

        // The *fallback* slot is misconfigured. Discovering that only when the primary CSMS
        // goes down would be the worst possible moment.
        let error = Station::builder()
            .identity("CS-0001")
            .unwrap()
            .network_profiles(
                NetworkProfiles::new([
                    NetworkProfile::new(0, "ws://fallback/ocpp")
                        .security_profile(SecurityProfile::BasicAuth),
                    NetworkProfile::new(1, "ws://primary/ocpp")
                        .security_profile(SecurityProfile::BasicAuth)
                        .password(BasicAuthPassword::utf8("0123456789abcdef").unwrap()),
                ])
                .priority([1, 0])
                .unwrap(),
            )
            .build()
            .err()
            .expect("slot 0 has no password");
        assert!(
            error.to_string().contains("configuration slot 0"),
            "{error}"
        );
    }
}
