//! Turning what the server asks for into a frame the device accepts.
//!
//! The other half of [`crate::growatt::report`]: that one says what arrived, this one says what goes out.
//! Everything a command needs and the seam does not carry — which registers a setting occupies, what the
//! vendor's own server puts in each field, at which QoS it publishes — is decided here.

use snafu::OptionExt as _;

use crate::driver::commands::{Command as Asked, Outgoing};
use crate::growatt::v7::encode::{Command, EncodeError, NotWritableConfigSnafu, WritableConfig};
use crate::growatt::v7::meter;
use crate::mqtt::QoS;

/// Prepare one command for `device_id`.
///
/// # Errors
///
/// [`EncodeError`] if the command cannot be expressed: an unwritable register, a value out of range, a
/// config field absent from the allowlist.
pub fn prepare(device_id: &str, asked: &Asked) -> Result<Outgoing, EncodeError> {
    let command = translate(asked)?;
    let frame = command.to_frame(device_id)?;

    Ok(Outgoing {
        // Matching the capture rather than picking one: the vendor sends config writes at QoS 1 and
        // register writes at QoS 0. All four commands captured from its web interface were QoS 1, like the
        // clock push. A read goes at QoS 1 by this program's own choice, not the vendor's: it is a question
        // this program asked, and the PUBACK is how it learns the device took it.
        qos: match command {
            Command::WriteConfig { .. } | Command::ReadSingle { .. } => QoS::AtLeastOnce,
            Command::WriteSingle { .. } | Command::WriteRange { .. } | Command::ReadConfig { .. } => QoS::AtMostOnce,
        },
        acknowledged: command.is_acknowledged(),
        description: frame.message_type().to_string(),
        verify: command.registers_to_verify(),
        payload: frame.to_wire(),
    })
}

/// What the server asked for, in this generation's terms.
fn translate(asked: &Asked) -> Result<Command, EncodeError> {
    match asked {
        // `set` rather than `write`: it is the one that reproduces the vendor's composite writes, so
        // `default_output_power` goes out as the `321..322` range the vendor server sends and a schedule
        // slot as all five of its registers.
        Asked::Set { register, value } => Command::set(*register, *value),
        Asked::Read { register } => Ok(Command::read(*register)),
        Asked::ReadConfig { registers } => Ok(Command::read_config_many(registers)),
        Asked::WriteConfig { register, value } => {
            // The allowlist is consulted here as well as by whoever asked, because this is the last place
            // it can be: past this point there are octets.
            let field = WritableConfig::ALL
                .into_iter()
                .find(|entry| entry.register() == *register)
                .context(NotWritableConfigSnafu { register: *register })?;
            Command::write_config(field, value.clone())
        }
        Asked::PushTime(time) => Command::time_push(*time),
        Asked::MeterReading { watts, valid } => meter::command(*watts, *valid),
        Asked::Restart => Ok(Command::restart_datalogger()),
        Asked::FactoryReset => Ok(Command::factory_reset()),
    }
}
