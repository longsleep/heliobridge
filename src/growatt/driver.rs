//! Growatt as an implementation of the server's [`Driver`](crate::driver::Driver) seam.
//!
//! One type, one `impl` per capability, and nothing else. The knowledge each capability needs lives in its
//! own module — firmware in [`super::firmware`] — so this file stays a map of what is wired up rather than
//! a place where meaning accumulates.
//!
//! `Frame<'a>` is a parsed [`Frame`]: the whole protocol is one frame shape (§4 of the specification), so
//! the driver's own strongly typed value and the unit the cloud sends happen to coincide. A protocol
//! carrying several message shapes would put an enumeration here instead, and nothing on the server's side
//! would change.

use crate::driver::upstream::{Endpoint, Target, Upstream};
use crate::driver::{AdvertisedFirmware, Firmware, Wire};
use crate::growatt::cloud::{self, Relay, RelayError};
use crate::growatt::firmware;
use crate::growatt::v7::frame::Frame;

/// Growatt's generation-7 protocol.
#[derive(Debug, Clone, Copy, Default)]
pub struct Growatt;

impl Wire for Growatt {
    type Frame<'a> = Frame;

    fn parse<'a>(&self, payload: &'a [u8]) -> Option<Self::Frame<'a>> {
        Frame::parse(payload).ok()
    }
}

impl Upstream for Growatt {
    type Relay = Relay;
    type Error = RelayError;

    fn endpoint(&self) -> Endpoint {
        Endpoint {
            host: cloud::DEFAULT_HOST.to_owned(),
            port: cloud::DEFAULT_PORT,
        }
    }

    fn certificate_names(&self) -> &'static [&'static str] {
        cloud::CERTIFICATE_NAMES
    }

    fn relay(&self, device_id: &str, target: Target) -> Result<Self::Relay, Self::Error> {
        Relay::start(device_id, target)
    }
}

impl Firmware for Growatt {
    fn advertised(&self, frame: &Self::Frame<'_>) -> Option<AdvertisedFirmware> {
        firmware::advertised(frame)
    }

    fn request(&self, firmware: &AdvertisedFirmware) -> http::request::Builder {
        http::Request::builder()
            .method(http::Method::GET)
            .uri(firmware.url.as_str())
            .header(http::header::USER_AGENT, firmware::DEVICE_USER_AGENT)
            .header(http::header::CACHE_CONTROL, firmware::DEVICE_CACHE_CONTROL)
    }
}

#[cfg(test)]
mod tests {
    use super::Growatt;
    use crate::driver::{Firmware, Wire};
    use crate::growatt::v7::frame::{Frame, MessageType};

    #[test]
    fn a_payload_that_is_not_a_frame_is_not_a_message() {
        // The seam takes octets, so nonsense has to be a `None` rather than a panic.
        assert!(Growatt.parse(b"not a frame at all").is_none());
        assert!(Growatt.parse(&[]).is_none());
    }

    #[test]
    fn a_frame_round_trips_through_the_seam() {
        let frame = Frame::new(MessageType::Telemetry, "0EXAMPLE00000001", &[0; 8]).expect("build");
        let wire = frame.to_wire();
        let parsed = Growatt.parse(&wire).expect("a message");
        assert_eq!(parsed.message_type(), MessageType::Telemetry);
        // Telemetry advertises no firmware, and asking is not an error.
        assert!(Growatt.advertised(&parsed).is_none());
    }

    #[test]
    fn the_firmware_request_says_what_the_device_says_and_no_more() {
        let firmware = crate::driver::AdvertisedFirmware {
            url: url::Url::parse("http://cdn.growatt.com/x/WIFI/4.0.2.6.bin").expect("a URL"),
            file: "WIFI-4.0.2.6.bin".to_owned(),
            source: "configuration register 80".to_owned(),
        };
        let request = Growatt.request(&firmware).body(()).expect("a request");
        assert_eq!(request.method(), http::Method::GET);
        assert_eq!(request.headers().get(http::header::USER_AGENT).unwrap(), "esp-07s");
        assert_eq!(request.headers().get(http::header::CACHE_CONTROL).unwrap(), "no-cache");
        // The device's template carries nothing else, so neither does this.
        assert_eq!(request.headers().len(), 2, "{:?}", request.headers());
    }
}
