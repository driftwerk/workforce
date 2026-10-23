//! Mutual TLS for the remote runner link: a private CA on both sides, and the
//! peer's common name as the identity check the unix socket gets from its uid.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use eyre::{bail, Context, Result};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::WebPkiClientVerifier;
use rustls::{ClientConfig, RootCertStore, ServerConfig};
use x509_cert::der::asn1::{PrintableStringRef, Utf8StringRef};
use x509_cert::der::oid::db::rfc4519::CN;
use x509_cert::der::Decode;

use super::transport::TlsIdentity;

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

fn certificates(pem: &str, what: &str) -> Result<Vec<CertificateDer<'static>>> {
    let certs = rustls_pemfile::certs(&mut pem.as_bytes())
        .collect::<std::io::Result<Vec<_>>>()
        .with_context(|| format!("parsing the {what} certificate PEM"))?;
    if certs.is_empty() {
        bail!("the {what} certificate PEM holds no certificate");
    }
    Ok(certs)
}

fn private_key(pem: &str) -> Result<PrivateKeyDer<'static>> {
    rustls_pemfile::private_key(&mut pem.as_bytes())
        .context("parsing the private key PEM")?
        .ok_or_else(|| eyre::eyre!("the private key PEM holds no key"))
}

fn roots(ca_pem: &str) -> Result<RootCertStore> {
    let mut store = RootCertStore::empty();
    for cert in certificates(ca_pem, "CA")? {
        store.add(cert).context("adding the CA certificate")?;
    }
    Ok(store)
}

/// A client that presents `identity` and trusts only its CA.
pub fn client_config(identity: &TlsIdentity) -> Result<Arc<ClientConfig>> {
    let config = ClientConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()?
        .with_root_certificates(roots(&identity.ca_pem)?)
        .with_client_auth_cert(
            certificates(&identity.cert_pem, "client")?,
            private_key(&identity.key_pem)?,
        )?;
    Ok(Arc::new(config))
}

/// A daemon that presents `identity` and admits only clients its CA signed.
pub fn server_config(identity: &TlsIdentity) -> Result<Arc<ServerConfig>> {
    let verifier =
        WebPkiClientVerifier::builder_with_provider(Arc::new(roots(&identity.ca_pem)?), provider())
            .build()?;
    let config = ServerConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()?
        .with_client_cert_verifier(verifier)
        .with_single_cert(
            certificates(&identity.cert_pem, "server")?,
            private_key(&identity.key_pem)?,
        )?;
    Ok(Arc::new(config))
}

/// When the first certificate in `pem` stops being valid.
pub fn certificate_expiry(pem: &str) -> Result<DateTime<Utc>> {
    let cert = certificates(pem, "peer")?
        .into_iter()
        .next()
        .expect("checked non-empty");
    let parsed = x509_cert::Certificate::from_der(&cert).context("parsing the certificate")?;
    let not_after = parsed.tbs_certificate.validity.not_after.to_unix_duration();
    DateTime::from_timestamp(not_after.as_secs() as i64, 0)
        .ok_or_else(|| eyre::eyre!("certificate expiry is out of range"))
}

/// The subject common name of a DER certificate, if it carries one.
pub fn common_name(cert: &CertificateDer<'_>) -> Result<Option<String>> {
    let parsed = x509_cert::Certificate::from_der(cert).context("parsing the peer certificate")?;
    Ok(parsed
        .tbs_certificate
        .subject
        .0
        .iter()
        .flat_map(|rdn| rdn.0.iter())
        .find(|attribute| attribute.oid == CN)
        .and_then(|attribute| {
            attribute
                .value
                .decode_as::<Utf8StringRef>()
                .map(|s| s.to_string())
                .or_else(|_| {
                    attribute
                        .value
                        .decode_as::<PrintableStringRef>()
                        .map(|s| s.to_string())
                })
                .ok()
        }))
}
