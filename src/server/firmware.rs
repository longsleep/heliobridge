//! Downloading and keeping firmware the vendor's cloud advertises.
//!
//! The cloud advertises an update by writing a URL into datalogger configuration register 80. This program
//! refuses cloud writes to the configuration space, so the device never installs one — but the write is
//! worth reading: it names the exact image the vendor considers current for this device, on a channel whose
//! object names cannot be guessed and are only ever told to a device.
//!
//! What the advertisement *says* — which register carries one, the value's prefix, the path layout, the
//! identity the device presents when it fetches — reaches this module only through
//! [`Vendor`](crate::vendor::Vendor). Nothing here names a register, a message format or a vendor, so a
//! second vendor's campaign would need no change on this side.
//!
//! # An advertisement only exists while the cloud relay does
//!
//! Advertisements arrive on the cloud-to-device path, so nothing here sees one unless the relay is
//! running: with no relay the device never hears from the vendor's cloud through this program, and there is
//! nothing to notice. Given a relay, every advertisement is logged in full including the URL, because that
//! costs nothing and the alternative is knowing only that *something* was pushed. Fetching is a further
//! **off unless asked for**: reaching out to a vendor host is a different act from observing traffic that
//! arrives anyway.
//!
//! # The request looks like the device's
//!
//! The request comes from the vendor implementation, headers and all, and is sent exactly as given: this
//! module adds nothing of its own, which is why the transfer uses a low-level client rather than a
//! convenience one that would seed an `Accept` header behind its back. Header *order* is not reproduced,
//! which no HTTP implementation is entitled to care about.
//!
//! # Three properties of storing it
//!
//! - **Already held means done.** A file present under its name is left alone, so an advertisement repeated
//!   hourly costs one transfer.
//! - **The cap is enforced twice.** A declared length over the limit is refused before the body is read;
//!   the stream is counted anyway, because a declared length is a claim rather than a fact.
//! - **A partial file never gets the final name.** The body lands under `.part` and is renamed once
//!   complete; a failed transfer removes it.
//!
//! Nothing here installs anything, and nothing hands a URL to the device.

use std::path::{Path, PathBuf};
use std::time::Duration;

use http_body_util::{BodyExt, Empty};
use hyper::body::Bytes;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

use crate::vendor::{AdvertisedFirmware, Vendor};

/// How long a transfer may take, connection included.
const TRANSFER_TIMEOUT: Duration = Duration::from_mins(2);

/// Why a download produced no file.
#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    /// The server answered something other than success.
    #[error("server answered {status}")]
    Status {
        /// The status code.
        status: u16,
    },

    /// The response was larger than the limit in force.
    #[error("response exceeds the {limit}-octet limit")]
    TooLarge {
        /// The limit.
        limit: u64,
    },

    /// Asked to fetch when no directory is being kept.
    #[error("no firmware directory is configured")]
    NothingKept,

    /// Only plain HTTP is implemented. The advertised channel uses it; the automatic one does not.
    #[error("scheme {scheme:?} is not supported; only http is")]
    UnsupportedScheme {
        /// The scheme asked for.
        scheme: String,
    },

    /// The request could not be assembled.
    #[error("could not build the request")]
    Request {
        /// What the HTTP types said.
        #[from]
        source: http::Error,
    },

    /// The transfer failed before or during the response.
    #[error("transfer failed")]
    Transfer {
        /// What the client said.
        #[from]
        source: hyper_util::client::legacy::Error,
    },

    /// The response body failed part way through.
    #[error("the response body failed")]
    Body {
        /// What hyper said.
        #[from]
        source: hyper::Error,
    },

    /// The transfer did not finish in time.
    #[error("timed out after {}s", TRANSFER_TIMEOUT.as_secs())]
    TimedOut,

    /// The directory to keep images in could not be created.
    #[error("could not create {}", dir.display())]
    Directory {
        /// The directory asked for.
        dir: PathBuf,
        /// What the filesystem said.
        source: std::io::Error,
    },

    /// The file could not be written.
    #[error("could not write the download")]
    Io {
        /// What the filesystem said.
        #[from]
        source: std::io::Error,
    },
}

/// A file that was downloaded and is now on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stored {
    /// Where it landed.
    pub path: PathBuf,
    /// How many octets were written.
    pub bytes: u64,
    /// SHA-256 of the contents, hex, so it can be compared with an image already held.
    pub sha256: String,
}

/// What to do about firmware a vendor's cloud advertises.
///
/// The vendor is always present, because *noticing* an advertisement costs nothing and is worth doing
/// wherever one can arrive — which is any deployment relaying to the cloud, and no other. Keeping an image
/// is optional, and downloading is a further opt-in inside that: an advertisement is observed traffic,
/// whereas fetching reaches out to a vendor host.
#[derive(Debug)]
pub struct FirmwareStore<V: Vendor> {
    vendor: std::sync::Arc<V>,
    keep: Option<Keep>,
}

// Derived `Clone` would demand `V: Clone`, which a vendor has no reason to be: the handle is an `Arc`, and
// cloning it is what a spawned transfer needs.
impl<V: Vendor> Clone for FirmwareStore<V> {
    fn clone(&self) -> Self {
        Self {
            vendor: std::sync::Arc::clone(&self.vendor),
            keep: self.keep.clone(),
        }
    }
}

/// Where images are kept, once somewhere has been named.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Keep {
    dir: PathBuf,
    fetch: bool,
    max_bytes: u64,
}

impl<V: Vendor> FirmwareStore<V> {
    /// Read advertisements the way `vendor` says, and only log them.
    pub fn new(vendor: std::sync::Arc<V>) -> Self {
        Self { vendor, keep: None }
    }

    /// Also keep images under `dir`, refusing any single transfer over `max_bytes`, downloading only if
    /// `fetch`.
    ///
    /// The directory is created here, not at the first advertisement — which may be an hour away, on a
    /// path nobody is watching by then. It also means an operator can see that the setting took effect
    /// without waiting for a campaign.
    ///
    /// # Errors
    ///
    /// [`FetchError::Directory`] if the directory cannot be created. Later write failures are reported per
    /// transfer instead; this is the one that means nothing would ever be kept.
    pub fn keeping(mut self, dir: PathBuf, fetch: bool, max_bytes: u64) -> Result<Self, FetchError> {
        std::fs::create_dir_all(&dir).map_err(|source| FetchError::Directory {
            dir: dir.clone(),
            source,
        })?;
        self.keep = Some(Keep { dir, fetch, max_bytes });
        Ok(self)
    }

    /// Whether this store downloads, as opposed to logging and keeping nothing.
    pub fn fetches(&self) -> bool {
        self.keep.as_ref().is_some_and(|keep| keep.fetch)
    }

    /// The directory in use, if any.
    pub fn dir(&self) -> Option<&Path> {
        self.keep.as_ref().map(|keep| keep.dir.as_path())
    }

    /// Note something the cloud sent, and act on it if the vendor says it advertises firmware.
    ///
    /// Returns whether it was an advertisement, so a caller can count them without knowing what one looks
    /// like. Every cloud message can be offered: recognising one is the vendor implementation's job, and
    /// the common case is that it is not one. Any transfer runs on its own task, because serving the
    /// device outranks collecting firmware and a slow vendor host must not stall a session.
    pub fn offer(&self, payload: &[u8], refused: bool) -> bool {
        // Parsed and interpreted by the vendor, in that order: what comes back between the two calls is
        // the vendor's own typed value, and this module neither constructs nor inspects it.
        let Some(firmware) = self
            .vendor
            .parse(payload)
            .and_then(|message| self.vendor.advertised_firmware(&message))
        else {
            return false;
        };
        tracing::info!(
            source = firmware.source,
            url = %firmware.url,
            file = firmware.file,
            refused,
            fetch = self.fetches(),
            keeping = self.dir().map(|dir| dir.display().to_string()),
            "the cloud advertised a firmware update"
        );

        if self.fetches() {
            let store = self.clone();
            tokio::spawn(async move {
                match store.fetch(&firmware).await {
                    Ok(Some(stored)) => tracing::info!(
                        file = %stored.path.display(),
                        bytes = stored.bytes,
                        sha256 = %stored.sha256,
                        "fetched the advertised firmware"
                    ),
                    Ok(None) => tracing::info!(
                        url = %firmware.url,
                        "the advertised firmware is already held; not fetching it again"
                    ),
                    Err(error) => tracing::warn!(
                        %error,
                        url = %firmware.url,
                        "could not fetch the advertised firmware"
                    ),
                }
            });
        }
        true
    }

    /// Whether a file of this name is already held. False when nothing is being kept.
    pub async fn holds(&self, name: &str) -> bool {
        match self.dir() {
            Some(dir) => tokio::fs::try_exists(dir.join(name)).await.unwrap_or(false),
            None => false,
        }
    }

    /// Fetch one advertisement, or `Ok(None)` if its file is already held.
    ///
    /// # Errors
    ///
    /// [`FetchError`] for a name that cannot be used, a non-success status, a response over the limit, a
    /// transport failure, or any filesystem error. A failed transfer leaves nothing behind.
    pub async fn fetch(&self, firmware: &AdvertisedFirmware) -> Result<Option<Stored>, FetchError> {
        let keep = self.keep.as_ref().ok_or(FetchError::NothingKept)?;
        // The vendor's path decides a file name, so it is made fit for one before being used as one.
        let name = sanitize_filename::sanitize(&firmware.file);
        if name.is_empty() {
            return Err(FetchError::Io {
                source: std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("{:?} leaves no usable file name", firmware.file),
                ),
            });
        }
        if self.holds(&name).await {
            return Ok(None);
        }

        if firmware.url.scheme() != "http" {
            return Err(FetchError::UnsupportedScheme {
                scheme: firmware.url.scheme().to_owned(),
            });
        }

        // Built per fetch rather than kept: transfers are an hour apart at most, so there is nothing to
        // reuse. The request itself comes from `growatt::firmware`, headers and all, so nothing here can
        // add a header the device would not send -- which a higher-level client would do on its own.
        let client: Client<_, Empty<Bytes>> = Client::builder(TokioExecutor::new()).build_http();
        let request = self.vendor.firmware_request(firmware).body(Empty::new())?;
        let response = match tokio::time::timeout(TRANSFER_TIMEOUT, client.request(request)).await {
            Ok(result) => result?,
            Err(_) => return Err(FetchError::TimedOut),
        };

        let status = response.status();
        if !status.is_success() {
            return Err(FetchError::Status {
                status: status.as_u16(),
            });
        }
        let declared = response
            .headers()
            .get(http::header::CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok());
        if declared.is_some_and(|length| length > keep.max_bytes) {
            return Err(FetchError::TooLarge { limit: keep.max_bytes });
        }

        self.store(response, keep, &name).await
    }

    /// Stream a response to disk under `name`, capped, digested, and renamed only once whole.
    async fn store<B>(&self, response: http::Response<B>, keep: &Keep, name: &str) -> Result<Option<Stored>, FetchError>
    where
        B: hyper::body::Body<Data = Bytes, Error = hyper::Error> + Unpin,
    {
        tokio::fs::create_dir_all(&keep.dir).await?;
        let destination = keep.dir.join(name);
        let partial = destination.with_extension("part");
        let mut file = tokio::fs::File::create(&partial).await?;
        let mut digest = Sha256::new();
        let mut bytes = 0u64;
        let mut body = response.into_body();

        let outcome = loop {
            match body.frame().await {
                Some(Ok(frame)) => {
                    // Trailers carry no data and are simply not interesting here.
                    let Ok(chunk) = frame.into_data() else {
                        continue;
                    };
                    bytes = bytes.saturating_add(chunk.len() as u64);
                    if bytes > keep.max_bytes {
                        break Err(FetchError::TooLarge { limit: keep.max_bytes });
                    }
                    digest.update(&chunk);
                    if let Err(error) = file.write_all(&chunk).await {
                        break Err(error.into());
                    }
                }
                Some(Err(error)) => break Err(error.into()),
                None => break Ok(()),
            }
        };

        if let Err(error) = outcome {
            drop(file);
            // Best effort: the transfer already failed, and a leftover `.part` is the lesser problem.
            tokio::fs::remove_file(&partial).await.ok();
            return Err(error);
        }
        file.sync_all().await?;
        drop(file);
        tokio::fs::rename(&partial, &destination).await?;
        Ok(Some(Stored {
            path: destination,
            bytes,
            sha256: format!("{:x}", digest.finalize()),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::{FetchError, FirmwareStore};
    use crate::vendor::{AdvertisedFirmware, Vendor};
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use url::Url;

    /// A vendor whose protocol is "the payload is a URL", which is all a test needs it to be.
    ///
    /// Deliberately not Growatt's: these tests are about downloading and storing, and using the real
    /// implementation would tie them to a register number and a URL layout that have nothing to do with
    /// what is being checked. That the seam permits this *is* the point of the seam — and its `Message`
    /// being a borrowed `&str` shows the associated type earning its keep.
    #[derive(Debug)]
    struct Fake {
        agent: &'static str,
    }

    impl Vendor for Fake {
        type Message<'a> = &'a str;

        fn parse<'a>(&self, payload: &'a [u8]) -> Option<Self::Message<'a>> {
            std::str::from_utf8(payload).ok()
        }

        fn advertised_firmware(&self, message: &Self::Message<'_>) -> Option<AdvertisedFirmware> {
            Some(AdvertisedFirmware {
                url: Url::parse(message).ok()?,
                file: "WIFI-4.0.2.6.bin".to_owned(),
                source: "the test".to_owned(),
            })
        }

        fn firmware_request(&self, firmware: &AdvertisedFirmware) -> http::request::Builder {
            http::Request::builder()
                .uri(firmware.url.as_str())
                .header(http::header::USER_AGENT, self.agent)
                .header(http::header::CACHE_CONTROL, "no-cache")
        }
    }

    fn store(dir: &std::path::Path, fetch: bool, max_bytes: u64) -> FirmwareStore<Fake> {
        FirmwareStore::new(Arc::new(Fake { agent: "esp-07s" }))
            .keeping(dir.to_path_buf(), fetch, max_bytes)
            .expect("a temporary directory can be created")
    }

    /// A one-shot HTTP server: answers once, and hands back the request head it saw.
    ///
    /// Real sockets rather than a mocked client, because the point is what goes out on the wire — a mock
    /// would assert this program's intentions instead of its behaviour.
    async fn serve_once(response: Vec<u8>) -> (String, tokio::task::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("address").to_string();
        let handle = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            let mut head = Vec::new();
            let mut byte = [0u8; 1];
            while stream.read_exact(&mut byte).await.is_ok() {
                head.push(byte[0]);
                if head.ends_with(b"\r\n\r\n") {
                    break;
                }
            }
            stream.write_all(&response).await.expect("write");
            stream.flush().await.expect("flush");
            String::from_utf8_lossy(&head).into_owned()
        });
        (address, handle)
    }

    fn advertised(address: &str) -> AdvertisedFirmware {
        AdvertisedFirmware {
            url: Url::parse(&format!("http://{address}/x/WIFI/4.0.2.6.bin")).expect("a URL"),
            file: "WIFI-4.0.2.6.bin".to_owned(),
            source: "the test".to_owned(),
        }
    }

    fn ok_response(body: &str) -> Vec<u8> {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/octet-stream\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    }

    /// A private directory per test. Nothing here needs a crate to make one.
    fn tempdir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("heliobridge-firmware-{name}"));
        std::fs::remove_dir_all(&dir).ok();
        dir
    }

    #[tokio::test]
    async fn the_request_is_the_vendor_s_and_carries_nothing_else() {
        let (address, server) = serve_once(ok_response("firmware")).await;
        let dir = tempdir("headers");
        store(&dir, true, 1024)
            .fetch(&advertised(&address))
            .await
            .expect("fetched");

        let head = server.await.expect("server");
        let lower = head.to_ascii_lowercase();
        assert!(lower.contains("user-agent: esp-07s"), "{head}");
        assert!(lower.contains("cache-control: no-cache"), "{head}");
        assert!(lower.contains("host: "), "hyper supplies Host from the URL: {head}");
        assert!(!lower.contains("heliobridge"), "the fetch named this program: {head}");
        // The header set is the vendor's alone: a convenience client would have added these.
        assert!(!lower.contains("accept-encoding"), "{head}");
        assert!(!lower.contains("accept:"), "{head}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn a_fetched_image_is_stored_with_its_digest_and_not_fetched_twice() {
        let (address, server) = serve_once(ok_response("firmware")).await;
        let dir = tempdir("stored");
        let store = store(&dir, true, 1024);
        let advertised = advertised(&address);

        let stored = store.fetch(&advertised).await.expect("fetched").expect("a file");
        server.await.expect("server");
        assert_eq!(stored.bytes, 8);
        assert!(stored.path.ends_with("WIFI-4.0.2.6.bin"), "{}", stored.path.display());
        assert_eq!(stored.sha256.len(), 64, "a hex sha256: {}", stored.sha256);
        assert_eq!(std::fs::read_to_string(&stored.path).expect("readable"), "firmware");

        // Second time nothing is fetched, so it succeeds with no server listening at all.
        assert!(store.fetch(&advertised).await.expect("second").is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn a_declared_length_over_the_limit_is_refused_before_the_body() {
        let (address, server) = serve_once(b"HTTP/1.1 200 OK\r\nContent-Length: 99999\r\n\r\n".to_vec()).await;
        let dir = tempdir("declared");
        let store = store(&dir, true, 16);
        let error = store.fetch(&advertised(&address)).await.expect_err("refused");
        server.await.expect("server");
        assert!(matches!(error, FetchError::TooLarge { limit: 16 }), "{error}");
        assert!(!store.holds("WIFI-4.0.2.6.bin").await, "nothing should be stored");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn a_body_over_the_limit_leaves_nothing_behind() {
        // Chunked, so there is no declared length and the cap has to hold while streaming.
        let (address, server) = serve_once(
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n14\r\n0123456789abcdefghij\r\n0\r\n\r\n".to_vec(),
        )
        .await;
        let dir = tempdir("streamed");
        let store = store(&dir, true, 8);
        let error = store.fetch(&advertised(&address)).await.expect_err("refused");
        server.await.expect("server");
        assert!(matches!(error, FetchError::TooLarge { limit: 8 }), "{error}");
        assert!(!store.holds("WIFI-4.0.2.6.bin").await);
        assert_eq!(
            std::fs::read_dir(&dir).map(Iterator::count).unwrap_or_default(),
            0,
            "a partial file was left behind"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn a_failure_status_is_reported_rather_than_stored() {
        let (address, server) = serve_once(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n".to_vec()).await;
        let dir = tempdir("status");
        let error = store(&dir, true, 1024)
            .fetch(&advertised(&address))
            .await
            .expect_err("refused");
        server.await.expect("server");
        assert!(matches!(error, FetchError::Status { status: 404 }), "{error}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn https_is_refused_rather_than_attempted() {
        // The client is plain HTTP; the advertised channel uses it, and pretending otherwise would fail
        // somewhere less legible than here.
        let dir = tempdir("scheme");
        let advertised = AdvertisedFirmware {
            url: Url::parse("https://cdn.invalid/x/WIFI-1.bin").expect("a URL"),
            file: "WIFI-1.bin".to_owned(),
            source: "the test".to_owned(),
        };
        let error = store(&dir, true, 1024).fetch(&advertised).await.expect_err("refused");
        assert!(matches!(error, FetchError::UnsupportedScheme { .. }), "{error}");
    }

    #[tokio::test]
    async fn a_message_the_vendor_does_not_recognise_is_not_an_advertisement() {
        // `offer` asks the vendor first and reports what it said, so this session can count
        // advertisements without knowing what one looks like.
        let dir = tempdir("unrecognised");
        let store = store(&dir, false, 1024);
        assert!(!store.offer(b"not a url", true));
        assert!(store.offer(b"http://cdn.invalid/x/WIFI-1.bin", true));
        assert!(!store.holds("WIFI-4.0.2.6.bin").await, "logging only, nothing kept");
    }

    #[test]
    fn the_directory_exists_as_soon_as_keeping_is_switched_on() {
        // Not at the first advertisement: a campaign arrives about once an hour, and an operator who
        // enabled the setting should see the directory now rather than then.
        let dir = tempdir("created-eagerly").join("nested");
        let kept = store(&dir, true, 1024);
        assert!(dir.is_dir(), "{} was not created", dir.display());
        assert_eq!(kept.dir(), Some(dir.as_path()));
        // Switching it on again over an existing directory is not an error.
        assert_eq!(store(&dir, true, 1024).dir(), Some(dir.as_path()));
    }

    #[test]
    fn a_directory_that_cannot_be_created_is_refused() {
        // A file where the directory should be: the failure an operator most plausibly arranges, and one
        // worth a startup message rather than silence.
        let dir = tempdir("occupied");
        std::fs::create_dir_all(dir.parent().expect("a parent")).expect("the parent exists");
        std::fs::write(&dir, b"in the way").expect("the file is written");
        let error = FirmwareStore::new(Arc::new(Fake { agent: "esp-07s" }))
            .keeping(dir.clone(), true, 1024)
            .expect_err("refused");
        assert!(matches!(error, FetchError::Directory { .. }), "{error}");
        std::fs::remove_file(&dir).ok();
    }

    #[test]
    fn a_store_that_does_not_fetch_says_so() {
        assert!(!store(&tempdir("no-fetch"), false, 1024).fetches());
        // A store with nowhere to keep anything still notices advertisements; that is the default.
        let logging_only = FirmwareStore::new(Arc::new(Fake { agent: "esp-07s" }));
        assert!(!logging_only.fetches());
        assert!(logging_only.dir().is_none());
    }
}
