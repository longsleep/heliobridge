//! What a driver must be able to say for the accessory-enrolment routes to work.
//!
//! The control API resolves names and renders values through the driver rather than knowing any protocol
//! itself, exactly as it does for settings and configuration registers. Enrolling an accessory needs four
//! kinds of knowledge that are all the driver's: what an accessory identifier looks like, which mDNS
//! service and type a named model needs, how to read the accessory list back, and how to read the register
//! a search reports into.
//!
//! Kept separate from [`crate::driver::catalogue::Catalogue`] and composed alongside it, so a driver for a
//! device with no accessories implements nothing and the routes are simply unavailable for it.

/// One entry of a device's accessory list, whatever list it came from.
///
/// Deliberately not the driver's own richer type: what the API needs is a state it can render, an identity
/// it can address and an address it can show. Anything protocol-specific stays behind [`Enrols`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Enrolled {
    /// How the accessory got into the list, which decides what can be done with it.
    ///
    /// A device of this family reaches accessories more than one way and the list holds all of them
    /// together, so an entry that cannot be told apart from another is one a caller cannot act on. The
    /// enrolment routes manage **one** of these kinds; the rest are reported and left alone.
    pub kind: &'static str,
    /// The accessory type, in the device's own numbering. Part of the key that identifies an entry.
    pub accessory: u16,
    /// The mode, which selects the sub-protocol the device uses to reach it. The other half of the key.
    pub mode: u16,
    /// The entry's own second field, as the device spells it: a serial for one it discovered, a name for
    /// one it was given an address for.
    pub name: Option<String>,
    /// The device's own state number, kept as it sends it.
    pub state: u16,
    /// A word for that state, from the driver, because only it knows what the numbers mean.
    pub state_label: &'static str,
    /// Whether the device is actively polling this accessory.
    pub paired: bool,
    /// The accessory's serial, absent for a row that has none.
    pub serial: Option<u64>,
    /// Where the device found it, absent until it has.
    pub address: Option<String>,
}

/// A driver that can enrol an accessory reached over the local network.
pub trait Enrols {
    /// The accessory an identifier names, in whatever notations this protocol's users have.
    ///
    /// A serial here is the protocol's own identifier and not a printed serial number; on this family it is
    /// a MAC, which is why a caller may reasonably write either form.
    fn accessory_serial(&self, text: &str) -> Option<u64>;

    /// Render one as a person reads it, for output beside the protocol's own form.
    fn accessory_mac(&self, serial: u64) -> String;

    /// The mDNS service and accessory type a named model needs, or `None` for a name not known.
    ///
    /// The type is the *device's* index for that model and not the vendor's model code, so a caller cannot
    /// be expected to supply it and this is the lookup that spares them.
    fn accessory_search(&self, model: &str) -> Option<(String, u16)>;

    /// Every model name this driver can resolve, so a refusal can name the alternatives.
    fn accessory_models(&self) -> Vec<&'static str>;

    /// Decode an accessory list register.
    ///
    /// A report, so an entry that does not parse is left out rather than failing the rest.
    fn enrolled(&self, value: &str) -> Vec<Enrolled>;

    /// The serial a search-result register is reporting, if it is reporting one.
    fn accessory_found(&self, value: &str) -> Option<u64>;

    /// The name of the register holding accessories reached over the local network.
    fn accessory_list(&self) -> &'static str;

    /// The name of the register a search reports its result in.
    fn accessory_found_register(&self) -> &'static str;

    /// The name of the register holding accessories reached over the driver's own radio, if it has one.
    fn accessory_list_radio(&self) -> Option<&'static str>;

    /// The telemetry reading that says whether the device is using an accessory's figure.
    ///
    /// The accessory list cannot answer it — an accessory enrolled without being put in service appears in
    /// it identically — so this is the only honest source, and it is a reading rather than a register
    /// because that is what a caller can watch.
    fn accessory_in_use_reading(&self) -> &'static str;

    /// How long a search runs on the device, so a caller streaming its results knows when to stop.
    fn accessory_search_window(&self) -> core::time::Duration;
}
