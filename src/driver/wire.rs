//! Octets in, a driver's own frame out.

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
    type Frame<'a>;

    /// Read octets as a frame, or `None` if they are not one.
    ///
    /// Octets are the only input the server can honestly offer: every transport delivers bytes, and
    /// anything more structured would be a shape borrowed from one protocol. A payload that does not parse
    /// is not an error — this program stands between a device and a cloud it does not control, and a
    /// message it cannot read is a fact about the world rather than a fault.
    fn parse<'a>(&self, payload: &'a [u8]) -> Option<Self::Frame<'a>>;
}
