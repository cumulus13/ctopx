//! HTTP client for notification channels, tuned for minimal environments (Alpine, Termux)
//! where the usual system files may be missing.

use crate::config::Config;
use rustls::pki_types::{CertificateDer, pem::PemObject};
use std::path::Path;
use std::time::Duration;

/// Extra CA certificates from `network.ca_file` or `SSL_CERT_FILE`. Problems are logged, not fatal.
fn extra_roots(cfg: &Config) -> Vec<CertificateDer<'static>> {
    let path = cfg
        .network
        .ca_file
        .clone()
        .filter(|p| !p.is_empty())
        .or_else(|| {
            std::env::var("SSL_CERT_FILE")
                .ok()
                .filter(|p| !p.is_empty())
        });
    let Some(path) = path else { return vec![] };
    match CertificateDer::pem_file_iter(&path).and_then(|it| it.collect::<Result<Vec<_>, _>>()) {
        Ok(certs) => {
            tracing::info!("loaded {} extra CA certificate(s) from {path}", certs.len());
            certs
        }
        Err(e) => {
            tracing::warn!("cannot read CA file {path}: {e}");
            vec![]
        }
    }
}

/// Static musl builds (Alpine, Termux) and native Android builds carry Mozilla's root
/// certificates inside the binary, so HTTPS works without a system certificate bundle.
/// Other builds use the platform's trust store.
#[cfg(any(target_env = "musl", target_os = "android"))]
fn apply_tls(
    b: reqwest::ClientBuilder,
    extra: Vec<CertificateDer<'static>>,
) -> reqwest::ClientBuilder {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    for c in extra {
        if let Err(e) = roots.add(c) {
            tracing::warn!("ignoring an invalid extra CA certificate: {e}");
        }
    }
    let provider = std::sync::Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("rustls protocol versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
    b.use_preconfigured_tls(config)
}

#[cfg(not(any(target_env = "musl", target_os = "android")))]
fn apply_tls(
    b: reqwest::ClientBuilder,
    extra: Vec<CertificateDer<'static>>,
) -> reqwest::ClientBuilder {
    let certs: Vec<reqwest::Certificate> = extra
        .iter()
        .filter_map(|c| reqwest::Certificate::from_der(c).ok())
        .collect();
    if certs.is_empty() {
        b
    } else {
        b.tls_certs_merge(certs)
    }
}

pub fn build_client(cfg: &Config) -> reqwest::Client {
    let mut b = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .user_agent(concat!("ctopx/", env!("CARGO_PKG_VERSION")));
    b = apply_tls(b, extra_roots(cfg));
    // The static musl resolver reads /etc/resolv.conf, which Android (Termux) does not have.
    // Without it use the built-in resolver, which falls back to public DNS servers.
    if cfg!(target_env = "musl") && !Path::new("/etc/resolv.conf").exists() {
        tracing::warn!(
            "/etc/resolv.conf not found: notification channels use the built-in DNS resolver (public DNS fallback)"
        );
        b = b.hickory_dns(true);
    }
    b.build().expect("http client")
}
