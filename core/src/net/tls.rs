// File: ./core/src/net/tls.rs
// SPDX-License-Identifier: GPL-3.0-or-later
//! Deterministic TLS for the mesh. Both peers derive the same root CA and
//! leaf certificate from the shared pairing token, so the control layer can
//! verify a peer's identity without any certificate exchange, and the media
//! layer can still encrypt its traffic.

use rcgen::{
    BasicConstraints, CertificateParams, CertifiedIssuer, DnType, ExtendedKeyUsagePurpose, IsCa,
    KeyPair,
};
use rustls::client::WebPkiServerVerifier;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::{
    ClientConfig, DigitallySignedStruct, Error, RootCertStore, ServerConfig, SignatureScheme,
};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};

/// Verifies the token-derived certificate while ignoring the hostname/IP,
/// which may roam (Wi-Fi, VPN, DHCP).
#[derive(Debug)]
pub struct PinVerifier {
    inner: Arc<dyn ServerCertVerifier>,
}

impl ServerCertVerifier for PinVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, Error> {
        // Bypasses the hostname check. Since the certificate is issued by our
        // deterministic Root CA (derived from the token), possessing a valid
        // certificate guarantees authentication regardless of the IP address
        // the peer is currently using.
        let dummy = ServerName::try_from("currant.local").unwrap().to_owned();
        self.inner
            .verify_server_cert(end_entity, intermediates, &dummy, ocsp_response, now)
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        self.inner.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        self.inner.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.inner.supported_verify_schemes()
    }
}

/// Deterministic crypto material derived from the pairing token.
pub struct CryptoMaterial {
    /// Leaf certificate, PEM (for tiny_http).
    pub leaf_pem: Vec<u8>,
    /// Leaf private key, PEM (for tiny_http).
    pub key_pem: Vec<u8>,
    /// Leaf certificate, DER (for the rustls server).
    pub leaf_der: CertificateDer<'static>,
    /// Root CA certificate, DER (for the client's trust store).
    pub ca_der: CertificateDer<'static>,
    /// Leaf private key, DER (for the rustls server).
    pub key_der: PrivateKeyDer<'static>,
}

static CRYPTO_CACHE: LazyLock<Mutex<HashMap<String, Arc<CryptoMaterial>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Memoized access to deterministic crypto material. Generating X.509
/// certificates and signing them is computationally heavy, so it is cached
/// by token.
pub fn get_crypto(token: &str) -> Arc<CryptoMaterial> {
    let mut cache = CRYPTO_CACHE.lock().unwrap();
    cache
        .entry(token.to_string())
        .or_insert_with(|| Arc::new(generate_crypto(token)))
        .clone()
}

/// Derives the deterministic crypto material from the pairing token: a root
/// CA certificate and a leaf certificate signed by it. Both peers derive
/// identical material, so no certificate exchange is ever needed.
pub fn generate_crypto(token: &str) -> CryptoMaterial {
    let _ = rustls::crypto::ring::default_provider().install_default();

    let ca_key = KeyPair::try_from(derive_pkcs8(token, b"ca").as_slice()).expect("valid ca key");
    let leaf_pkcs8 = derive_pkcs8(token, b"leaf");
    let leaf_key = KeyPair::try_from(leaf_pkcs8.as_slice()).expect("valid leaf key");

    // Root CA: the trust anchor both peers pin in their client config.
    let mut ca_params = CertificateParams::new(Vec::<String>::new()).expect("valid params");
    ca_params
        .distinguished_name
        .push(DnType::CommonName, "Currant Root CA");
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let ca = CertifiedIssuer::self_signed(ca_params, ca_key).expect("valid ca");

    // Leaf: what the server actually presents. webpki rejects CA certificates
    // as end entities, so this one must stay a non-CA.
    let mut leaf_params =
        CertificateParams::new(vec!["currant.local".to_string()]).expect("valid params");
    leaf_params
        .distinguished_name
        .push(DnType::CommonName, "currant.local");
    leaf_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    let key_pem = leaf_key.serialize_pem().into_bytes();
    let leaf = CertifiedIssuer::signed_by(leaf_params, leaf_key, &ca).expect("valid leaf");

    CryptoMaterial {
        leaf_pem: leaf.pem().into_bytes(),
        key_pem,
        leaf_der: leaf.der().clone(),
        ca_der: ca.der().clone(),
        key_der: PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(leaf_pkcs8)),
    }
}

/// Wraps the SHA-256 of `token` + `salt` in the PKCS#8 envelope of an
/// Ed25519 private key, so the same token always yields the same key.
fn derive_pkcs8(token: &str, salt: &[u8]) -> Vec<u8> {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    hasher.update(salt);
    let mut pkcs8 = vec![
        0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22, 0x04,
        0x20,
    ];
    pkcs8.extend_from_slice(&hasher.finalize());
    pkcs8
}

/// Server config for the strict TLS control layer (WebSocket).
pub fn server_config(token: &str) -> Arc<ServerConfig> {
    let crypto = get_crypto(token);
    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![crypto.leaf_der.clone()], crypto.key_der.clone_key())
        .expect("bad cert/key");
    Arc::new(config)
}

/// Client config for the strict TLS control layer: verifies the peer's
/// token-derived certificate, ignoring the hostname/IP.
pub fn rustls_client_config(token: &str) -> Arc<ClientConfig> {
    let crypto = get_crypto(token);
    let mut root_store = RootCertStore::empty();
    root_store
        .add(crypto.ca_der.clone())
        .expect("failed to add root");
    let inner = WebPkiServerVerifier::builder(Arc::new(root_store))
        .build()
        .expect("webpki verifier");
    let verifier = Arc::new(PinVerifier { inner });

    let config = ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    Arc::new(config)
}

/// Unverified TLS for the read-only media endpoints (ureq): encrypted
/// tunnel, no certificate verification, no token on the wire.
pub fn ureq_client_config() -> ureq::tls::TlsConfig {
    ureq::tls::TlsConfig::builder()
        .disable_verification(true)
        .build()
}
