//! BoringSSL transport configured from a BrowserProfile.
//!
//! Chrome-true, not Chrome-like: we drive Chrome's own TLS library with its
//! native Chrome behaviors on (GREASE, extension permutation, ECH-GREASE,
//! ALPS, brotli cert compression), configured from live-captured Chrome data.

use boring::ssl::{Ssl, SslConnector, SslMethod, SslSession, SslVersion};
use boring::x509::X509;
use foreign_types::ForeignType;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::net::TcpStream;
use tokio_boring::{SslStream, SslStreamBuilder};

use crate::error::FetchError;
use crate::profile::BrowserProfile;

/// Per-origin TLS session-ticket store (Chrome resumes sessions; so do we).
pub type SessionStore = Arc<Mutex<HashMap<String, SslSession>>>;

pub fn new_session_store() -> SessionStore {
    Arc::new(Mutex::new(HashMap::new()))
}

fn tls_err<E: std::fmt::Display>(e: E) -> FetchError {
    FetchError::Tls(e.to_string())
}

/// Brotli certificate decompression (TLS compress_certificate, alg id 2).
/// Client-side: servers may send brotli-compressed certificates.
unsafe extern "C" fn cert_decompress_brotli(
    _ssl: *mut boring_sys::SSL,
    out: *mut *mut boring_sys::CRYPTO_BUFFER,
    uncompressed_len: usize,
    input: *const u8,
    in_len: usize,
) -> std::os::raw::c_int {
    unsafe {
        let compressed = std::slice::from_raw_parts(input, in_len);
        let mut decompressed = Vec::with_capacity(uncompressed_len);
        if std::io::Read::read_to_end(
            &mut brotli::Decompressor::new(compressed, 1 << 20),
            &mut decompressed,
        )
        .is_err()
        {
            return 0;
        }
        if decompressed.len() != uncompressed_len {
            return 0;
        }
        let mut data_ptr: *mut u8 = std::ptr::null_mut();
        let buf = boring_sys::CRYPTO_BUFFER_alloc(&mut data_ptr, uncompressed_len);
        if buf.is_null() || data_ptr.is_null() {
            return 0;
        }
        std::ptr::copy_nonoverlapping(decompressed.as_ptr(), data_ptr, uncompressed_len);
        *out = buf;
        1
    }
}

/// Chrome's ALPS payload for h2: the same SETTINGS it sends on the wire.
fn alps_h2_payload(profile: &BrowserProfile) -> Vec<u8> {
    let h2 = &profile.h2;
    let mut v = Vec::with_capacity(24);
    for (id, val) in [
        (0x1u16, h2.header_table_size),
        (0x2, h2.enable_push),
        (0x4, h2.initial_window_size),
        (0x6, h2.max_header_list_size),
    ] {
        v.extend_from_slice(&id.to_be_bytes());
        v.extend_from_slice(&val.to_be_bytes());
    }
    v
}

pub fn build_connector(
    profile: &BrowserProfile,
    _sessions: SessionStore,
) -> Result<SslConnector, FetchError> {
    let mut b = SslConnector::builder(SslMethod::tls()).map_err(tls_err)?;
    b.set_min_proto_version(Some(SslVersion::TLS1_2))
        .map_err(tls_err)?;
    b.set_max_proto_version(Some(SslVersion::TLS1_3))
        .map_err(tls_err)?;
    b.set_cipher_list(profile.tls.ciphers_12).map_err(tls_err)?;
    b.set_curves_list(profile.tls.groups).map_err(tls_err)?;
    b.set_sigalgs_list(profile.tls.sigalgs).map_err(tls_err)?;
    b.set_alpn_protos(profile.tls.alpn).map_err(tls_err)?;
    b.set_grease_enabled(true);
    b.set_permute_extensions(true);

    // Session storage lives in connect(): tickets are
    // egress-scoped there (a proxy's ticket must never
    // resume from the direct IP or another proxy : that
    // would link the lanes at the edge).

    // OCSP stapling request (status_request extension), like Chrome.
    unsafe { boring_sys::SSL_CTX_enable_ocsp_stapling(b.as_ptr()) };
    // SCT requests (signed_certificate_timestamp extension), like Chrome.
    unsafe { boring_sys::SSL_CTX_enable_signed_cert_timestamps(b.as_ptr()) };

    // Brotli certificate compression (compress_certificate ext, alg 2).
    // Client direction: compress = NULL (never used), real brotli decompress.
    let rc = unsafe {
        boring_sys::SSL_CTX_add_cert_compression_alg(
            b.as_ptr(),
            2,
            None,
            Some(cert_decompress_brotli),
        )
    };
    if rc != 1 {
        return Err(FetchError::Tls(
            "cert compression registration failed".into(),
        ));
    }

    // Platform-native root store (Chrome uses the OS trust store; so do we).
    let roots = rustls_native_certs::load_native_certs();
    let mut loaded = 0usize;
    for cert in roots.certs {
        if let Ok(x) = X509::from_der(cert.as_ref())
            && b.cert_store_mut().add_cert(x).is_ok()
        {
            loaded += 1;
        }
    }
    if loaded == 0 {
        return Err(FetchError::Tls("no platform root certs loaded".into()));
    }

    Ok(b.build())
}

/// Handshake. Applies per-connection profile bits (ECH-GREASE, ALPS),
/// resumes a cached session when the origin gave us a ticket, then connects.
pub async fn connect(
    profile: &BrowserProfile,
    connector: &SslConnector,
    domain: &str,
    tcp: TcpStream,
    sessions: &SessionStore,
    session_key: &str,
) -> Result<SslStream<TcpStream>, FetchError> {
    let mut ssl: Ssl = connector
        .configure()
        .map_err(tls_err)?
        .into_ssl(domain)
        .map_err(tls_err)?;

    // Session resumption (ticket from a previous visit to this origin).
    if let Ok(guard) = sessions.lock()
        && let Some(session) = guard.get(session_key)
    {
        // Safe: session belongs to this client ctx; stale ticket just
        // falls back to a full handshake.
        let _ = unsafe { ssl.set_session(session) };
    }

    // ECH-GREASE (encrypted_client_hello extension), like Chrome.
    ssl.set_enable_ech_grease(true);

    // ALPS (application_settings extension) with Chrome's h2 settings payload.
    let alps = alps_h2_payload(profile);
    let rc = unsafe {
        boring_sys::SSL_add_application_settings(
            ssl.as_ptr(),
            b"h2".as_ptr(),
            2,
            alps.as_ptr(),
            alps.len(),
        )
    };
    if rc != 1 {
        return Err(FetchError::Tls("ALPS registration failed".into()));
    }

    let stream = SslStreamBuilder::new(ssl, tcp)
        .connect()
        .await
        .map_err(|e| FetchError::Tls(format!("{e:?}")))?;

    // Chrome caches session tickets aggressively : so do
    // we, but EGRESS-SCOPED (session_key carries the
    // proxy id when proxied; see fetch/client.rs).
    if let Some(sess) = stream.ssl().session() {
        let mut store = sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if store.len() >= 512 {
            store.clear(); // sessions are short-lived; wipe + refill
        }
        store.insert(session_key.to_string(), sess.to_owned());
    }
    Ok(stream)
}
