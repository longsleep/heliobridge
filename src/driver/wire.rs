//! Octets in, a driver's own frame out.

use core::fmt;

/// Why a payload is not a frame.
///
/// Three answers rather than one, because a server logs them differently and an operator reads them
/// differently: an unsupported generation is this program lagging the device, a short payload is usually
/// something else entirely on the topic, and a malformed frame is the interesting case worth a hex dump.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unreadable {
    /// Too few octets to decide anything.
    TooShort,
    /// A generation, dialect or version this driver does not implement, in its own words.
    Unsupported {
        /// What it says it is.
        generation: String,
    },
    /// The right shape, but it does not hold together — a bad length, a failed integrity check.
    Malformed {
        /// The driver's reason.
        reason: String,
    },
}

impl fmt::Display for Unreadable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooShort => f.write_str("too short to be a frame"),
            Self::Unsupported { generation } => write!(f, "unsupported protocol generation {generation}"),
            Self::Malformed { reason } => f.write_str(reason),
        }
    }
}

/// Reading a manufacturer's framing.
///
/// The first capability every other one rests on: nothing can be asked about a message until something has
/// turned bytes into one, and this is where that happens.
///
/// The supertraits are what being a driver costs rather than what framing costs: one driver is shared,
/// immutably, by every session and by the tasks they spawn. Requiring it here — of the capability every
/// driver has — saves stating it again on each of the others.
pub trait Wire: std::fmt::Debug + Send + Sync + 'static {
    /// A message from the wire, in whatever form this driver's protocol gives it.
    ///
    /// Opaque to the server, which only ever obtains one from [`Self::parse`] and hands it back. That is
    /// what lets an implementation be strongly typed in its own terms — a parsed frame, a decoded
    /// envelope, an enumeration of message kinds — without any of it reaching this side of the seam.
    ///
    /// `Send`, because a server holds one across the awaits of serving a device.
    type Frame<'a>: Send;

    /// Read octets as a frame, saying why not when they are not one.
    ///
    /// Octets are the only input the server can honestly offer: every transport delivers bytes, and
    /// anything more structured would be a shape borrowed from one protocol.
    ///
    /// # Errors
    ///
    /// [`Unreadable`], which is a fact about the world rather than a fault: this program stands between a
    /// device and a cloud it does not control, and both send it things it cannot read.
    fn read<'a>(&self, payload: &'a [u8]) -> Result<Self::Frame<'a>, Unreadable>;

    /// The same, where the reason does not matter.
    fn parse<'a>(&self, payload: &'a [u8]) -> Option<Self::Frame<'a>> {
        self.read(payload).ok()
    }
}
