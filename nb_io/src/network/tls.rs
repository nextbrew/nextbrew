use std::sync::{Arc, OnceLock};

use rustls::DigitallySignedStruct;
use rustls::SignatureScheme;
use rustls::client::WebPkiServerVerifier;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use tracing::warn;

static SHARED_TLS_CONFIG: OnceLock<Arc<rustls::ClientConfig>> = OnceLock::new();

/// Process-wide rustls config used by every reqwest client.
///
/// System trust roots are preferred, but when none are available (e.g. inside
/// a packaging/build sandbox) we fall back to the bundled Mozilla roots from
/// `webpki-roots`. The config is always available, so callers never fall back
/// to reqwest's default TLS (which eagerly loads system roots and panics when
/// none exist).
pub(crate) fn shared_tls_config() -> Arc<rustls::ClientConfig> {
    SHARED_TLS_CONFIG
        .get_or_init(|| Arc::new(build_rustls_config()))
        .clone()
}

fn build_rustls_config() -> rustls::ClientConfig {
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());

    let mut config = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        // The aws-lc-rs provider always supports TLS 1.2/1.3, so this is unreachable.
        // Panicking here (instead of returning None) avoids silently dropping back to
        // reqwest's system-root TLS, which would reintroduce the sandbox CA-cert panic.
        .expect("aws-lc-rs provider supports TLS 1.2 and 1.3")
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(LazyRoots { provider }))
        .with_no_client_auth();
    // reqwest only negotiates HTTP/2 when the TLS config it is handed offers
    // it; without this every connection is HTTP/1.1 and nothing multiplexes.
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    config
}

/// The usual WebPKI verification, with the system roots read at the first
/// handshake rather than up front: reading them from the macOS keychain
/// takes over 100ms, which every command paid even when it made no request.
#[derive(Debug)]
struct LazyRoots {
    provider: Arc<CryptoProvider>,
}

static ROOTS: OnceLock<Arc<WebPkiServerVerifier>> = OnceLock::new();

impl LazyRoots {
    fn verifier(&self) -> &WebPkiServerVerifier {
        roots(&self.provider)
    }
}

fn roots(provider: &Arc<CryptoProvider>) -> &'static WebPkiServerVerifier {
    ROOTS.get_or_init(|| {
        WebPkiServerVerifier::builder_with_provider(Arc::new(native_root_store()), provider.clone())
            .build()
            .expect("the root store is never empty")
    })
}

/// Start reading the system roots in the background, for a command about
/// to download: the first handshake then waits for less of it.
pub(crate) fn preload_roots() {
    std::thread::spawn(|| {
        roots(&Arc::new(rustls::crypto::aws_lc_rs::default_provider()));
    });
}

impl ServerCertVerifier for LazyRoots {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        self.verifier().verify_server_cert(
            end_entity,
            intermediates,
            server_name,
            ocsp_response,
            now,
        )
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

fn native_root_store() -> rustls::RootCertStore {
    let cert_result = rustls_native_certs::load_native_certs();
    if !cert_result.errors.is_empty() {
        let details = cert_result
            .errors
            .iter()
            .take(3)
            .map(std::string::ToString::to_string)
            .collect::<Vec<_>>()
            .join("; ");
        warn!(
            errors = cert_result.errors.len(),
            details = %details,
            "failed to load native certificates"
        );
    }
    assemble_root_store(cert_result.certs)
}

/// Assemble a trust store from the system's native certs, falling back to the
/// bundled Mozilla roots (`webpki-roots`) when no native roots are available
/// (e.g. inside a packaging/build sandbox). The returned store is never empty.
fn assemble_root_store(native_certs: Vec<CertificateDer<'static>>) -> rustls::RootCertStore {
    let mut root_store = rustls::RootCertStore::empty();

    for cert in native_certs {
        let _ = root_store.add(cert);
    }

    if root_store.roots.is_empty() {
        warn!("no native CA certificates found; falling back to bundled webpki-roots trust store");
        root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    }

    root_store
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_rustls_config_does_not_panic() {
        let _ = build_rustls_config();
        let _ = native_root_store();
    }

    #[test]
    fn falls_back_to_webpki_roots_when_native_empty() {
        // Simulates a sandbox with no system trust store: the webpki-roots
        // fallback must still produce a non-empty trust store. This is the
        // regression guard for the "No CA certificates were loaded" panic.
        let store = assemble_root_store(Vec::new());
        assert!(!store.roots.is_empty());
    }
}
