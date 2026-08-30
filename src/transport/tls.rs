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

/// Collects the trust anchors and, for profile 3, the client identity.
pub struct ClientTlsBuilder {
    roots: rustls::RootCertStore,
    identity: Option<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>)>,
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

    /// Builds the configuration.
    pub fn build(self) -> Result<ClientTls, TlsError> {
        if self.roots.is_empty() {
            return Err(TlsError::Pem(
                "no trust anchors: call `webpki_roots()` or `root_file()`".to_owned(),
            ));
        }
        let builder = rustls::ClientConfig::builder_with_provider(provider())
            .with_safe_default_protocol_versions()?
            .with_root_certificates(self.roots);
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
