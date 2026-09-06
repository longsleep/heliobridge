//! Accessories on the **local network**: the grammar of config register 122, and the three commands that
//! enrol one.
//!
//! The counterpart to [`crate::growatt::v7::lora`], and deliberately a separate module for the same reason
//! that one is: a device of this family acquires accessories three unrelated ways, and the shapes do not
//! generalise. An accessory on the radio is *adopted* by a window the server opens and can name nothing. An
//! accessory here is *searched for* by mDNS, chosen from what the search reported, and then polled by
//! address — three stages, with arguments, over two registers.
//!
//! # The three commands
//!
//! All three are written to config register 122 as ASCII, and all three are config writes, which the device
//! does not acknowledge.
//!
//! ```text
//! ADD:111-7-_http._tcp.,2                          browse for this service, expecting this type
//! CRL:111-7-add:sn,<serial>|access,0               pair what the search found
//! DEL:111-7-_http._tcp.,2                          tombstone the entry
//! ```
//!
//! `CRL:` is the **pair** command; despite the name it clears nothing. The device's own entries come back
//! from the same register with a `DEV:` prefix.
//!
//! # Three properties that decide whether a caller gets what it asked for
//!
//! **A search is a precondition.** The pair command on its own does nothing at all — verified against a
//! serial the device was still holding, complete with a working address, in its own tombstoned entry. The
//! serial *selects among what a search reported*; it does not identify an accessory.
//!
//! **`access` decides whether the reading is used.** With `access,0` the accessory becomes the device's
//! meter. Omit the field and it becomes `1`, and the device polls the accessory, gets answers and reports
//! them upstream while its own meter registers stay zero — the reading is taken and not applied. So
//! [`Pairing`] carries it explicitly and there is no way to build the command without saying which.
//!
//! **The entry cannot tell those two apart.** `DEV:111-7-3,<serial>,<address>` is what both produce. What
//! an accessory was enrolled *with* is readable only from its own telemetry
//! ([`crate::growatt::v7::accessory`]), and whether the reading is in use only from the meter registers.
//! Nothing in this module can answer it, and a caller that needs to know must look there.
//!
//! # The leading pair is a constant
//!
//! `111-7` — the accessory type and mode of every network entry and every command above. It was identical
//! across every vendor observed: three manufacturers and seven models, whose only differences were the mDNS
//! service name and the trailing type. Mode 7 is name discovery, which is what `CRL:` applies to. Should a
//! device ever be seen using a different pair, this constant is the assumption that breaks.
//!
//! # The type is the datalogger's index, not the vendor's model code
//!
//! The trailing field of a search names a row in the datalogger's own accessory table, and it is **not** the
//! model code the vendor's application uses; the two agree only where their orderings happen to coincide.
//! [`MODELS`] maps a name a caller can reasonably know onto both fields, and it is partial by nature — what
//! is in it was observed on the wire, and a model that is missing is a lookup failure rather than a guess.

use core::fmt;

/// The accessory type and mode every network entry and command carries.
///
/// See the module documentation: constant across every vendor observed, and the one assumption here that a
/// new device could break.
const PREFIX: &str = "111-7";

/// The mode that means "discovered by name", which is the one this module enrols.
const MODE: u16 = 7;

/// How long the device browses after an `ADD:`.
///
/// Measured rather than documented: the device reports what it has found every ~7 s and then stops, and the
/// bursts run 55 to 63 seconds. Only the *reporting* is bounded by this. The value itself is **unreliable in
/// both directions** — it is not cleared when the reporting stops, nor by a `DEL:`, and has been seen
/// surviving seven minutes and several commands; but after some pairings it does go to zero, with nothing
/// observed deciding which. So a non-zero value is not evidence that anything was just found, and a zero is
/// not evidence that nothing was. Anything treating it as a discovery has to know it arrived inside a
/// window; see the control API.
pub const SEARCH_WINDOW: core::time::Duration = core::time::Duration::from_mins(1);

/// The widest a MAC-derived serial can be, which is what makes one representable as a `u64`.
const SERIAL_BITS: u32 = 48;

/// Models whose mDNS service and accessory type have been observed, by a name a caller can know.
///
/// `(name, mDNS service, accessory type)`. The service is what the device browses for; the type is the
/// datalogger's own table index.
///
/// **Partial, and knowingly so.** These are the rows seen on the wire. Two more indices were read out of
/// firmware without ever being driven, and are left out rather than half-supported: a caller that knows a
/// type this table does not can name it directly.
pub const MODELS: &[(&str, &str, u16)] = &[
    ("shelly-3em", "_http._tcp.", 1),
    ("shelly-pro-3em", "_http._tcp.", 2),
    ("shelly-pro-3em-3ct63", "_http._tcp.", 8),
    ("shelly-pro-em-50", "_http._tcp.", 7),
    ("shelly-pro-1pm", "_http._tcp.", 6),
    ("everhome-ecotracker", "_everhome._tcp.", 3),
    ("sparky-p1", "_chargee_p1._tcp.", 50),
];

/// An accessory's serial as this protocol uses it: its MAC as a plain integer.
///
/// The device reports it in config register 123 and expects it back in the pair command as a **decimal**
/// integer — `187723572702975` is `aa:bb:cc:dd:ee:ff`. Unreadable in that form, which is why this type
/// exists: it parses either notation, renders both, and keeps the conversion in one place instead of at
/// every boundary that has to display one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Serial(u64);

impl Serial {
    /// The serial the device reported, as a decimal integer.
    ///
    /// # Errors
    ///
    /// `None` for a value too wide to be a MAC, which is not a serial this protocol can carry.
    pub const fn new(value: u64) -> Option<Self> {
        if value >> SERIAL_BITS == 0 {
            Some(Self(value))
        } else {
            None
        }
    }

    /// Parse either notation a caller might have.
    ///
    /// A value containing `:` or `-` is a MAC. Otherwise all-decimal is the protocol's own form, and twelve
    /// hex digits are a MAC written without separators. The order matters: `112233445566` is a valid
    /// decimal serial *and* a valid separator-less MAC, and the decimal reading wins because it is what the
    /// device itself reports and what the wire carries.
    ///
    /// # Errors
    ///
    /// `None` if it is neither, or if the value is wider than a MAC.
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        if text.contains(':') || text.contains('-') {
            return Self::from_mac(text);
        }
        if !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit()) {
            return text.parse().ok().and_then(Self::new);
        }
        Self::from_mac(text)
    }

    /// Parse a MAC, with or without separators.
    fn from_mac(text: &str) -> Option<Self> {
        let digits: String = text
            .chars()
            .filter(|character| *character != ':' && *character != '-')
            .collect();
        if digits.len() != 12 || !digits.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return None;
        }
        u64::from_str_radix(&digits, 16).ok().and_then(Self::new)
    }

    /// The decimal the wire carries.
    pub const fn get(self) -> u64 {
        self.0
    }

    /// The same value as a MAC, for anything a person reads.
    pub fn mac(self) -> String {
        let octets = self.0.to_be_bytes();
        let mac: Vec<String> = octets[2..].iter().map(|octet| format!("{octet:02x}")).collect();
        mac.join(":")
    }
}

impl fmt::Display for Serial {
    /// The decimal form, because that is what the protocol means by the serial.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.0)
    }
}

/// What to look for: an mDNS service, and the accessory type the device should expect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Search {
    /// The mDNS service to browse, as the device wants it — trailing dot included.
    pub service: String,
    /// The datalogger's own accessory-table index.
    pub accessory: u16,
}

impl Search {
    /// The search a named model needs, from [`MODELS`].
    ///
    /// # Errors
    ///
    /// `None` for a name the table does not carry, which a caller should report as such rather than
    /// substitute for.
    pub fn of_model(model: &str) -> Option<Self> {
        let wanted = model.trim().to_ascii_lowercase();
        MODELS
            .iter()
            .find(|(name, _, _)| *name == wanted)
            .map(|(_, service, accessory)| Self {
                service: (*service).to_owned(),
                accessory: *accessory,
            })
    }

    /// Every model name this build knows, for an error message that can be acted on.
    pub fn models() -> Vec<&'static str> {
        MODELS.iter().map(|(name, _, _)| *name).collect()
    }

    /// The command that starts the browse.
    ///
    /// ⚠ This is what **clears** an existing entry's serial and address: a tombstone holding both goes back
    /// to `DEV:111-7-0,,`. What it does to a *paired* entry has never been observed, so a caller that has
    /// one should delete it deliberately first rather than find out.
    pub fn start(&self) -> String {
        format!("ADD:{PREFIX}-{},{}", self.service, self.accessory)
    }
}

/// The payload a delete carries, which the device does not read.
///
/// Well-formed rather than empty, because a payload that fails to parse is a different experiment from one
/// that is ignored, and only the second has been run.
const FORGET_PAYLOAD: &str = "_http._tcp.,0";

/// Tombstone one discovered accessory, by the number the device gave it.
///
/// **The number selects and the payload is ignored.** Measured against a device holding two mode-7 entries:
/// a delete naming `112` tombstoned `112` and left `111` paired; naming `111` then tombstoned `111`. The
/// payload was a service never browsed and an index never requested in both cases, and made no difference —
/// so a caller supplies the number from [`Entry::number`] and nothing else.
///
/// A delete that names no particular entry is therefore not expressible, which is why this takes an
/// argument at all. With a single entry any number that happened to match it worked, and that is how this
/// was first, wrongly, read as taking nothing.
pub fn forget(entry: u16) -> String {
    format!("DEL:{entry}-{MODE}-{FORGET_PAYLOAD}")
}

/// The pair command: which accessory, and whether the device should use what it reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pairing {
    /// The serial the device reported in register 123.
    pub serial: Serial,
    /// Whether the reading is used. `0` puts the accessory in service; see [`Self::IN_SERVICE`].
    pub access: u16,
}

impl Pairing {
    /// The `access` value that makes an accessory the device's meter.
    ///
    /// The only value the vendor's own server has been seen to send. Omitting the field yields `1`, with
    /// which the device polls the accessory and ignores what it reads, so this is not a default anything
    /// should arrive at by accident.
    pub const IN_SERVICE: u16 = 0;

    /// Pair an accessory and put it in service.
    pub const fn in_service(serial: Serial) -> Self {
        Self {
            serial,
            access: Self::IN_SERVICE,
        }
    }

    /// The command.
    pub fn command(&self) -> String {
        format!("CRL:{PREFIX}-add:sn,{}|access,{}", self.serial, self.access)
    }

    /// Whether this pairing puts the accessory's reading to use.
    pub const fn in_use(&self) -> bool {
        self.access == Self::IN_SERVICE
    }
}

/// What the device does with an entry, as its own state number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// `0` — registered by a search and not paired. The dialler skips it, so nothing is ever read from it.
    Registered,
    /// `3` — paired, and being polled.
    Paired,
    /// `5` — deleted. A tombstone rather than a removal: the row stays, and one that was paired keeps its
    /// serial and address until the next search clears them.
    Deleted,
    /// A state this build has not seen. Kept rather than rejected so an entry still decodes.
    Other(u16),
}

impl State {
    /// The device's own number.
    const fn of(value: u16) -> Self {
        match value {
            0 => Self::Registered,
            3 => Self::Paired,
            5 => Self::Deleted,
            other => Self::Other(other),
        }
    }

    /// The number, for anything that wants the device's own value back.
    pub const fn code(self) -> u16 {
        match self {
            Self::Registered => 0,
            Self::Paired => 3,
            Self::Deleted => 5,
            Self::Other(other) => other,
        }
    }

    /// A word for it.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Registered => "registered",
            Self::Paired => "paired",
            Self::Deleted => "deleted",
            Self::Other(_) => "unknown",
        }
    }

    /// Whether the device is polling this accessory.
    pub const fn is_paired(self) -> bool {
        matches!(self, Self::Paired)
    }
}

/// One entry as the device reports it in register 122.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The device's own number for this record, which is what a delete names.
    ///
    /// **Assigned by the device, not by the server.** Every discovery write sends `111`; the device puts
    /// the entry wherever it likes, and a second one registered while the first was live came back as
    /// `112`. So it can be read back and used, but never predicted.
    pub number: u16,
    /// The mode — `7`, name discovery, for everything this module builds.
    pub mode: u16,
    /// What the device is doing with it.
    pub state: State,
    /// The entry's second field as the device spells it — a serial for a discovered accessory, a name for
    /// one reached by address. Kept raw as well as parsed, because only one of those two is a serial.
    pub name: Option<String>,
    /// The accessory's serial, absent for a row whose second field is not one.
    pub serial: Option<Serial>,
    /// Where the device found it, taken from the mDNS SRV record. Absent until it has been.
    pub address: Option<String>,
}

impl Entry {
    /// How this accessory is reached, from its mode.
    ///
    /// The mode selects the sub-protocol, and only two exist. `7` is the one this module enrols: the device
    /// browses mDNS, identifies what answers over HTTP, and polls it. `1` is an outbound TCP client to an
    /// address the server supplies, and **several of those coexist** — two were held at once in an earlier
    /// experiment.
    ///
    /// The two are deleted differently, which is the practical consequence. A mode-1 entry is selected by
    /// its *payload*: two were removed one at a time by naming each one's address and name, and the leading
    /// field played no part — both deletes named type `2` while one of the entries was reading `3`. A
    /// mode-7 delete reads no payload at all. Only ever one mode-7 entry has been seen, so what a delete
    /// would do with two is unknown.
    pub const fn kind(&self) -> &'static str {
        match self.mode {
            1 => "dialled",
            7 => "discovered",
            _ => "unknown",
        }
    }

    /// Decode the register's value.
    ///
    /// `DEV:` alone is an empty list and yields nothing, which is what an untouched device and the radio's
    /// own register both report. Entries are separated by `&`. A row that does not parse is skipped rather
    /// than failing the whole list: this is a report, and one unfamiliar row should not hide the others.
    ///
    /// Trailing NULs are expected — the register is a fixed-width string — and trimmed.
    pub fn parse_list(value: &str) -> Vec<Self> {
        let body = value.trim_matches(|character: char| character == '\0' || character.is_whitespace());
        let Some(body) = body.strip_prefix("DEV:") else {
            return Vec::new();
        };
        body.split('&').filter_map(Self::parse_one).collect()
    }

    /// One `<type>-<mode>-<state>,<serial>,<address>` row.
    fn parse_one(row: &str) -> Option<Self> {
        let row = row.trim_matches(|character: char| character == '\0' || character.is_whitespace());
        if row.is_empty() {
            return None;
        }
        let mut fields = row.splitn(3, ',');
        let head = fields.next()?;
        let serial = fields.next().unwrap_or_default();
        let address = fields.next().unwrap_or_default();

        // The head is three dash-separated numbers, and the state is the last of them. Split from the left
        // so a type that grows a dash would fail to parse rather than silently read as something else.
        let mut parts = head.split('-');
        let number = parts.next()?.parse().ok()?;
        let mode = parts.next()?.parse().ok()?;
        let state = parts.next()?.parse().ok()?;
        if parts.next().is_some() {
            return None;
        }

        Some(Self {
            number,
            mode,
            state: State::of(state),
            name: Some(serial.trim().to_owned()).filter(|text| !text.is_empty()),
            serial: Serial::parse(serial),
            address: Some(address.trim().to_owned()).filter(|text| !text.is_empty()),
        })
    }
}

/// The serial register 123 is reporting, if it is reporting one.
///
/// `0` is the register's way of saying nothing was found, and is not a serial. Anything else is a decimal
/// MAC. A server **must** read this as an integer: it is not the accessory's printed serial number.
pub fn found(value: &str) -> Option<Serial> {
    let value = value.trim_matches(|character: char| character == '\0' || character.is_whitespace());
    Serial::parse(value).filter(|serial| serial.get() != 0)
}

#[cfg(test)]
mod tests {
    use super::{Entry, MODELS, Pairing, Search, Serial, State, found};

    /// A placeholder MAC and the decimal the protocol carries for it.
    const MAC: &str = "aa:bb:cc:dd:ee:ff";
    const DECIMAL: u64 = 187_723_572_702_975;

    #[test]
    fn a_serial_is_a_mac_and_reads_in_either_notation() {
        let expected = Serial::new(DECIMAL).expect("48 bits");
        assert_eq!(Serial::parse(MAC), Some(expected));
        assert_eq!(Serial::parse("aabbccddeeff"), Some(expected));
        assert_eq!(Serial::parse("aa-bb-cc-dd-ee-ff"), Some(expected));
        assert_eq!(Serial::parse("187723572702975"), Some(expected));
        // Both renderings, because the wire wants one and a person wants the other.
        assert_eq!(expected.to_string(), "187723572702975");
        assert_eq!(expected.mac(), MAC);
    }

    #[test]
    fn a_digits_only_serial_is_read_as_the_decimal_the_device_reports() {
        // The one ambiguous shape: twelve digits are a valid decimal serial and a valid separator-less MAC.
        // The decimal wins, because it is what the device reports and what the command carries.
        let both = "112233445566";
        assert_eq!(Serial::parse(both).map(Serial::get), Some(112_233_445_566));
    }

    #[test]
    fn a_value_too_wide_for_a_mac_is_not_a_serial() {
        assert_eq!(Serial::new(1 << 48), None);
        assert_eq!(Serial::parse("281474976710656"), None);
        assert_eq!(Serial::parse(""), None);
        assert_eq!(Serial::parse("not-a-serial"), None);
    }

    #[test]
    fn a_delete_names_the_entry_and_the_device_ignores_the_rest() {
        // Measured against a device holding two mode-7 entries: naming 112 tombstoned 112 and left 111
        // paired, then naming 111 tombstoned 111. The payload was a service never browsed in both cases.
        assert_eq!(super::forget(112), "DEL:112-7-_http._tcp.,0");
        assert_eq!(super::forget(111), "DEL:111-7-_http._tcp.,0");
        assert_ne!(
            super::forget(111),
            super::forget(112),
            "the number is the whole selector"
        );
    }

    #[test]
    fn the_search_commands_are_the_ones_observed_on_the_wire() {
        let search = Search::of_model("shelly-pro-3em").expect("a known model");
        assert_eq!(search.service, "_http._tcp.");
        assert_eq!(search.accessory, 2);
        assert_eq!(search.start(), "ADD:111-7-_http._tcp.,2");
        // A search always sends 111. Which entry the device puts it in is the device's business, and a
        // delete has to name what it chose — see `forget`.
        assert!(search.start().starts_with("ADD:111-7-"));
    }

    #[test]
    fn every_model_in_the_table_resolves_and_the_names_are_unique() {
        for (name, service, accessory) in MODELS {
            let search = Search::of_model(name).expect("its own name resolves");
            assert_eq!(search.service, *service);
            assert_eq!(search.accessory, *accessory);
        }
        let mut names = Search::models();
        names.sort_unstable();
        let count = names.len();
        names.dedup();
        assert_eq!(names.len(), count, "a duplicate name would make one row unreachable");
    }

    #[test]
    fn an_unknown_model_is_a_failure_rather_than_a_guess() {
        assert_eq!(Search::of_model("homewizard-p1"), None);
        assert!(Search::models().contains(&"sparky-p1"));
    }

    #[test]
    fn a_model_name_is_matched_without_case_or_padding() {
        assert_eq!(
            Search::of_model("  Shelly-Pro-3EM "),
            Search::of_model("shelly-pro-3em")
        );
    }

    #[test]
    fn the_pair_command_always_states_access() {
        let serial = Serial::parse(MAC).expect("a serial");
        let pairing = Pairing::in_service(serial);
        assert_eq!(pairing.command(), "CRL:111-7-add:sn,187723572702975|access,0");
        assert!(pairing.in_use());

        // There is no way to build the command without the field: omitting it on the wire yields access 1,
        // with which the device polls the accessory and ignores the reading.
        let watched = Pairing { serial, access: 1 };
        assert_eq!(watched.command(), "CRL:111-7-add:sn,187723572702975|access,1");
        assert!(!watched.in_use());
    }

    #[test]
    fn a_paired_entry_decodes_to_its_state_serial_and_address() {
        let entries = Entry::parse_list("DEV:111-7-3,187723572702975,192.168.2.212\0");
        assert_eq!(entries.len(), 1);
        let entry = &entries[0];
        assert_eq!(entry.number, 111);
        assert_eq!(entry.mode, 7);
        assert_eq!(entry.state, State::Paired);
        assert!(entry.state.is_paired());
        assert_eq!(entry.serial.map(Serial::get), Some(DECIMAL));
        assert_eq!(entry.address.as_deref(), Some("192.168.2.212"));
    }

    #[test]
    fn a_tombstone_keeps_what_it_was_holding_and_is_not_paired() {
        // Deleting a *paired* entry leaves the serial and address in place; deleting a registration leaves
        // them blank. Both are state 5, and neither is enough to pair from.
        let kept = Entry::parse_list("DEV:111-7-5,187723572702975,192.168.2.212");
        assert_eq!(kept[0].state, State::Deleted);
        assert!(!kept[0].state.is_paired());
        assert_eq!(kept[0].serial.map(Serial::get), Some(DECIMAL));

        let blank = Entry::parse_list("DEV:111-7-5,,");
        assert_eq!(blank[0].state, State::Deleted);
        assert_eq!(blank[0].serial, None);
        assert_eq!(blank[0].address, None);
    }

    #[test]
    fn a_fresh_search_registers_an_entry_with_nothing_in_it() {
        let entries = Entry::parse_list("DEV:111-7-0,,");
        assert_eq!(entries[0].state, State::Registered);
        assert_eq!(entries[0].state.code(), 0);
        assert_eq!(entries[0].state.label(), "registered");
        assert_eq!(entries[0].serial, None);
    }

    #[test]
    fn an_empty_list_is_no_entries_rather_than_a_parse_failure() {
        // What an untouched device reports, and what the radio's own register reports with nothing paired.
        assert!(Entry::parse_list("DEV:").is_empty());
        assert!(Entry::parse_list("DEV:\0\0").is_empty());
        // And a value that is not a list at all yields nothing rather than a bad entry.
        assert!(Entry::parse_list("").is_empty());
        assert!(Entry::parse_list("ADD:111-7-_http._tcp.,2").is_empty());
    }

    #[test]
    fn several_entries_are_separated_by_an_ampersand() {
        let entries = Entry::parse_list("DEV:2-1-3,helio-a,192.168.1.77&111-7-5,,");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].number, 2);
        assert_eq!(entries[0].mode, 1);
        assert_eq!(entries[1].mode, 7);
    }

    #[test]
    fn the_mode_says_how_an_accessory_is_reached_and_the_two_are_not_interchangeable() {
        // The exact list the device reported on 2026-09-04 with two dialled accessories held at once. Both
        // were *written* as type 2 and came back as 2 and 3, so the leading field is the device's own and
        // not the one a client sends — which is why nothing here addresses an entry by it.
        let entries = Entry::parse_list("DEV:2-1-0,helio-a,192.168.1.77:3333&3-1-0,helio-b,192.168.1.78:3333");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].kind(), "dialled");
        assert_eq!(entries[1].kind(), "dialled");
        assert_eq!((entries[0].number, entries[1].number), (2, 3));

        // A dialled entry's second field is a name, not a serial, so it parses as one and not the other.
        assert_eq!(entries[0].name.as_deref(), Some("helio-a"));
        assert_eq!(entries[0].serial, None);
        assert_eq!(entries[0].address.as_deref(), Some("192.168.1.77:3333"));

        let discovered = Entry::parse_list("DEV:111-7-3,187723572702975,192.168.2.212");
        assert_eq!(discovered[0].kind(), "discovered");
        assert_eq!(discovered[0].name.as_deref(), Some("187723572702975"));
        assert_eq!(discovered[0].serial.map(Serial::get), Some(DECIMAL));
    }

    #[test]
    fn a_mode_this_build_has_not_seen_is_reported_rather_than_guessed_at() {
        let entries = Entry::parse_list("DEV:5-9-0,,");
        assert_eq!(entries[0].kind(), "unknown");
    }

    #[test]
    fn a_row_that_does_not_parse_is_skipped_and_the_others_survive() {
        // A report is a report: one unfamiliar row must not hide the rest.
        let entries = Entry::parse_list("DEV:nonsense&111-7-3,187723572702975,192.168.2.212");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].state, State::Paired);
    }

    #[test]
    fn an_unknown_state_still_decodes() {
        let entries = Entry::parse_list("DEV:111-7-9,,");
        assert_eq!(entries[0].state, State::Other(9));
        assert_eq!(entries[0].state.code(), 9);
        assert_eq!(entries[0].state.label(), "unknown");
    }

    #[test]
    fn register_123_reports_nothing_as_zero_rather_than_as_a_serial() {
        assert_eq!(found("0"), None);
        assert_eq!(found("0\0"), None);
        assert_eq!(found(""), None);
        assert_eq!(found("187723572702975").map(Serial::get), Some(DECIMAL));
    }
}
