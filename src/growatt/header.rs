//! The 8-octet frame header, shared by every generation of the family.
//!
//! ```text
//! 0      2        4        6   7
//! +------+--------+--------+---+------+
//! | txid | proto  | length |ad |func  |
//! +------+--------+--------+---+------+
//! ```
//!
//! This is the one part that cannot be generation-specific, because it is what tells a receiver
//! which generation it is holding. It is transmitted in clear in generation 7 and parsed here without
//! reference to obfuscation, integrity or body layout — all of which are generation-specific and live
//! in the version modules.

use crate::growatt::ProtocolVersion;

/// Size of the header.
pub const LEN: usize = 8;

/// A parsed frame header.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub struct Header {
    /// Transaction ID. Every observed frame in either direction carried `0x0001`; the field's
    /// purpose is unconfirmed.
    pub transaction_id: u16,
    /// Protocol generation, which selects the codec.
    pub protocol: ProtocolVersion,
    /// Declared length: total frame length minus 8. Counts every octet after itself except the
    /// trailing CRC.
    pub length: u16,
    /// Modbus unit address. `0x01` is device-scoped, `0xFE` datalogger-scoped.
    pub address: u8,
    /// Function code.
    pub function: u8,
}

impl Header {
    /// Read a header without validating anything beyond having enough octets.
    ///
    /// Deliberately permissive. Its job is to let a caller discover the generation and the message
    /// type; whether the frame is well formed is the codec's business, and a frame that fails
    /// validation still needs its header to be reportable in the log line that says so.
    pub fn peek(wire: &[u8]) -> Option<Self> {
        let head = wire.get(..LEN)?;
        match *head {
            [t0, t1, p0, p1, l0, l1, address, function] => Some(Self {
                transaction_id: u16::from_be_bytes([t0, t1]),
                protocol: ProtocolVersion(u16::from_be_bytes([p0, p1])),
                length: u16::from_be_bytes([l0, l1]),
                address,
                function,
            }),
            _ => None,
        }
    }

    /// Serialise back to octets.
    pub const fn to_bytes(self) -> [u8; LEN] {
        let [t0, t1] = self.transaction_id.to_be_bytes();
        let [p0, p1] = self.protocol.number().to_be_bytes();
        let [l0, l1] = self.length.to_be_bytes();
        [t0, t1, p0, p1, l0, l1, self.address, self.function]
    }

    /// The length a frame with this header should occupy in total, CRC included.
    ///
    /// From the rule `length = total − 8`, so `total = length + 8`. Computed in `usize` because the
    /// arithmetic overflows `u16` at the top of the range.
    pub const fn implied_total_len(self) -> usize {
        (self.length as usize).saturating_add(LEN)
    }

    /// Whether the declared length agrees with the octets actually present.
    pub const fn length_matches(self, actual_total: usize) -> bool {
        self.implied_total_len() == actual_total
    }
}

#[cfg(test)]
mod tests {
    use super::{Header, LEN};
    use crate::growatt::ProtocolVersion;

    /// The header of a real 585-octet telemetry frame.
    const TELEMETRY_HEADER: [u8; LEN] = [0x00, 0x01, 0x00, 0x07, 0x02, 0x41, 0x01, 0x04];

    #[test]
    fn parses_a_telemetry_header() {
        let header = Header::peek(&TELEMETRY_HEADER).expect("eight octets is enough");
        assert_eq!(header.transaction_id, 1);
        assert_eq!(header.protocol, ProtocolVersion::V7);
        assert_eq!(header.length, 577);
        assert_eq!(header.address, 0x01);
        assert_eq!(header.function, 0x04);
    }

    #[test]
    fn length_rule_holds_for_a_real_frame() {
        let header = Header::peek(&TELEMETRY_HEADER).expect("peek");
        assert_eq!(header.implied_total_len(), 585);
        assert!(header.length_matches(585));
        assert!(!header.length_matches(584));
    }

    #[test]
    fn round_trips_to_octets() {
        let header = Header::peek(&TELEMETRY_HEADER).expect("peek");
        assert_eq!(header.to_bytes(), TELEMETRY_HEADER);
    }

    #[test]
    fn peek_needs_a_whole_header() {
        assert!(Header::peek(&TELEMETRY_HEADER[..7]).is_none());
        assert!(Header::peek(&[]).is_none());
    }

    #[test]
    fn peek_accepts_an_unsupported_generation() {
        // Must succeed, so the caller can log "unsupported generation" rather than "malformed".
        let mut octets = TELEMETRY_HEADER;
        octets[3] = 5;
        let header = Header::peek(&octets).expect("peek");
        assert_eq!(header.protocol, ProtocolVersion(5));
        assert!(!header.protocol.is_supported());
    }

    #[test]
    fn implied_length_does_not_overflow() {
        let header = Header {
            transaction_id: 1,
            protocol: ProtocolVersion::V7,
            length: u16::MAX,
            address: 1,
            function: 4,
        };
        assert_eq!(header.implied_total_len(), 65_543);
    }
}
