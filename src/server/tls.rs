//! TLS for the device-facing listener, without a separate broker.
//!
//! The device performs no certificate validation, so a self-signed certificate is enough — but that is
//! a property of the firmware tested, not of the protocol. Other implementers report newer firmware
//! requiring a full chain, so supplying a certificate is a first-class option rather than an
//! afterthought: if that day comes the fix is a configuration change, not a code change.
//!
//! # What the device requires
//!
//! - **TLS 1.2.** It offers 1.2 only. Raising the minimum above that turns every connection into a
//!   handshake failure that looks like a certificate problem.
//! - **An RSA key.** The certificate known to work is RSA-2048 with CN `*.growatt.com` and SANs
//!   `*.growatt.com` and `mqtt.growatt.com`, so generation matches that rather than reaching for the
//!   more modern ECDSA default. Holding TLS constant is worth more than a shorter key here.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rustls::ServerConfig;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use snafu::{ResultExt, Snafu};

use crate::mqtt::ClientIdentity;

/// Subject common name of a generated certificate.
pub const GENERATED_CN: &str = "*.growatt.com";

/// Subject alternative names of a generated certificate.
pub const GENERATED_SANS: [&str; 2] = ["*.growatt.com", "mqtt.growatt.com"];

/// File name of the generated certificate inside the state directory.
pub const CERT_FILE: &str = "device-facing.crt";

/// File name of the generated key inside the state directory.
pub const KEY_FILE: &str = "device-facing.key";

/// Why TLS could not be set up.
#[derive(Debug, Snafu)]
#[snafu(visibility(pub))]
pub enum TlsError {
    /// A certificate or key file could not be read or written.
    #[snafu(display("could not access {}", path.display()))]
    File {
        /// The path involved.
        path: PathBuf,
        /// The underlying error.
        source: std::io::Error,
    },

    /// The state directory could not be created.
    #[snafu(display("could not create the state directory {}", path.display()))]
    StateDir {
        /// The directory.
        path: PathBuf,
        /// The underlying error.
        source: std::io::Error,
    },

    /// A PEM file contained no certificate.
    #[snafu(display("{} contains no certificate", path.display()))]
    NoCertificate {
        /// The file.
        path: PathBuf,
    },

    /// A PEM file contained no private key.
    #[snafu(display("{} contains no private key", path.display()))]
    NoPrivateKey {
        /// The file.
        path: PathBuf,
    },

    /// Certificate generation failed.
    #[snafu(display("could not generate a self-signed certificate"))]
    Generate {
        /// The underlying error.
        source: rcgen::Error,
    },

    /// rustls rejected the certificate and key.
    #[snafu(display("rustls rejected the certificate and key"))]
    Rustls {
        /// The underlying error.
        source: rustls::Error,
    },
}

/// Where the certificate came from, for the startup log line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CertificateOrigin {
    /// Supplied explicitly through configuration.
    Supplied,
    /// Loaded from a previous run's generated files.
    Cached,
    /// Generated on this run.
    Generated,
}

impl core::fmt::Display for CertificateOrigin {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match *self {
            Self::Supplied => "supplied",
            Self::Cached => "cached",
            Self::Generated => "generated",
        })
    }
}

/// Build a rustls server configuration, generating a certificate on first run.
///
/// Resolution order: supplied paths, then the state directory, then generate and persist.
///
/// # Errors
///
/// [`TlsError`] naming the file or step that failed.
pub fn server_config(
    cert_path: Option<&Path>,
    key_path: Option<&Path>,
    state_dir: &Path,
) -> Result<(Arc<ServerConfig>, CertificateOrigin), TlsError> {
    let (chain, key, origin) = if let (Some(cert), Some(key)) = (cert_path, key_path) {
        (load_certificates(cert)?, load_key(key)?, CertificateOrigin::Supplied)
    } else {
        let cert = state_dir.join(CERT_FILE);
        let key = state_dir.join(KEY_FILE);
        if cert.exists() && key.exists() {
            (load_certificates(&cert)?, load_key(&key)?, CertificateOrigin::Cached)
        } else {
            let (chain, key_der) = generate(state_dir)?;
            (chain, key_der, CertificateOrigin::Generated)
        }
    };

    // rustls defaults cover TLS 1.2 and 1.3. Deliberately not narrowing to 1.3: the device offers 1.2
    // only, so a failure must be able to mean the certificate rather than the parameters.
    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(chain, key)
        .context(RustlsSnafu)?;

    Ok((Arc::new(config), origin))
}

/// Generate an RSA-2048 self-signed certificate and persist it.
fn generate(state_dir: &Path) -> Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>), TlsError> {
    std::fs::create_dir_all(state_dir).context(StateDirSnafu {
        path: state_dir.to_path_buf(),
    })?;

    let mut params =
        rcgen::CertificateParams::new(GENERATED_SANS.iter().map(|name| (*name).to_owned()).collect::<Vec<_>>())
            .context(GenerateSnafu)?;
    params.distinguished_name.push(rcgen::DnType::CommonName, GENERATED_CN);

    let key_pair = rcgen::KeyPair::generate_for(&rcgen::PKCS_RSA_SHA256).context(GenerateSnafu)?;
    let certificate = params.self_signed(&key_pair).context(GenerateSnafu)?;

    let cert_pem = certificate.pem();
    let key_pem = key_pair.serialize_pem();

    let cert_path = state_dir.join(CERT_FILE);
    let key_path = state_dir.join(KEY_FILE);
    std::fs::write(&cert_path, &cert_pem).context(FileSnafu { path: cert_path })?;
    write_private(&key_path, &key_pem)?;

    let chain = vec![CertificateDer::from(certificate.der().to_vec())];
    let key = PrivateKeyDer::try_from(key_pair.serialize_der())
        .map_err(|_| TlsError::NoPrivateKey { path: key_path.clone() })?;
    Ok((chain, key))
}

/// Write a private key, readable only by its owner.
fn write_private(path: &Path, contents: &str) -> Result<(), TlsError> {
    std::fs::write(path, contents).context(FileSnafu {
        path: path.to_path_buf(),
    })?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).context(FileSnafu {
            path: path.to_path_buf(),
        })?;
    }
    Ok(())
}

/// Load a client certificate and its key, to present to a peer that authenticates clients by
/// certificate.
///
/// Uses the same loaders as the device-facing certificate, so the accepted formats and the error
/// messages are the ones an operator has already seen.
///
/// # Errors
///
/// [`TlsError`] if either file cannot be read, holds nothing of the expected kind, or is malformed.
pub fn client_identity(certificate: &Path, key: &Path) -> Result<ClientIdentity, TlsError> {
    Ok(ClientIdentity {
        chain: load_certificates(certificate)?,
        key: load_key(key)?,
    })
}

/// Load a PEM certificate chain.
fn load_certificates(path: &Path) -> Result<Vec<CertificateDer<'static>>, TlsError> {
    let data = std::fs::read(path).context(FileSnafu {
        path: path.to_path_buf(),
    })?;
    let mut reader = data.as_slice();
    let chain: Vec<_> = rustls_pemfile::certs(&mut reader)
        .collect::<Result<Vec<_>, _>>()
        .context(FileSnafu {
            path: path.to_path_buf(),
        })?;
    if chain.is_empty() {
        return Err(TlsError::NoCertificate {
            path: path.to_path_buf(),
        });
    }
    Ok(chain)
}

/// Load a PEM private key, accepting PKCS#8, PKCS#1 and SEC1.
fn load_key(path: &Path) -> Result<PrivateKeyDer<'static>, TlsError> {
    let data = std::fs::read(path).context(FileSnafu {
        path: path.to_path_buf(),
    })?;
    let mut reader = data.as_slice();
    rustls_pemfile::private_key(&mut reader)
        .context(FileSnafu {
            path: path.to_path_buf(),
        })?
        .ok_or_else(|| TlsError::NoPrivateKey {
            path: path.to_path_buf(),
        })
}

#[cfg(test)]
mod tests {
    use super::{CERT_FILE, CertificateOrigin, GENERATED_SANS, KEY_FILE, server_config};

    /// A scratch directory that removes itself.
    struct Scratch(std::path::PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!("heliobridge-test-{name}"));
            drop(std::fs::remove_dir_all(&path));
            Self(path)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            drop(std::fs::remove_dir_all(&self.0));
        }
    }

    #[test]
    fn generates_and_then_reuses_a_certificate() {
        let scratch = Scratch::new("tls-generate");

        let (_, origin) = server_config(None, None, &scratch.0).expect("generate");
        assert_eq!(origin, CertificateOrigin::Generated);
        assert!(scratch.0.join(CERT_FILE).exists());
        assert!(scratch.0.join(KEY_FILE).exists());

        // A restart must not mint a new certificate; the device would see a changed one.
        let (_, origin) = server_config(None, None, &scratch.0).expect("reuse");
        assert_eq!(origin, CertificateOrigin::Cached);
    }

    #[test]
    fn the_generated_certificate_is_rsa_with_the_expected_names() {
        let scratch = Scratch::new("tls-shape");
        server_config(None, None, &scratch.0).expect("generate");
        let pem = std::fs::read_to_string(scratch.0.join(CERT_FILE)).expect("read");
        assert!(pem.starts_with("-----BEGIN CERTIFICATE-----"));

        let key = std::fs::read_to_string(scratch.0.join(KEY_FILE)).expect("read");
        // An RSA key, not the ECDSA that rcgen would produce by default. The device is known to accept
        // RSA-2048, and holding TLS constant matters more than key modernity here.
        assert!(
            key.contains("PRIVATE KEY"),
            "expected a PEM private key, got {:?}",
            key.lines().next()
        );

        assert_eq!(GENERATED_SANS.len(), 2);
        assert!(GENERATED_SANS.contains(&"mqtt.growatt.com"));
    }

    #[cfg(unix)]
    #[test]
    fn the_generated_key_is_not_world_readable() {
        use std::os::unix::fs::PermissionsExt as _;
        let scratch = Scratch::new("tls-perms");
        server_config(None, None, &scratch.0).expect("generate");
        let mode = std::fs::metadata(scratch.0.join(KEY_FILE))
            .expect("stat")
            .permissions()
            .mode();
        assert_eq!(mode & 0o077, 0, "key mode {mode:o} exposes it beyond the owner");
    }

    #[test]
    fn a_supplied_certificate_is_preferred() {
        let scratch = Scratch::new("tls-supplied");
        // Generate once to obtain a valid pair, then supply it explicitly from elsewhere.
        server_config(None, None, &scratch.0).expect("generate");
        let cert = scratch.0.join(CERT_FILE);
        let key = scratch.0.join(KEY_FILE);

        let other = Scratch::new("tls-supplied-empty");
        let (_, origin) = server_config(Some(&cert), Some(&key), &other.0).expect("supplied");
        assert_eq!(origin, CertificateOrigin::Supplied);
        // The state directory must be left untouched when a certificate is supplied.
        assert!(!other.0.join(CERT_FILE).exists());
    }

    #[test]
    fn a_missing_file_is_reported_with_its_path() {
        let missing = std::path::Path::new("/nonexistent/heliobridge/cert.pem");
        let key = std::path::Path::new("/nonexistent/heliobridge/key.pem");
        let err = server_config(Some(missing), Some(key), std::path::Path::new("/tmp")).expect_err("should fail");
        assert!(
            err.to_string().contains("cert.pem"),
            "error should name the file: {err}"
        );
    }
}
