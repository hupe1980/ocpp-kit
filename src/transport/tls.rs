//! TLS for security profiles 2 and 3, on `rustls` (feature `rustls`).
//!
//! `rustls` negotiates TLS 1.2 and 1.3 only and offers no anonymous, null or export cipher
//! suites, which is exactly what A00.FR.313 / A00.FR.416 and A00.FR.320 / A00.FR.423
//! require. Nothing here can be configured back below that line.

use std::fmt;
use std::io;
use std::path::Path;
use std::sync::Arc;

use tokio_rustls::rustls;
use tokio_rustls::rustls::pki_types::pem::PemObject as _;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};

/// Why a TLS configuration could not be built.
#[derive(Debug)]
#[non_exhaustive]
pub enum TlsError {
    /// A PEM file could not be read.
    Io(io::Error),
    /// `rustls` rejected the configuration.
    Rustls(rustls::Error),
    /// A PEM file held nothing usable.
    Pem(String),
}

impl fmt::Display for TlsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TlsError::Io(error) => write!(f, "reading a PEM file: {error}"),
            TlsError::Rustls(error) => write!(f, "rustls: {error}"),
            TlsError::Pem(what) => write!(f, "{what}"),
        }
    }
}

impl std::error::Error for TlsError {}

impl From<io::Error> for TlsError {
    fn from(error: io::Error) -> Self {
        TlsError::Io(error)
    }
}

impl From<rustls::Error> for TlsError {
    fn from(error: rustls::Error) -> Self {
        TlsError::Rustls(error)
    }
}

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    // Chosen explicitly rather than through the process-wide default, so a host application
    // that installs a different provider cannot silently change our cipher suites.
    Arc::new(rustls::crypto::ring::default_provider())
}

fn read_certs(path: &Path) -> Result<Vec<CertificateDer<'static>>, TlsError> {
    let certs: Vec<_> = CertificateDer::pem_file_iter(path)
        .map_err(|error| TlsError::Pem(format!("{}: {error}", path.display())))?
        .collect::<Result<_, _>>()
        .map_err(|error| TlsError::Pem(format!("{}: {error}", path.display())))?;
    if certs.is_empty() {
        return Err(TlsError::Pem(format!(
            "{} contains no certificates",
            path.display()
        )));
    }
    Ok(certs)
}

fn read_key(path: &Path) -> Result<PrivateKeyDer<'static>, TlsError> {
    PrivateKeyDer::from_pem_file(path)
        .map_err(|error| TlsError::Pem(format!("{}: {error}", path.display())))
}

/// The client side of TLS, for a Charging Station using security profile 2 or 3.
///
/// ```no_run
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// use ocpp_kit::transport::ClientTls;
///
/// // Profile 2: trust one private CSMS root.
/// let profile2 = ClientTls::builder().root_file("csms-root.pem")?.build()?;
///
/// // Profile 3: additionally present the station's own certificate.
/// let profile3 = ClientTls::builder()
///     .root_file("csms-root.pem")?
///     .client_certificate("station-chain.pem", "station-key.pem")?
///     .build()?;
/// # let _ = (profile2, profile3);
/// # Ok(()) }
/// ```
#[derive(Clone)]
pub struct ClientTls {
    config: Arc<rustls::ClientConfig>,
}

impl ClientTls {
    /// Starts an empty configuration.
    #[must_use]
    pub fn builder() -> ClientTlsBuilder {
        ClientTlsBuilder {
            roots: rustls::RootCertStore::empty(),
            identity: None,
            allow_wildcards: false,
        }
    }

    /// Trusts the Mozilla root programme, which is what a publicly issued CSMS certificate
    /// chains to.
    pub fn with_webpki_roots() -> Result<Self, TlsError> {
        ClientTls::builder().webpki_roots().build()
    }

    /// Trusts exactly the certificates in `path` — the usual choice, since a CSMS root is
    /// normally private (`InstallCertificate` with `CSMSRootCertificate`).
    pub fn with_root_file(path: impl AsRef<Path>) -> Result<Self, TlsError> {
        ClientTls::builder().root_file(path)?.build()
    }

    /// Wraps an existing `rustls` configuration.
    #[must_use]
    pub fn from_config(config: Arc<rustls::ClientConfig>) -> Self {
        Self { config }
    }

    pub(crate) fn connector(&self) -> tokio_rustls::TlsConnector {
        tokio_rustls::TlsConnector::from(self.config.clone())
    }

    pub(crate) fn server_name(host: &str) -> Result<ServerName<'static>, TlsError> {
        ServerName::try_from(host.to_owned())
            .map_err(|_| TlsError::Pem(format!("{host} is not a valid TLS server name")))
    }
}

impl fmt::Debug for ClientTls {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ClientTls")
    }
}

/// The standard verifier, plus the refusal of a wildcard name ([`ClientTlsBuilder::
/// allow_wildcard_server_certificates`]).
///
/// The chain, validity and host name are checked by the inner verifier first; only a
/// certificate that already passes is then refused for carrying a wildcard. rustls matches the
/// subjectAltName and ignores the commonName, so the SAN is where a wildcard takes effect.
#[derive(Debug)]
struct NoWildcards {
    inner: Arc<rustls::client::WebPkiServerVerifier>,
}

impl rustls::client::danger::ServerCertVerifier for NoWildcards {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        let verified = self.inner.verify_server_cert(
            end_entity,
            intermediates,
            server_name,
            ocsp_response,
            now,
        )?;
        let certificate = webpki::EndEntityCert::try_from(end_entity).map_err(|_| {
            rustls::Error::InvalidCertificate(rustls::CertificateError::BadEncoding)
        })?;
        if certificate
            .valid_dns_names()
            .any(|name| name.starts_with("*."))
        {
            return Err(rustls::Error::InvalidCertificate(
                rustls::CertificateError::ApplicationVerificationFailure,
            ));
        }
        Ok(verified)
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.inner.supported_verify_schemes()
    }
}

/// Collects the trust anchors and, for profile 3, the client identity.
pub struct ClientTlsBuilder {
    roots: rustls::RootCertStore,
    identity: Option<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>)>,
    allow_wildcards: bool,
}

impl ClientTlsBuilder {
    /// Adds the Mozilla root programme.
    #[must_use]
    pub fn webpki_roots(mut self) -> Self {
        self.roots
            .roots
            .extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        self
    }

    /// Adds every certificate in a PEM file as a trust anchor.
    pub fn root_file(mut self, path: impl AsRef<Path>) -> Result<Self, TlsError> {
        for cert in read_certs(path.as_ref())? {
            self.roots.add(cert)?;
        }
        Ok(self)
    }

    /// Adds a trust anchor from DER.
    pub fn root_der(mut self, der: CertificateDer<'static>) -> Result<Self, TlsError> {
        self.roots.add(der)?;
        Ok(self)
    }

    /// Presents this certificate chain and key to the CSMS — security profile 3.
    pub fn client_certificate(
        mut self,
        chain: impl AsRef<Path>,
        key: impl AsRef<Path>,
    ) -> Result<Self, TlsError> {
        self.identity = Some((read_certs(chain.as_ref())?, read_key(key.as_ref())?));
        Ok(self)
    }

    /// Accepts a CSMS certificate whose name is a wildcard (`*.example.com`).
    ///
    /// Refused by default, on every version: "OCPP-compliant behavior is that a Charging
    /// Station rejects a connection from a CSMS that presents a wildcard certificate. It is
    /// highly RECOMMENDED to not allow wildcard certificates" (2.x
    /// `SecurityCtrlr.AllowCSMSTLSWildcards`; 1.6 Security Whitepaper edition 4 §7.7,
    /// `AllowCentralSystemTLSWildcards`). Call this when that variable is `true`.
    #[must_use]
    pub fn allow_wildcard_server_certificates(mut self) -> Self {
        self.allow_wildcards = true;
        self
    }

    /// Builds the configuration.
    pub fn build(self) -> Result<ClientTls, TlsError> {
        if self.roots.is_empty() {
            return Err(TlsError::Pem(
                "no trust anchors: call `webpki_roots()` or `root_file()`".to_owned(),
            ));
        }
        let provider = provider();
        let webpki = rustls::client::WebPkiServerVerifier::builder_with_provider(
            Arc::new(self.roots),
            provider.clone(),
        )
        .build()
        .map_err(|error| TlsError::Pem(error.to_string()))?;
        let verifier: Arc<dyn rustls::client::danger::ServerCertVerifier> = if self.allow_wildcards
        {
            webpki
        } else {
            Arc::new(NoWildcards { inner: webpki })
        };
        let builder = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()?
            .dangerous()
            .with_custom_certificate_verifier(verifier);
        let config = match self.identity {
            Some((chain, key)) => builder.with_client_auth_cert(chain, key)?,
            None => builder.with_no_client_auth(),
        };
        Ok(ClientTls {
            config: Arc::new(config),
        })
    }
}

/// The server side of TLS, for a CSMS or a Local Controller.
#[derive(Clone)]
pub struct ServerTls {
    config: Arc<rustls::ServerConfig>,
    /// Whether a client certificate is demanded — security profile 3.
    mutual: bool,
}

impl ServerTls {
    /// Serves `chain` / `key` without asking for a client certificate (profile 2).
    pub fn new(chain: impl AsRef<Path>, key: impl AsRef<Path>) -> Result<Self, TlsError> {
        let certs = read_certs(chain.as_ref())?;
        let key = read_key(key.as_ref())?;
        let config = rustls::ServerConfig::builder_with_provider(provider())
            .with_safe_default_protocol_versions()?
            .with_no_client_auth()
            .with_single_cert(certs, key)?;
        Ok(Self {
            config: Arc::new(config),
            mutual: false,
        })
    }

    /// Demands a client certificate issued under `client_roots` — security profile 3.
    ///
    /// The verified end-entity certificate reaches the authenticator as
    /// [`Credentials::ClientCertificate`](super::Credentials::ClientCertificate), so a CSMS
    /// can still bind it to the identity in the URL.
    pub fn with_client_auth(
        chain: impl AsRef<Path>,
        key: impl AsRef<Path>,
        client_roots: impl AsRef<Path>,
    ) -> Result<Self, TlsError> {
        let certs = read_certs(chain.as_ref())?;
        let key = read_key(key.as_ref())?;
        let mut roots = rustls::RootCertStore::empty();
        for cert in read_certs(client_roots.as_ref())? {
            roots.add(cert)?;
        }
        let verifier = rustls::server::WebPkiClientVerifier::builder_with_provider(
            Arc::new(roots),
            provider(),
        )
        .build()
        .map_err(|error| TlsError::Pem(error.to_string()))?;
        let config = rustls::ServerConfig::builder_with_provider(provider())
            .with_safe_default_protocol_versions()?
            .with_client_cert_verifier(verifier)
            .with_single_cert(certs, key)?;
        Ok(Self {
            config: Arc::new(config),
            mutual: true,
        })
    }

    /// Wraps an existing `rustls` configuration.
    #[must_use]
    pub fn from_config(config: Arc<rustls::ServerConfig>, mutual: bool) -> Self {
        Self { config, mutual }
    }

    /// Whether this configuration implements security profile 3.
    #[must_use]
    pub fn is_mutual(&self) -> bool {
        self.mutual
    }

    pub(crate) fn acceptor(&self) -> tokio_rustls::TlsAcceptor {
        tokio_rustls::TlsAcceptor::from(self.config.clone())
    }
}

impl fmt::Debug for ServerTls {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServerTls")
            .field("mutual", &self.mutual)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod wildcard_tests {
    use super::*;
    use rustls::client::danger::ServerCertVerifier as _;

    // `testdata/`: a P-256 CA and two leaves it signed — `csms.example.com` and
    // `*.example.com` — valid for 100 years from 2026-11-07, made with `openssl req -x509` /
    // `openssl x509 -req`. Committed rather than generated per run so the tests need no
    // certificate-generating dependency.
    const CA: &[u8] = include_bytes!("testdata/ca.pem");
    const PLAIN: &[u8] = include_bytes!("testdata/plain.pem");
    const WILD: &[u8] = include_bytes!("testdata/wild.pem");

    fn der(pem: &[u8]) -> CertificateDer<'static> {
        CertificateDer::from_pem_slice(pem).unwrap()
    }

    fn verifier(allow_wildcards: bool) -> Arc<rustls::ClientConfig> {
        let mut builder = ClientTls::builder().root_der(der(CA)).unwrap();
        if allow_wildcards {
            builder = builder.allow_wildcard_server_certificates();
        }
        builder.build().unwrap().config
    }

    /// 2.x `AllowCSMSTLSWildcards`, 1.6 `AllowCentralSystemTLSWildcards`: a wildcard CSMS
    /// certificate is refused unless allowed; a plain one is accepted either way.
    #[test]
    fn a_wildcard_csms_certificate_is_refused_unless_allowed() {
        let name = ServerName::try_from("csms.example.com").unwrap();
        let now = rustls::pki_types::UnixTime::now();
        let check = |leaf: &[u8], name: &ServerName<'_>, allow: bool| {
            let leaf = der(leaf);
            let mut roots = rustls::RootCertStore::empty();
            roots.add(der(CA)).unwrap();
            let inner = rustls::client::WebPkiServerVerifier::builder_with_provider(
                Arc::new(roots),
                provider(),
            )
            .build()
            .unwrap();
            if allow {
                inner
                    .verify_server_cert(&leaf, &[], name, &[], now)
                    .map(|_| ())
            } else {
                NoWildcards { inner }
                    .verify_server_cert(&leaf, &[], name, &[], now)
                    .map(|_| ())
            }
        };
        let other = ServerName::try_from("other.example.org").unwrap();
        assert!(check(PLAIN, &name, false).is_ok());
        assert_eq!(
            check(WILD, &name, false).unwrap_err(),
            rustls::Error::InvalidCertificate(
                rustls::CertificateError::ApplicationVerificationFailure
            )
        );
        assert!(check(WILD, &name, true).is_ok());
        // A certificate that fails the ordinary checks still fails them first.
        assert!(matches!(
            check(PLAIN, &other, false).unwrap_err(),
            rustls::Error::InvalidCertificate(
                rustls::CertificateError::NotValidForName
                    | rustls::CertificateError::NotValidForNameContext { .. }
            )
        ));
    }

    /// The builder wires the refusing verifier in by default.
    #[test]
    fn the_builder_refuses_wildcards_by_default() {
        let strict = verifier(false);
        let lax = verifier(true);
        assert!(format!("{strict:?}").contains("NoWildcards"));
        assert!(!format!("{lax:?}").contains("NoWildcards"));
    }
}
