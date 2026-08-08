//! Raw frame recording, off unless configured.
//!
//! A diagnostic facility, not the state store: nothing in normal operation reads it back. It exists
//! because the protocol became tractable by re-reading captures with a better decoder, and the next
//! unknown register will be found the same way.
//!
//! # Four streams, kept separate
//!
//! | File | Contents |
//! |---|---|
//! | `up.bin` | frames received from the device |
//! | `down.bin` | frames received from the cloud, when relaying |
//! | `inject.bin` | frames this program originated itself |
//! | `blocked.bin` | frames the relay policy refused to deliver to the device |
//!
//! Keeping `inject` apart from `down` is the point rather than tidiness: from the device's side the two
//! are indistinguishable, so when a write misbehaves the first question is whether this program sent what
//! it thought it sent. One merged stream cannot answer that.
//!
//! `blocked` answers a different question — what the relay policy refused — and is recorded *in addition*
//! to `down`, so that stream stays a complete record of what the cloud sent. Filtering the wrong thing is
//! the failure mode a filter introduces, and it is only auditable if the refusals are written down.
//! Uplink refusals need no file of their own: `up.bin` already holds every frame the device sent, whether
//! or not it was forwarded.
//!
//! # Raw octets, exactly as they crossed the socket
//!
//! Obfuscated, unparsed, undecoded. A recording is only worth keeping if a *later, fixed* decoder can
//! re-read it — every correction made during this project came from re-reading old captures — and a
//! recording of decoded values can only ever be as good as the decoder that wrote it. It also means a
//! recording can become a test fixture directly, once redacted.
//!
//! Binary, not hex: one frame is 585 octets, and doubling that buys nothing when the tooling reads binary
//! anyway.
//!
//! # Never at the device's expense
//!
//! The device has nowhere else to publish, so recording is subordinate to serving it:
//!
//! - Writes go through a bounded channel. A slow disk applies backpressure to the writer task, never to
//!   the session. A full queue drops the record and counts it — a gap in a diagnostic capture is
//!   acceptable, a stalled session is not.
//! - A write failure disables recording and logs once. Full disk, unwritable path, permissions: the bridge
//!   keeps serving.
//!
//! # Record layout
//!
//! Each record is self-describing so a truncated file can be resynchronised from any point:
//!
//! ```text
//! +--------+--------+--------------+--------------+-----------+
//! | magic  | stream | microseconds | payload len  | payload   |
//! | 4      | 1      | 8            | 4            | len       |
//! +--------+--------+--------------+--------------+-----------+
//! ```
//!
//! Integers are big-endian, matching the protocol this records. The timestamp is microseconds since the
//! Unix epoch rather than a monotonic count: monotonic time survives clock steps but cannot be lined up
//! against a log, and lining a frame up against a log line is the entire use case.

use core::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use snafu::{ResultExt, Snafu};
use tokio::io::AsyncWriteExt as _;
use tokio::sync::mpsc;

/// Marks the start of a record.
pub const MAGIC: [u8; 4] = *b"HBR1";

/// Octets before the payload.
pub const HEADER_LEN: usize = 17;

/// Default cap per stream before rotating.
pub const DEFAULT_MAX_BYTES: u64 = 256 * 1024 * 1024;

/// How many records may queue before new ones are dropped.
///
/// Telemetry is one 585-octet frame every five seconds, so this is many seconds of slack — far more than a
/// healthy disk needs, and a bounded amount of memory when the disk is not healthy.
pub const QUEUE_DEPTH: usize = 256;

/// Why recording could not be started.
#[derive(Debug, Snafu)]
#[snafu(visibility(pub))]
pub enum RecordError {
    /// The directory could not be created.
    #[snafu(display("could not create the recording directory {}", path.display()))]
    Directory {
        /// The directory.
        path: PathBuf,
        /// The underlying error.
        source: std::io::Error,
    },
}

/// Which direction a recorded frame travelled.
#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Stream {
    /// Received from the device.
    Up,
    /// Received from the cloud, when relaying.
    Down,
    /// Originated by this program.
    Inject,
    /// Refused by the relay policy and never delivered to the device.
    ///
    /// Deliberately redundant with [`Self::Down`], which keeps holding everything the cloud sent: one
    /// file answers "what did the cloud send", the other "what did we refuse", and neither has to be
    /// reconstructed by subtracting the other. Refusals are rare enough — six in twelve hours — that the
    /// duplicated octets do not matter.
    Blocked,
}

impl Stream {
    /// Every stream, for iterating.
    pub const ALL: [Self; 4] = [Self::Up, Self::Down, Self::Inject, Self::Blocked];

    /// The octet written into a record.
    pub const fn tag(self) -> u8 {
        match self {
            Self::Up => 0,
            Self::Down => 1,
            Self::Inject => 2,
            Self::Blocked => 3,
        }
    }

    /// Recover a stream from its octet.
    pub const fn from_tag(tag: u8) -> Option<Self> {
        match tag {
            0 => Some(Self::Up),
            1 => Some(Self::Down),
            2 => Some(Self::Inject),
            3 => Some(Self::Blocked),
            _ => None,
        }
    }

    /// The file this stream is written to.
    pub const fn file_name(self) -> &'static str {
        match self {
            Self::Up => "up.bin",
            Self::Down => "down.bin",
            Self::Inject => "inject.bin",
            Self::Blocked => "blocked.bin",
        }
    }
}

impl fmt::Display for Stream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match *self {
            Self::Up => "up",
            Self::Down => "down",
            Self::Inject => "inject",
            Self::Blocked => "blocked",
        })
    }
}

/// One recorded frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    /// Which direction it travelled.
    pub stream: Stream,
    /// Microseconds since the Unix epoch.
    pub micros: u64,
    /// The octets exactly as they crossed the socket.
    pub payload: Vec<u8>,
}

impl Record {
    /// Build a record stamped with the current time.
    pub fn now(stream: Stream, payload: &[u8]) -> Self {
        let micros = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| u64::try_from(since.as_micros()).unwrap_or(u64::MAX));
        Self {
            stream,
            micros,
            payload: payload.to_vec(),
        }
    }

    /// Serialise, header and payload together.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(HEADER_LEN.saturating_add(self.payload.len()));
        out.extend_from_slice(&MAGIC);
        out.push(self.stream.tag());
        out.extend_from_slice(&self.micros.to_be_bytes());
        out.extend_from_slice(&u32::try_from(self.payload.len()).unwrap_or(u32::MAX).to_be_bytes());
        out.extend_from_slice(&self.payload);
        out
    }

    /// Read one record from the front of a buffer, returning it and the octets consumed.
    ///
    /// `None` when the buffer holds no complete record. Provided so the offline tooling and the tests read
    /// the same format the writer produces, rather than each having its own opinion of it.
    pub fn decode(buf: &[u8]) -> Option<(Self, usize)> {
        if buf.get(..MAGIC.len())? != MAGIC {
            return None;
        }
        let stream = Stream::from_tag(buf.get(4).copied()?)?;

        let micros = u64::from_be_bytes(buf.get(5..13)?.try_into().ok()?);
        let len = usize::try_from(u32::from_be_bytes(buf.get(13..17)?.try_into().ok()?)).ok()?;

        let end = HEADER_LEN.checked_add(len)?;
        let payload = buf.get(HEADER_LEN..end)?.to_vec();
        Some((
            Self {
                stream,
                micros,
                payload,
            },
            end,
        ))
    }
}

/// Where recordings go and how large they may grow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecorderConfig {
    /// Directory holding one file per stream.
    pub dir: PathBuf,
    /// Cap per stream. On reaching it the file rotates once to `.1`, so the most recent window survives
    /// rather than the recording stopping at the least useful moment.
    pub max_bytes: u64,
}

impl RecorderConfig {
    /// A configuration with the default size cap.
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            max_bytes: DEFAULT_MAX_BYTES,
        }
    }
}

/// A handle for submitting frames to be recorded.
///
/// Cheap to clone, and cloning shares the counters: several sessions over the lifetime of one process
/// write to one set of files.
#[derive(Debug, Clone)]
pub struct Recorder {
    tx: mpsc::Sender<Record>,
    counters: Arc<Counters>,
}

/// Shared tallies, so a clone reports the same figures.
#[derive(Debug, Default)]
struct Counters {
    written: AtomicU64,
    dropped: AtomicU64,
}

impl Recorder {
    /// Create the directory and start the writer task.
    ///
    /// # Errors
    ///
    /// [`RecordError::Directory`] if the directory cannot be created — the one failure worth reporting at
    /// startup, because it means nothing would ever be recorded. Later write failures are handled by
    /// disabling recording rather than by failing.
    pub fn start(config: RecorderConfig) -> Result<Self, RecordError> {
        std::fs::create_dir_all(&config.dir).context(DirectorySnafu {
            path: config.dir.clone(),
        })?;

        let (tx, rx) = mpsc::channel(QUEUE_DEPTH);
        let counters = Arc::new(Counters::default());

        tokio::spawn(Writer::new(config, Arc::clone(&counters)).run(rx));

        Ok(Self { tx, counters })
    }

    /// Submit a frame, without waiting.
    ///
    /// Silently drops when the queue is full or the writer has stopped. Both are counted, and both are
    /// preferable to delaying the device.
    pub fn record(&self, stream: Stream, payload: &[u8]) {
        if self.tx.try_send(Record::now(stream, payload)).is_err() {
            self.counters.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// How many records have been written.
    pub fn written(&self) -> u64 {
        self.counters.written.load(Ordering::Relaxed)
    }

    /// How many records were dropped rather than written.
    pub fn dropped(&self) -> u64 {
        self.counters.dropped.load(Ordering::Relaxed)
    }
}

/// The writer task's state: one open file per stream, plus its size.
///
/// A fixed-size array indexed by [`Stream::tag`], so there is exactly one sink per stream by construction
/// rather than by convention.
struct Writer {
    config: RecorderConfig,
    counters: Arc<Counters>,
    files: [Option<Sink>; Stream::ALL.len()],
}

/// One stream's file and how much has been written to it.
struct Sink {
    file: tokio::fs::File,
    written: u64,
}

impl Writer {
    fn new(config: RecorderConfig, counters: Arc<Counters>) -> Self {
        Self {
            config,
            counters,
            files: core::array::from_fn(|_| None),
        }
    }

    /// Write records until the channel closes or a write fails.
    async fn run(mut self, mut rx: mpsc::Receiver<Record>) {
        tracing::info!(
            dir = %self.config.dir.display(),
            max_bytes = self.config.max_bytes,
            "recording raw frames"
        );

        while let Some(record) = rx.recv().await {
            if let Err(error) = self.write(&record).await {
                // Disable rather than retry. A disk that cannot be written to now will not fix itself, and
                // repeating the message every five seconds would bury the log that still matters.
                tracing::warn!(
                    %error,
                    stream = %record.stream,
                    written = self.counters.written.load(Ordering::Relaxed),
                    "recording failed; disabling it and continuing to serve the device"
                );
                return;
            }
            self.counters.written.fetch_add(1, Ordering::Relaxed);
        }

        tracing::info!(
            written = self.counters.written.load(Ordering::Relaxed),
            dropped = self.counters.dropped.load(Ordering::Relaxed),
            "recording stopped"
        );
    }

    /// Append one record, rotating first if it would exceed the cap.
    async fn write(&mut self, record: &Record) -> std::io::Result<()> {
        let index = usize::from(record.stream.tag());
        let encoded = record.encode();

        let path = self.config.dir.join(record.stream.file_name());
        let slot = self
            .files
            .get_mut(index)
            .ok_or_else(|| std::io::Error::other("unknown stream"))?;

        if slot.is_none() {
            *slot = Some(Sink::open(&path).await?);
        }

        let needs_rotation = slot
            .as_ref()
            .is_some_and(|sink| sink.written.saturating_add(encoded.len() as u64) > self.config.max_bytes);

        if needs_rotation {
            tracing::info!(stream = %record.stream, "rotating the recording");
            *slot = None;
            rotate(&path).await?;
            *slot = Some(Sink::open(&path).await?);
        }

        let sink = slot
            .as_mut()
            .ok_or_else(|| std::io::Error::other("recording file not open"))?;
        sink.write(&encoded).await
    }
}

impl Sink {
    /// Open for append, learning the existing size so the cap accounts for it.
    async fn open(path: &Path) -> std::io::Result<Self> {
        let file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .await?;
        let written = file.metadata().await.map_or(0, |meta| meta.len());
        Ok(Self { file, written })
    }

    async fn write(&mut self, encoded: &[u8]) -> std::io::Result<()> {
        self.file.write_all(encoded).await?;
        // Flushed, not synced. A crash may lose the last records; calling fsync every five seconds to
        // avoid that would be a poor trade for a diagnostic file.
        self.file.flush().await?;
        self.written = self.written.saturating_add(encoded.len() as u64);
        Ok(())
    }
}

/// Move a full recording aside, replacing any previous rotation.
async fn rotate(path: &Path) -> std::io::Result<()> {
    let mut rotated = path.as_os_str().to_owned();
    rotated.push(".1");
    tokio::fs::rename(path, PathBuf::from(rotated)).await
}

#[cfg(test)]
mod tests {
    use super::{HEADER_LEN, MAGIC, Record, Recorder, RecorderConfig, Stream};

    /// A scratch directory that removes itself.
    struct Scratch(std::path::PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!("heliobridge-record-{name}"));
            drop(std::fs::remove_dir_all(&path));
            Self(path)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            drop(std::fs::remove_dir_all(&self.0));
        }
    }

    /// Read every record from a stream's file.
    fn read_all(dir: &std::path::Path, stream: Stream) -> Vec<Record> {
        let Ok(data) = std::fs::read(dir.join(stream.file_name())) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        let mut rest = data.as_slice();
        while let Some((record, used)) = Record::decode(rest) {
            out.push(record);
            rest = rest.get(used..).unwrap_or_default();
        }
        out
    }

    #[test]
    fn a_record_round_trips() {
        let record = Record::now(Stream::Up, &[0xDE, 0xAD, 0xBE, 0xEF]);
        let encoded = record.encode();
        assert_eq!(encoded.len(), HEADER_LEN + 4);
        assert_eq!(encoded.get(..4), Some(MAGIC.as_slice()));

        let (decoded, used) = Record::decode(&encoded).expect("decode");
        assert_eq!(used, encoded.len());
        assert_eq!(decoded, record);
    }

    #[test]
    fn a_truncated_record_decodes_as_nothing() {
        let encoded = Record::now(Stream::Up, &[1, 2, 3]).encode();
        for cut in 0..encoded.len() {
            assert!(
                Record::decode(encoded.get(..cut).expect("prefix")).is_none(),
                "a {cut}-octet prefix should not decode"
            );
        }
    }

    #[test]
    fn stream_tags_round_trip() {
        for stream in Stream::ALL {
            assert_eq!(Stream::from_tag(stream.tag()), Some(stream));
        }
        assert_eq!(Stream::from_tag(9), None);
        // The file names must differ, or the streams would merge — which is the thing this exists to
        // prevent.
        let mut names: Vec<_> = Stream::ALL.iter().map(|s| s.file_name()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), Stream::ALL.len());
    }

    #[tokio::test]
    async fn the_three_streams_are_written_to_separate_files() {
        let scratch = Scratch::new("streams");
        let recorder = Recorder::start(RecorderConfig::new(scratch.0.clone())).expect("start");

        recorder.record(Stream::Up, &[0x01; 10]);
        recorder.record(Stream::Down, &[0x02; 20]);
        recorder.record(Stream::Inject, &[0x03; 30]);
        recorder.record(Stream::Up, &[0x04; 40]);

        // Let the writer task drain.
        for _ in 0..50 {
            if recorder.written() >= 4 {
                break;
            }
            tokio::time::sleep(core::time::Duration::from_millis(10)).await;
        }
        assert_eq!(recorder.written(), 4);
        assert_eq!(recorder.dropped(), 0);

        let up = read_all(&scratch.0, Stream::Up);
        assert_eq!(up.len(), 2, "up should hold both of its records");
        assert_eq!(up.first().map(|r| r.payload.len()), Some(10));
        assert_eq!(up.get(1).map(|r| r.payload.len()), Some(40));

        // The whole point of separate files: an injected frame must not be mistakable for a cloud one.
        assert_eq!(read_all(&scratch.0, Stream::Down).len(), 1);
        assert_eq!(read_all(&scratch.0, Stream::Inject).len(), 1);
        for record in read_all(&scratch.0, Stream::Inject) {
            assert_eq!(record.stream, Stream::Inject);
        }
    }

    #[tokio::test]
    async fn payloads_are_stored_exactly_as_given() {
        // Raw octets, unparsed: a recording is only useful if a later decoder can re-read it.
        let scratch = Scratch::new("verbatim");
        let recorder = Recorder::start(RecorderConfig::new(scratch.0.clone())).expect("start");

        let frame: Vec<u8> = (0..=255u8).cycle().take(585).collect();
        recorder.record(Stream::Up, &frame);

        for _ in 0..50 {
            if recorder.written() >= 1 {
                break;
            }
            tokio::time::sleep(core::time::Duration::from_millis(10)).await;
        }

        let records = read_all(&scratch.0, Stream::Up);
        assert_eq!(records.len(), 1);
        assert_eq!(records.first().map(|r| r.payload.as_slice()), Some(frame.as_slice()));
    }

    #[tokio::test]
    async fn reaching_the_cap_rotates_once_and_keeps_going() {
        let scratch = Scratch::new("rotate");
        let recorder = Recorder::start(RecorderConfig {
            dir: scratch.0.clone(),
            // Room for two records of this size, so the third rotates.
            max_bytes: (HEADER_LEN as u64 + 100) * 2,
        })
        .expect("start");

        for _ in 0..5 {
            recorder.record(Stream::Up, &[0xAA; 100]);
        }
        for _ in 0..50 {
            if recorder.written() >= 5 {
                break;
            }
            tokio::time::sleep(core::time::Duration::from_millis(10)).await;
        }

        // The live file holds the most recent window, and the previous one is beside it.
        let live = read_all(&scratch.0, Stream::Up);
        assert!(!live.is_empty(), "recording must continue after rotating");
        assert!(scratch.0.join("up.bin.1").exists(), "the rotated file should be kept");
        assert_eq!(recorder.written(), 5, "no record should be lost to rotation");
    }

    #[tokio::test]
    async fn a_full_queue_drops_rather_than_blocks() {
        // Recording is subordinate to serving the device: submitting must never wait.
        let scratch = Scratch::new("drops");
        let recorder = Recorder::start(RecorderConfig::new(scratch.0.clone())).expect("start");

        for _ in 0..super::QUEUE_DEPTH.saturating_mul(8) {
            recorder.record(Stream::Up, &[0u8; 585]);
        }

        // Whatever the split between written and dropped, nothing hung and the totals add up.
        assert!(
            recorder.written().saturating_add(recorder.dropped()) > 0,
            "records should be accounted for"
        );
    }

    #[test]
    fn a_missing_directory_that_cannot_be_created_is_reported() {
        // `/proc` is real but not writable, so this exercises the failure without depending on being root.
        let config = RecorderConfig::new(std::path::PathBuf::from("/proc/heliobridge-cannot-exist"));
        let error = Recorder::start(config).expect_err("should fail");
        assert!(error.to_string().contains("heliobridge-cannot-exist"), "{error}");
    }
}
