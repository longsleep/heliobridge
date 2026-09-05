//! How Growatt advertises firmware, and what its own device looks like when it fetches.
//!
//! Knowledge only: no I/O, no downloading, no logging. [`super::driver::Growatt`] wires this into the
//! server's seam; acting on an advertisement — deciding whether to download, and keeping what arrives — is
//! [`crate::server::firmware`]'s business.

use url::Url;

use crate::driver::AdvertisedFirmware;
use crate::growatt::v7::decode::{ConfigWrite, FromFrame};
use crate::growatt::v7::frame::Frame;
use crate::model::Register;

/// The configuration register the cloud writes to advertise an update.
pub const UPDATE_URL_REGISTER: Register = Register(80);

/// What the datalogger calls itself when it fetches.
///
/// Read out of the request template compiled into its firmware **`[F]`**:
///
/// ```text
/// %s %s HTTP/1.1\r\nUser-Agent: esp-07s\r\nCache-Control: no-cache\r\nHost: %s
/// ```
///
/// A fetch that presents this, and adds nothing the template does not carry, is indistinguishable from the
/// device's own — which is the point: the vendor's CDN has no business learning that something other than
/// the device is reading its advertisements.
pub const DEVICE_USER_AGENT: &str = "esp-07s";

/// The cache directive the same template carries.
pub const DEVICE_CACHE_CONTROL: &str = "no-cache";

/// Firmware advertised by a frame, if it advertises any.
///
/// The cloud advertises an update by writing a URL into configuration register 80, so an advertisement is
/// a configuration write like any other — the periodic clock push arrives in exactly the same shape, which
/// is why the register is what separates them.
///
/// The value arrives prefixed — `1#type01#http://…` — so the URL is found rather than assumed to be the
/// whole field, and a frame that is not a configuration write, or carries no URL, simply advertises
/// nothing.
pub fn advertised(frame: &Frame) -> Option<AdvertisedFirmware> {
    let write = ConfigWrite::from_frame(frame).ok()?;
    write
        .entries
        .iter()
        .filter(|entry| entry.register == UPDATE_URL_REGISTER)
        .find_map(|entry| image(&entry.value))
}

/// The image a pushed value points at, if it points at one.
///
/// The file name is the last path segment, prefixed with the one before it where that adds something. The
/// manual channel's paths end `…/WIFI/4.0.2.6.bin`, where the version alone would collide across
/// components — the PD and the inverter ship version-named files too — so the name becomes
/// `WIFI-4.0.2.6.bin`, which is also how the images mirrored in this project are named. The automatic
/// channel's end `…/1.2/1.2-U.zip`, where the same prefixing would only stutter, so it is left alone.
fn image(value: &str) -> Option<AdvertisedFirmware> {
    let start = value.find("http://").or_else(|| value.find("https://"))?;
    let url = Url::parse(value.get(start..)?).ok()?;
    let segments: Vec<&str> = url.path_segments()?.filter(|segment| !segment.is_empty()).collect();
    let last = segments.last()?;
    let file = match segments.len().checked_sub(2).and_then(|index| segments.get(index)) {
        Some(component) if !last.contains(*component) => format!("{component}-{last}"),
        _ => (*last).to_owned(),
    };
    (!file.is_empty()).then(|| AdvertisedFirmware {
        url,
        file,
        source: format!("configuration register {UPDATE_URL_REGISTER}"),
    })
}

#[cfg(test)]
mod tests {
    use super::{UPDATE_URL_REGISTER, advertised};
    use crate::growatt::v7::frame::{Frame, MessageType};
    use crate::model::Register;

    /// What the cloud has been writing to register 80, verbatim from a capture.
    const PUSHED: &str =
        "1#type01#http://cdn.growatt.com/update/device/GB/manualUpgrade/7ca7/1eacd/f37825/R-D/WIFI/4.0.2.6.bin";

    /// A cloud configuration write carrying one assignment, framed as the vendor's server sends it.
    fn config_write(register: Register, value: &str) -> Frame {
        let mut body = 1u16.to_be_bytes().to_vec();
        let length = u16::try_from(value.len().saturating_add(4)).expect("a short value");
        body.extend_from_slice(&length.to_be_bytes());
        body.extend_from_slice(&register.number().to_be_bytes());
        body.extend_from_slice(&u16::try_from(value.len()).expect("a short value").to_be_bytes());
        body.extend_from_slice(value.as_bytes());
        Frame::new(MessageType::ConfigWrite, "0EXAMPLE00000001", &body).expect("build")
    }

    #[test]
    fn the_url_is_found_past_the_prefix() {
        let firmware = advertised(&config_write(UPDATE_URL_REGISTER, PUSHED)).expect("an advertisement");
        assert_eq!(firmware.url.host_str(), Some("cdn.growatt.com"));
        assert!(firmware.url.path().ends_with("/WIFI/4.0.2.6.bin"));
        assert_eq!(firmware.source, "configuration register 80");
    }

    #[test]
    fn the_file_name_carries_the_component_as_well_as_the_version() {
        // The version alone collides: every component ships a file named for its version.
        let firmware = advertised(&config_write(UPDATE_URL_REGISTER, PUSHED)).expect("an advertisement");
        assert_eq!(firmware.file, "WIFI-4.0.2.6.bin");
    }

    #[test]
    fn another_register_carrying_a_url_advertises_nothing() {
        // Which register matters is decided here and nowhere else.
        assert!(advertised(&config_write(Register(19), PUSHED)).is_none());
        assert!(advertised(&config_write(Register(80), PUSHED)).is_some());
    }

    #[test]
    fn the_clock_push_advertises_nothing() {
        // It arrives as the same message type, which is why the register is the discriminator.
        assert!(advertised(&config_write(Register(31), "2026-09-05 01:20:00")).is_none());
    }

    #[test]
    fn an_https_advertisement_parses_too() {
        // The automatic channel uses https, and only the manual one has been seen pushed here.
        let firmware = advertised(&config_write(
            UPDATE_URL_REGISTER,
            "1#type01#https://cdn.growatt.com/update/device/GB/autoUpgrade/7ca7/1eacd/f37825/1.2/1.2-U.zip",
        ))
        .expect("an advertisement");
        assert_eq!(firmware.url.scheme(), "https");
        // Prefixing here would only stutter: the segment before is already in the file name.
        assert_eq!(firmware.file, "1.2-U.zip");
    }

    #[test]
    fn a_value_with_no_url_advertises_nothing() {
        assert!(advertised(&config_write(UPDATE_URL_REGISTER, "1#type01#")).is_none());
        assert!(advertised(&config_write(UPDATE_URL_REGISTER, "")).is_none());
        // Nothing here dials anything but http and https.
        assert!(advertised(&config_write(UPDATE_URL_REGISTER, "ftp://cdn.growatt.com/x.bin")).is_none());
    }

    #[test]
    fn a_url_with_no_path_advertises_nothing() {
        assert!(advertised(&config_write(UPDATE_URL_REGISTER, "http://cdn.growatt.com/")).is_none());
    }

    #[test]
    fn a_frame_of_another_type_advertises_nothing() {
        let frame = Frame::new(MessageType::Telemetry, "0EXAMPLE00000001", &[0; 8]).expect("build");
        assert!(advertised(&frame).is_none());
    }
}
