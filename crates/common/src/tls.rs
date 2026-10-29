//! The process-wide TLS crypto provider.
//!
//! rustls 0.23 is built here with both of its crypto backends: ring (which
//! the workspace asks for) and aws-lc-rs (which the AWS SDK's HTTPS client
//! and reqwest 0.13 bring). With both compiled in, rustls cannot pick a
//! process default on its own, and any library that builds its TLS config
//! with plain `ClientConfig::builder()` — mysql_async, async-nats,
//! hyper-rustls' native-roots builder — panics on its first TLS connection.
//! Every binary therefore installs ring as the default before it does any
//! network I/O; code that names its provider explicitly is unaffected.

/// Install ring as rustls' process-wide default crypto provider. Call it
/// first thing in `main`. Idempotent: a provider that is already installed
/// (by an earlier call, or by an embedding binary) is kept.
pub fn install_default_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_default_client_config_builds_once_the_provider_is_installed() {
        install_default_crypto_provider();
        let provider =
            rustls::crypto::CryptoProvider::get_default().expect("a default provider is installed");
        assert_eq!(
            provider.cipher_suites,
            rustls::crypto::ring::default_provider().cipher_suites,
            "the default is ring",
        );
        // The call every library that leaves the provider to rustls makes.
        let _config = rustls::ClientConfig::builder()
            .with_root_certificates(rustls::RootCertStore::empty())
            .with_no_client_auth();
    }

    #[test]
    fn installing_twice_keeps_the_first_provider() {
        install_default_crypto_provider();
        install_default_crypto_provider();
        assert!(rustls::crypto::CryptoProvider::get_default().is_some());
    }
}
