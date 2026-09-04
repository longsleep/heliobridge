//! Protocol generation 7.
//!
//! Everything here is specific to this generation: the obfuscation key and its coverage, the CRC and
//! what it is computed over, the body layouts, and the register maps.
//!
//! The generation-agnostic header lives in [`crate::growatt::header`].

pub mod classify;
pub mod decode;
pub mod encode;
pub mod frame;
pub mod identity;
pub mod meter;
pub mod registers;
pub mod version;

pub use decode::{Telemetry, Timestamp};
pub use encode::{Command, EncodeError, SlotConfig, SlotField, WritableRegister};
pub use frame::{Frame, FrameError, MessageType};
pub use identity::Identity;
pub use version::FirmwareVersion;
