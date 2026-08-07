//! Protocol generation 7.
//!
//! Everything here is specific to this generation: the obfuscation key and its coverage, the CRC and
//! what it is computed over, the body layouts, and the register maps.
//!
//! The generation-agnostic header lives in [`crate::growatt::header`].

pub mod decode;
pub mod frame;
pub mod registers;

pub use decode::{Telemetry, Timestamp};
pub use frame::{Frame, FrameError, MessageType};
