//! The LoRa radio of protocol generation 7: the register that opens it for pairing.
//!
//! The datalogger drives an **LLCC68** in LoRa mode — the vendor's own component, log tag and parameter
//! block all name it that, and the parameters it takes are a spreading factor and a coding rate, which
//! belong to no other modulation. "Sub-GHz" is true and weaker: the part covers 150–960 MHz and the
//! frequency is not compiled into the firmware, so the band is unknown. It is **not** LoRaWAN — none of
//! that machinery is present.
//!
//! The identifiers say `lora` rather than `radio` because this unit has **three** radios — Wi-Fi,
//! Bluetooth and this one — so `radio` would name none of them.
//!
//! **This is the LoRa path only.** A device of this family acquires accessories three unrelated ways, and
//! conflating them is the mistake this module exists to prevent:
//!
//! | How it is acquired | Where |
//! |---|---|
//! | *adopted* over the LoRa radio, during a window the server opens | here |
//! | *searched for* over the local network by mDNS, then polled by address | the configuration space, written with `ADD:` |
//! | no accessory at all — a reading written straight into holding registers | [`crate::growatt::v7::meter`] |
//!
//! Only the first is expressible as a single action with no arguments. The network path takes a service
//! name and an accessory type and returns a list of what was found, so it belongs to a different interface
//! with a different shape; nothing here should grow to cover it.
//!
//! No accessory has ever been observed answering one of these windows, so the adoption itself and whatever
//! the device reports afterwards are both unobserved.
//!
//! Which kinds of accessory reach a device this way is not recorded here, and should not be: nothing in
//! this module depends on it, and a list of models in a comment is a claim that ages badly and cannot be
//! checked from the code.
//!
//! # What the server can and cannot say
//!
//! It can say "start pairing". It cannot say **which** accessory to pair. Two different accessories
//! requested through the vendor's own application produced frames identical byte for byte, checksum
//! included, so the type is not in the command and neither is a serial. Which accessory is adopted is
//! decided by which one is in its own pairing state at that moment, because somebody pressed a button on
//! it.
//!
//! That is why [`pair`] takes no arguments, and why a caller must not offer a choice of model. An
//! interface that asks which accessory to pair is describing something the protocol cannot express.
//!
//! # Nothing here has to be undone
//!
//! The register clears itself. Observed: written `1`, acknowledged 1.75 s later, and reading `0` again
//! when the window had ended — with no second write from anyone. So this is a one-shot command rather
//! than a mode, and there is no window left open by a caller that forgets to close it.
//!
//! # Why this bypasses the writable map
//!
//! [`PAIR_REGISTER`] holds nothing. It is not a setting, has no domain, and a read of it reports whether
//! a window happens to be open rather than a stored value — so an entry in the holding register map would
//! describe it wrongly and would surface it as a number somebody could type into. It goes out as
//! [`Command::Trigger`] instead, which exists for exactly this and is constructed only here.
//!
//! # This exposes the LoRa radio
//!
//! For as long as the window is open the device will adopt an accessory that asks to be adopted, and
//! nothing in the protocol authenticates the accessory to the device or the device to the accessory. So it
//! is an owner's decision, taken while standing next to the hardware, and never a maintenance step
//! something else may take on their behalf.

use crate::growatt::v7::encode::Command;
use crate::model::{Raw, Register};

/// The register that opens a pairing window on the LoRa radio.
///
/// The sub-MCU's own debug output names this address as a calibration command, in one line naming 319,
/// 320 and 321 together **`[F]`**. Nothing about the observed behaviour matches that: the vendor's server
/// writes it in an accessory-pairing flow, the device acknowledges it, and no calibration follows. The most
/// economical explanation is that the datalogger intercepts this register before forwarding it inward, as
/// it already does for the meter block, but that is inferred and not established. Register **319** is
/// deliberately left alone: the two names came from one debug line and only one of them has been cleared
/// by observation.
pub const PAIR_REGISTER: Register = Register(320);

/// The value that opens the window. The vendor's server sends `1` and the app's own request carries the
/// literal string `"1"`, which the cloud passes through untransformed.
pub const PAIR_TRIGGER: Raw = Raw(1);

/// Open a pairing window on the LoRa radio.
///
/// Takes nothing, because the command carries nothing: see the module documentation for why an interface
/// offering a choice of accessory would be describing something that does not exist.
pub fn pair() -> Command {
    Command::Trigger {
        register: PAIR_REGISTER,
        value: PAIR_TRIGGER,
    }
}

#[cfg(test)]
mod tests {
    use super::{PAIR_REGISTER, PAIR_TRIGGER, pair};
    use crate::growatt::v7::encode::Command;
    use crate::growatt::v7::frame::MessageType;
    use crate::growatt::v7::registers::HoldingRegister;

    #[test]
    fn it_writes_the_pairing_register_with_the_trigger_value() {
        assert_eq!(
            pair(),
            Command::Trigger {
                register: PAIR_REGISTER,
                value: PAIR_TRIGGER
            }
        );
    }

    #[test]
    fn it_goes_out_as_a_single_register_write() {
        // The vendor uses `0x06` for this, and "behave like the server it replaces" is the whole design. A
        // one-register range write would be a different frame on the wire for the same intent.
        assert_eq!(pair().message_type(), MessageType::WriteSingleRegister);
    }

    #[test]
    fn the_body_is_the_register_and_the_value() {
        let frame = pair().to_frame("0EXAMPLE00000001").expect("a printable device id");
        // Register 320 = 0x0140, value 1 — the four octets the vendor's own server sends.
        assert_eq!(frame.body(), [0x01, 0x40, 0x00, 0x01]);
        assert_eq!(frame.wire_len(), 44);
    }

    #[test]
    fn nothing_is_read_back_afterwards() {
        // The register clears itself, so a read-back races the window instead of confirming anything.
        assert!(pair().registers_to_verify().is_empty());
    }

    #[test]
    fn the_register_is_not_a_setting() {
        // If it ever gains an entry in the holding map it becomes writable as a value, which is the one
        // thing this must not be. The absence is load-bearing, so it is asserted rather than assumed.
        assert!(
            HoldingRegister::lookup(PAIR_REGISTER).is_none(),
            "register {PAIR_REGISTER} must not be a writable setting: it holds nothing"
        );
    }
}
