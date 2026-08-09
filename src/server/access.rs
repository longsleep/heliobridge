//! Who may connect: a source-address allowlist and a device-serial allowlist.
//!
//! The listener otherwise accepts any connection that completes the TLS handshake, and takes whatever
//! serial the CONNECT carries. That is right for a device on an isolated VLAN behind a firewall, and it is
//! the whole of the access control there — the protocol's credentials are the serial plus a fixed string,
//! so they identify rather than authenticate, and anyone who can reach the port and knows a serial *is*
//! that device as far as this program can tell.
//!
//! # Empty means everything
//!
//! Both lists are empty by default and an empty list admits everyone. A deployment that needs neither
//! should not have to configure them away, and the common case — one device, one VLAN — needs neither.
//!
//! # A typo must not be quiet
//!
//! An unparseable entry is a startup failure. The failure mode of a mistyped allowlist is a device that
//! silently stops connecting, which is worth being loud about before it happens rather than after.

use core::fmt;
use core::str::FromStr;
use std::net::IpAddr;

use ipnet::IpNet;
use snafu::Snafu;

/// Why an allowlist could not be read.
#[derive(Debug, Snafu, PartialEq, Eq)]
#[snafu(visibility(pub))]
pub enum AccessError {
    /// An entry was neither an address nor a network.
    #[snafu(display("{entry:?} is not an address or network, e.g. 192.168.2.238 or 2001:db8::/32"))]
    Malformed {
        /// What was written.
        entry: String,
    },
}

/// Which source addresses may connect.
///
/// Checked on `accept`, before the TLS handshake, so an unwanted peer costs a socket and a log line rather
/// than a certificate exchange. The peer address is the only thing consulted: there is no proxy in front of
/// this listener, so nothing may trust a forwarded header.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Peers {
    /// Empty admits everything. A bare address is held as a single-host network.
    networks: Vec<IpNet>,
}

impl Peers {
    /// Read a comma-separated list of addresses and networks, in either address family.
    ///
    /// # Errors
    ///
    /// [`AccessError::Malformed`] naming the entry that could not be read.
    pub fn parse(list: &str) -> Result<Self, AccessError> {
        let networks = list
            .split(',')
            .map(str::trim)
            .filter(|entry| !entry.is_empty())
            .map(|entry| {
                IpNet::from_str(entry)
                    // A bare address is the same thing with every bit significant, so both spellings go
                    // through one containment test rather than two code paths.
                    .or_else(|_ignored| IpAddr::from_str(entry).map(IpNet::from))
                    .map_err(|_ignored| AccessError::Malformed {
                        entry: entry.to_owned(),
                    })
            })
            .collect::<Result<Vec<IpNet>, AccessError>>()?;
        Ok(Self { networks })
    }

    /// Whether anything is allowed to connect from anywhere.
    pub fn is_open(&self) -> bool {
        self.networks.is_empty()
    }

    /// Whether a peer may connect.
    ///
    /// Listing only IPv4 does **not** deny IPv6, or the reverse: a list is a list of what is allowed, and
    /// an address family nobody mentioned is simply not mentioned. Doing otherwise is how a device that
    /// arrives over link-local IPv6 disappears the day a v4 network is added.
    pub fn admits(&self, address: IpAddr) -> bool {
        self.is_open() || self.networks.iter().any(|network| network.contains(&address))
    }

    /// What is allowed, as parsed.
    ///
    /// Deliberately not `len`/`is_empty`: an empty list here means *allow everything*, so a method called
    /// `is_empty` would read as the opposite of what it does. [`Self::is_open`] says it plainly.
    pub fn networks(&self) -> &[IpNet] {
        &self.networks
    }
}

impl fmt::Display for Peers {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_open() {
            return f.write_str("anywhere");
        }
        let listed: Vec<String> = self.networks.iter().map(ToString::to_string).collect();
        f.write_str(&listed.join(","))
    }
}

/// Which device serials may be served.
///
/// Checked when the serial becomes known, which is at CONNECT — before the session registers, so a refused
/// device never reaches the registry, never gets a Home Assistant entity and never has a frame recorded.
///
/// This is the filter that matters if the listener is ever reachable beyond a private network, since a
/// serial is the only thing an impostor needs. It is not a substitute for keeping it unreachable: the
/// serial crosses the wire inside a session whose certificate the device does not verify, so anyone
/// positioned to capture one already has it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Devices {
    /// Empty admits everything.
    serials: Vec<String>,
}

impl Devices {
    /// Read a comma-separated list of serials.
    ///
    /// Never fails: a serial is whatever the device calls itself, so there is no shape to validate against
    /// and an unrecognised entry is indistinguishable from a device that has not connected yet.
    pub fn parse(list: &str) -> Self {
        Self {
            serials: list
                .split(',')
                .map(str::trim)
                .filter(|entry| !entry.is_empty())
                .map(str::to_owned)
                .collect(),
        }
    }

    /// Whether any device may connect.
    pub fn is_open(&self) -> bool {
        self.serials.is_empty()
    }

    /// Whether a device may be served.
    pub fn admits(&self, serial: &str) -> bool {
        self.is_open() || self.serials.iter().any(|allowed| allowed == serial)
    }

    /// Which serials are allowed, as parsed. See [`Peers::networks`] for why this is not `len`.
    pub fn serials(&self) -> &[String] {
        &self.serials
    }
}

impl fmt::Display for Devices {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_open() {
            return f.write_str("any");
        }
        f.write_str(&self.serials.join(","))
    }
}

#[cfg(test)]
mod tests {
    use super::{AccessError, Devices, Peers};
    use std::net::IpAddr;
    use std::str::FromStr as _;

    fn address(text: &str) -> IpAddr {
        IpAddr::from_str(text).expect("an address")
    }

    #[test]
    fn an_empty_list_admits_everything() {
        // The default, and the common case: one device on an isolated VLAN needs neither list.
        for list in ["", "   ", ",", " , "] {
            let peers = Peers::parse(list).expect("empty");
            assert!(peers.is_open(), "{list:?}");
            assert!(peers.admits(address("203.0.113.9")), "{list:?}");
            assert!(peers.admits(address("2001:db8::1")), "{list:?}");
            assert_eq!(peers.to_string(), "anywhere");
        }
        assert!(Devices::parse("").is_open());
        assert!(Devices::parse("").admits("0EXAMPLE00000001"));
    }

    #[test]
    fn a_bare_address_matches_only_itself() {
        let peers = Peers::parse("192.0.2.238").expect("parse");
        assert!(peers.admits(address("192.0.2.238")));
        assert!(!peers.admits(address("192.0.2.239")));
        assert!(!peers.is_open());
    }

    #[test]
    fn a_network_matches_its_members_and_nothing_else() {
        let peers = Peers::parse("192.0.2.0/24").expect("parse");
        assert!(peers.admits(address("192.0.2.1")));
        assert!(peers.admits(address("192.0.2.255")));
        assert!(!peers.admits(address("192.0.3.1")));
    }

    #[test]
    fn both_address_families_work_and_neither_denies_the_other() {
        // The trap worth a test: a list that mentions only IPv4 must not become an implicit deny for IPv6,
        // which is how a device arriving over link-local disappears the day a v4 network is added.
        let v4_only = Peers::parse("192.0.2.0/24").expect("parse");
        assert!(!v4_only.admits(address("2001:db8::1")), "not listed, so not admitted");

        let both = Peers::parse("192.0.2.0/24, 2001:db8::/32, fe80::/10").expect("parse");
        assert!(both.admits(address("192.0.2.7")));
        assert!(both.admits(address("2001:db8::dead:beef")));
        assert!(both.admits(address("fe80::1")));
        assert!(!both.admits(address("2001:db9::1")));
        assert_eq!(both.networks().len(), 3);
    }

    #[test]
    fn an_ipv6_address_may_be_written_bare_too() {
        let peers = Peers::parse("2001:db8::1,::1").expect("parse");
        assert!(peers.admits(address("2001:db8::1")));
        assert!(peers.admits(address("::1")));
        assert!(!peers.admits(address("2001:db8::2")));
    }

    #[test]
    fn whitespace_around_entries_is_forgiven() {
        // A list written across a systemd unit or a shell variable picks these up, and refusing it would be
        // a startup failure over nothing.
        let peers = Peers::parse(" 192.0.2.1 , 2001:db8::/32 ").expect("parse");
        assert_eq!(peers.networks().len(), 2);
        assert!(peers.admits(address("192.0.2.1")));
    }

    #[test]
    fn a_typo_is_a_startup_failure_rather_than_an_entry_that_matches_nothing() {
        // The failure mode of a mistyped allowlist is a device that silently stops connecting.
        for bad in [
            "192.0.2.256",
            "192.0.2.0/33",
            "not-an-address",
            "192.0.2.0/",
            "example.com",
        ] {
            let error = Peers::parse(bad).expect_err(bad);
            assert!(matches!(error, AccessError::Malformed { .. }), "{bad}: {error:?}");
        }
        // And one bad entry refuses the whole list, rather than leaving a hole nobody notices.
        assert!(Peers::parse("192.0.2.0/24,nonsense").is_err());
    }

    #[test]
    fn a_serial_list_admits_exactly_what_it_names() {
        let devices = Devices::parse("0EXAMPLE00000001, 0EXAMPLE00000002");
        assert!(devices.admits("0EXAMPLE00000001"));
        assert!(devices.admits("0EXAMPLE00000002"));
        assert!(!devices.admits("0EXAMPLE00000003"));
        assert!(!devices.admits(""));
        assert_eq!(devices.serials().len(), 2);
        // Case matters: a serial is an identifier the device chose, not a name to be normalised.
        assert!(!devices.admits("0example00000001"));
    }

    #[test]
    fn a_list_says_what_it_holds() {
        assert_eq!(Peers::parse("192.0.2.0/24").expect("parse").to_string(), "192.0.2.0/24");
        // A bare address renders as the single-host network it became, which is what it means.
        assert_eq!(Peers::parse("192.0.2.1").expect("parse").to_string(), "192.0.2.1/32");
        assert_eq!(Devices::parse("0EXAMPLE00000001").to_string(), "0EXAMPLE00000001");
        assert_eq!(Devices::parse("").to_string(), "any");
    }
}
