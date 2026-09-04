//! What the relay carries while the cloud relay is running, in each direction.
//!
//! Without a relay there is no second party and none of this applies.
//!
//! # Whose vocabulary this is
//!
//! The modes are this program's own: how much authority an operator leaves the cloud is a decision about
//! this product, not about any manufacturer's protocol, and `--relay-mode` is where it is made. So the
//! rules live here, stated in terms of [`Intent`] — what a frame is *trying to do*, which is as much as a
//! policy needs to know.
//!
//! A driver's whole part in this is [`Arbiter::intent`]: looking at one of its own frames and saying which
//! of these it is. Nothing else about its protocol is consulted, and a driver cannot widen or narrow what
//! an operator asked for.
//!
//! # Downlink: three modes, described from the vendor app's side
//!
//! | [`Mode`] | The app can | The app cannot |
//! |---|---|---|
//! | `full` | everything, as if this program were absent | — |
//! | `controls` | slots, power, charge limits, switches | broker endpoint, DNS, timezone, clock; anything unrecognised |
//! | `observer` | display only | change anything |
//!
//! # Uplink: reports always, answers to our own commands never
//!
//! Telemetry, the identity report and the periodic settings snapshot are **always** forwarded, in every
//! configuration. They are what the vendor app displays, and nothing the device says can change its
//! behaviour, so there is nothing to gain by withholding one.
//!
//! Answers to commands *this program* issued are a different matter, and by default they are not forwarded
//! ([`Answers::CloudOnly`]). Every local write produces an acknowledgement and a read-back; every reconnect
//! produces a read-back of each exposed setting. A controller driving the device — Home Assistant adjusting
//! output power through the day — turns that into a steady stream of frames the cloud never requested.
//!
//! **Measured: forwarding them achieves nothing.** Of the three frames that carry a settings value upstream,
//! only the periodic snapshot updates what the vendor app displays:
//!
//! | Forwarded | App updated? |
//! |---|---|
//! | read response — the read-back after every local write | no |
//! | write acknowledgement | no |
//! | periodic settings snapshot | **yes** |
//!
//! A local change to 110 W was forwarded as a read response and the app went on showing 100 W; it moved only
//! after the next snapshot. Writing 100 W back with the acknowledgement forwarded left the app showing 110 W.
//! So the app trails the device by up to an hour whatever this program forwards, and the snapshot **cannot be
//! forced** — a `0x03` range read draws no response, and the hourly schedule is the device's own, observed
//! firing to the second across four restarts of this program.
//!
//! Since the cloud demonstrably ignores these frames, forwarding them is pure cost: unrequested traffic to a
//! vendor whose APIs are documented elsewhere as rate-limiting and IP-banning, growing with however often a
//! local controller writes. Withholding them also happens to make the uplink indistinguishable from an
//! unmodified device, which is a fair secondary benefit but not the reason.
//!
//! # Intents, not frames
//!
//! Nothing here names a frame, a register map or a protocol generation. Recognising a frame as an intent
//! is [`Arbiter::intent`], which a driver answers for its own protocol and its own generations; deciding
//! whether an intent may pass belongs here. Supporting another generation means writing a classifier, not
//! revisiting these rules.

use core::fmt;
use core::time::Duration;
use std::collections::VecDeque;

use tokio::time::Instant;

use crate::model::Register;

use super::wire::Wire;

/// How long a cloud command stays claimable by its answer.
///
/// Acknowledgement latency was observed between 1.0 s and 4.3 s, and a read was answered in about 0.6 s.
/// Fifteen seconds is generous against the slowest of those without keeping stale entries long enough to be
/// claimed by a later, unrelated answer.
pub const COMMAND_TTL: Duration = Duration::from_secs(15);

/// Reading a frame's purpose, so a policy can be applied to it.
pub trait Arbiter: Wire {
    /// What this frame is trying to do, given the direction it is travelling.
    ///
    /// [`Intent::Unrecognised`] is the honest answer for anything the driver cannot place, including a
    /// frame too short to read. The downlink policy refuses that, which is the safe direction to be wrong
    /// in.
    fn intent(&self, frame: &Self::Frame<'_>, direction: Direction) -> Intent;
}

/// Which way a frame is travelling.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub enum Direction {
    /// Cloud to device.
    ToDevice,
    /// Device to cloud.
    ToCloud,
}

impl fmt::Display for Direction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::ToDevice => "to-device",
            Self::ToCloud => "to-cloud",
        })
    }
}

/// What a frame is trying to do, independent of protocol generation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Intent {
    /// Periodic or extended telemetry.
    Telemetry,
    /// The periodic settings snapshot.
    SettingsSnapshot,
    /// A datalogger identity or configuration report.
    Identity,
    /// A write to one or more settings registers.
    WriteSettings {
        /// First register written.
        start: Register,
        /// Last register written, equal to `start` for a single write.
        end: Register,
    },
    /// A write to a datalogger configuration register.
    WriteConfig {
        /// The configuration register written.
        register: Register,
    },
    /// A request to read one settings register.
    ReadRequest {
        /// The register asked for.
        register: Register,
    },
    /// An answer to a read request.
    ReadResponse {
        /// The register answered for.
        register: Register,
    },
    /// An answer to a write.
    WriteAck {
        /// First register acknowledged.
        start: Register,
        /// Last register acknowledged.
        end: Register,
    },
    /// Something this build does not recognise.
    Unrecognised,
}

impl Intent {
    /// Whether this intent answers an earlier command, and so has an originator to attribute.
    pub const fn needs_attribution(&self) -> bool {
        matches!(self, Self::ReadResponse { .. } | Self::WriteAck { .. })
    }
}

impl fmt::Display for Intent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Telemetry => f.write_str("telemetry"),
            Self::SettingsSnapshot => f.write_str("settings-snapshot"),
            Self::Identity => f.write_str("identity"),
            Self::WriteSettings { start, end } if start == end => write!(f, "write-settings({start})"),
            Self::WriteSettings { start, end } => write!(f, "write-settings({start}..{end})"),
            Self::WriteConfig { register } => write!(f, "write-config({register})"),
            Self::ReadRequest { register } => write!(f, "read-request({register})"),
            Self::ReadResponse { register } => write!(f, "read-response({register})"),
            Self::WriteAck { start, end } if start == end => write!(f, "write-ack({start})"),
            Self::WriteAck { start, end } => write!(f, "write-ack({start}..{end})"),
            Self::Unrecognised => f.write_str("unrecognised"),
        }
    }
}

/// Who caused a frame that answers an earlier command.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub enum Originator {
    /// The cloud issued the command.
    Cloud,
    /// No matching command is on record.
    ///
    /// Treated as local: a command nobody remembers issuing is not one the cloud is waiting for.
    Unknown,
}

/// Commands the cloud issued recently, so their answers can be told from ours.
///
/// The device answers a read or a write without saying who asked, so attribution has to be remembered on the
/// way past. Registers are the only correlator available — the protocol has no request identifier — which is
/// why entries expire: an unclaimed one must not sit there waiting to absorb an unrelated answer.
#[derive(Debug, Default)]
pub struct CloudCommands {
    outstanding: VecDeque<(Register, Register, Instant)>,
}

impl CloudCommands {
    /// Note a command being relayed to the device, if it is one the device will answer.
    pub fn remember(&mut self, intent: &Intent, now: Instant) {
        let (start, end) = match intent {
            Intent::WriteSettings { start, end } => (*start, *end),
            Intent::ReadRequest { register } => (*register, *register),
            // A config write draws no answer at all, so there is nothing to attribute later.
            _ => return,
        };
        self.expire(now);
        self.outstanding.push_back((start, end, now));
    }

    /// Attribute an answer, consuming the command it answers.
    ///
    /// Returns [`Originator::Unknown`] when nothing matches, which the policy treats as local — the safe way
    /// round, since an answer nobody is waiting for is one the cloud has no use for.
    pub fn claim(&mut self, intent: &Intent, now: Instant) -> Originator {
        let (start, end) = match intent {
            Intent::WriteAck { start, end } => (*start, *end),
            Intent::ReadResponse { register } => (*register, *register),
            _ => return Originator::Unknown,
        };
        self.expire(now);
        let found = self
            .outstanding
            .iter()
            .position(|&(from, to, _)| from <= start && end <= to);
        match found {
            Some(index) => {
                self.outstanding.remove(index);
                Originator::Cloud
            }
            None => Originator::Unknown,
        }
    }

    /// Drop entries too old to be answered.
    fn expire(&mut self, now: Instant) {
        self.outstanding
            .retain(|&(_, _, at)| now.saturating_duration_since(at) < COMMAND_TTL);
    }

    /// How many commands are still awaiting an answer.
    pub fn outstanding(&self) -> usize {
        self.outstanding.len()
    }
}

/// Why a frame was refused, phrased for a log line.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub enum Refusal {
    /// The cloud is an observer in this mode and may not write at all.
    ObserverMode,
    /// Datalogger configuration is not writable by the cloud.
    ConfigWrite,
    /// The frame was not recognised, and unrecognised frames are not delivered to the device.
    Unrecognised,
    /// An answer to a command the cloud never issued, which it ignores anyway.
    NotCloudOriginated,
}

impl Refusal {
    /// A short reason, stable enough to assert on.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ObserverMode => "the cloud is an observer in this mode",
            Self::ConfigWrite => "the cloud may not write datalogger configuration",
            Self::Unrecognised => "unrecognised frames are not delivered to the device",
            Self::NotCloudOriginated => "answers a command the cloud never issued",
        }
    }
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The outcome of evaluating one frame.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub enum Decision {
    /// Carry it.
    Allow,
    /// Do not carry it.
    Refuse(Refusal),
}

impl Decision {
    /// Whether the frame may be carried.
    pub const fn allowed(self) -> bool {
        matches!(self, Self::Allow)
    }

    /// The refusal, if it was refused.
    pub const fn refusal(self) -> Option<Refusal> {
        match self {
            Self::Allow => None,
            Self::Refuse(refusal) => Some(refusal),
        }
    }
}

/// How much authority the vendor cloud keeps, described from the vendor app's side.
///
/// Ordered from most to least. Every mode leaves the app displaying correctly; what differs is what the app
/// may change.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, Default, clap::ValueEnum)]
pub enum Mode {
    /// The app works fully, as if this program were not here.
    ///
    /// Includes datalogger configuration, so the cloud also owns the clock — and could retarget the device's
    /// broker away from this program.
    Full,

    /// The app works fully for device controls, but cannot touch datalogger configuration.
    ///
    /// Slots, output power, charge limits and the switches all still work from the app. Refused: the broker
    /// endpoint, DNS, timezone and clock, and anything unrecognised — which is the shape an unknown firmware
    /// trigger would take. This program owns the clock.
    #[default]
    Controls,

    /// The app displays everything and changes nothing.
    ///
    /// The cloud becomes a pure observer. Appropriate once settings are driven locally, since a second writer
    /// is then only a way for the two pictures to disagree.
    Observer,
}

impl Mode {
    /// The mode's name, as the configuration spells it.
    ///
    /// Deliberately the same word `--relay-mode` takes, so a log line, an API response and the flag that set
    /// it all read alike.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Controls => "controls",
            Self::Observer => "observer",
        }
    }

    /// Whether the cloud is permitted to write datalogger configuration.
    ///
    /// This decides who owns the device's clock. The vendor server sets it with a configuration write, so if
    /// that write is refused, this program must send its own — otherwise nobody does.
    pub const fn cloud_may_write_config(self) -> bool {
        matches!(self, Self::Full)
    }
}

/// Which answers to earlier commands are forwarded to the cloud.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, Default, clap::ValueEnum)]
pub enum Answers {
    /// Forward every answer, including those to commands this program issued.
    All,

    /// Forward only answers to commands the cloud itself issued. The default.
    ///
    /// The cloud ignores the rest — measured, see the module documentation — so forwarding them is unrequested
    /// traffic that grows with however often a local controller writes.
    #[default]
    CloudOnly,
}

/// What the relay carries in each direction.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, Default)]
pub struct Policy {
    /// How much authority the cloud keeps over the device.
    pub mode: Mode,
    /// Which answers to earlier commands reach the cloud.
    pub answers: Answers,
}

impl Policy {
    /// A policy that carries everything, which is what a relay does without one.
    pub const OPEN: Self = Self {
        mode: Mode::Full,
        answers: Answers::All,
    };

    /// Whether the cloud is permitted to write datalogger configuration.
    pub const fn cloud_may_write_config(self) -> bool {
        self.mode.cloud_may_write_config()
    }

    /// Evaluate one frame.
    ///
    /// `originator` is consulted only for intents that answer an earlier command; pass
    /// [`Originator::Unknown`] otherwise.
    pub const fn evaluate(self, direction: Direction, intent: &Intent, originator: Originator) -> Decision {
        match direction {
            Direction::ToDevice => self.to_device(intent),
            Direction::ToCloud => self.to_cloud(intent, originator),
        }
    }

    const fn to_device(self, intent: &Intent) -> Decision {
        match self.mode {
            Mode::Full => Decision::Allow,
            Mode::Observer => Decision::Refuse(Refusal::ObserverMode),
            Mode::Controls => match intent {
                // The device answers a read whoever asked, and a read changes nothing.
                //
                // That covers config reads too, which ask for the datalogger's own fields — including the
                // broker endpoint. Refusing those would protect nothing today: the identity report carrying
                // all 32 config registers is forwarded on every connect, and the settings snapshot hourly.
                //
                // It stops being true the moment either of those is withheld. If identity reports are ever
                // held back — §8.6 of the implementation plan proposes exactly that after a retarget, so the
                // cloud cannot see the new endpoint — then config reads must be held back with them, or the
                // cloud simply asks for register 19 instead. They are one disclosure through two doors.
                Intent::WriteSettings { .. } | Intent::ReadRequest { .. } => Decision::Allow,
                Intent::WriteConfig { .. } => Decision::Refuse(Refusal::ConfigWrite),
                // Fails closed, and deliberately: an unrecognised frame heading for the device is the shape
                // an unknown firmware trigger would take. Nothing observed from the vendor server falls here.
                _ => Decision::Refuse(Refusal::Unrecognised),
            },
        }
    }

    const fn to_cloud(self, intent: &Intent, originator: Originator) -> Decision {
        match self.answers {
            Answers::All => Decision::Allow,
            // Only answers are ever withheld. Reports fall through to the final arm, as does anything
            // unrecognised: the uplink fails open, the opposite of the downlink, because the cloud
            // understands frames this build does not and nothing the device says changes its behaviour.
            Answers::CloudOnly => match intent {
                Intent::ReadResponse { .. } | Intent::WriteAck { .. } => match originator {
                    Originator::Cloud => Decision::Allow,
                    Originator::Unknown => Decision::Refuse(Refusal::NotCloudOriginated),
                },
                _ => Decision::Allow,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use tokio::time::Instant;

    use super::{Answers, CloudCommands, Decision, Direction, Intent, Mode, Originator, Policy, Refusal};
    use crate::model::Register;

    const fn reg(number: u16) -> Register {
        Register(number)
    }

    fn evaluate(policy: Policy, direction: Direction, intent: &Intent) -> Decision {
        policy.evaluate(direction, intent, Originator::Unknown)
    }

    #[test]
    fn the_default_mode_carries_control_writes_but_not_configuration() {
        let policy = Policy::default();
        assert_eq!(policy.mode, Mode::Controls);
        assert!(
            evaluate(
                policy,
                Direction::ToDevice,
                &Intent::WriteSettings {
                    start: reg(257),
                    end: reg(257),
                },
            )
            .allowed(),
            "the vendor app must keep working"
        );
        assert_eq!(
            evaluate(policy, Direction::ToDevice, &Intent::WriteConfig { register: reg(31) }).refusal(),
            Some(Refusal::ConfigWrite)
        );
    }

    #[test]
    fn an_unrecognised_frame_reaches_the_device_only_in_full_mode() {
        assert_eq!(
            evaluate(Policy::default(), Direction::ToDevice, &Intent::Unrecognised).refusal(),
            Some(Refusal::Unrecognised),
            "this is the shape an unknown firmware trigger would take"
        );
        assert!(evaluate(Policy::OPEN, Direction::ToDevice, &Intent::Unrecognised).allowed());
    }

    #[test]
    fn refusing_config_writes_makes_this_program_the_clock_authority() {
        assert!(!Policy::default().cloud_may_write_config());
        assert!(Policy::OPEN.cloud_may_write_config());
    }

    #[test]
    fn an_observer_may_not_even_write_settings() {
        let policy = Policy {
            mode: Mode::Observer,
            answers: Answers::default(),
        };
        assert_eq!(
            evaluate(
                policy,
                Direction::ToDevice,
                &Intent::WriteSettings {
                    start: reg(250),
                    end: reg(251),
                },
            )
            .refusal(),
            Some(Refusal::ObserverMode)
        );
    }

    #[test]
    fn reports_are_forwarded_in_every_configuration() {
        // The vendor app is fed by these, and the periodic snapshot is the only frame that updates what it
        // displays, so withholding one would make the app wrong for no gain.
        for mode in [Mode::Full, Mode::Controls, Mode::Observer] {
            for answers in [Answers::All, Answers::CloudOnly] {
                let policy = Policy { mode, answers };
                for intent in [Intent::Telemetry, Intent::Identity, Intent::SettingsSnapshot] {
                    assert!(
                        evaluate(policy, Direction::ToCloud, &intent).allowed(),
                        "{mode:?}/{answers:?} withheld {intent}"
                    );
                }
            }
        }
    }

    #[test]
    fn by_default_only_answers_to_the_clouds_own_commands_are_forwarded() {
        let policy = Policy::default();
        assert_eq!(policy.answers, Answers::CloudOnly);
        for intent in [
            Intent::WriteAck {
                start: reg(257),
                end: reg(257),
            },
            Intent::ReadResponse { register: reg(257) },
        ] {
            assert!(
                policy
                    .evaluate(Direction::ToCloud, &intent, Originator::Cloud)
                    .allowed(),
                "{intent} was the cloud's own"
            );
            assert_eq!(
                policy
                    .evaluate(Direction::ToCloud, &intent, Originator::Unknown)
                    .refusal(),
                Some(Refusal::NotCloudOriginated),
                "{intent} answers one of ours"
            );
        }
    }

    #[test]
    fn the_uplink_fails_open_and_the_downlink_fails_closed() {
        let policy = Policy::default();
        assert!(
            evaluate(policy, Direction::ToCloud, &Intent::Unrecognised).allowed(),
            "the cloud understands frames this build does not"
        );
        assert!(!evaluate(policy, Direction::ToDevice, &Intent::Unrecognised).allowed());
    }

    #[test]
    fn an_answer_is_attributed_to_the_cloud_only_once() {
        let mut commands = CloudCommands::default();
        let now = Instant::now();
        let write = Intent::WriteSettings {
            start: reg(257),
            end: reg(257),
        };
        let ack = Intent::WriteAck {
            start: reg(257),
            end: reg(257),
        };

        commands.remember(&write, now);
        assert_eq!(commands.outstanding(), 1);
        assert_eq!(commands.claim(&ack, now), Originator::Cloud);
        // Consumed, so a second acknowledgement — ours, for the same register moments later — is not
        // mistaken for the cloud's.
        assert_eq!(commands.claim(&ack, now), Originator::Unknown);
        assert_eq!(commands.outstanding(), 0);
    }

    #[test]
    fn an_answer_inside_a_relayed_range_is_attributed_to_it() {
        let mut commands = CloudCommands::default();
        let now = Instant::now();
        commands.remember(
            &Intent::WriteSettings {
                start: reg(254),
                end: reg(258),
            },
            now,
        );
        // The vendor writes a whole slot as one range; a narrower answer must still match, because
        // attribution is about who asked rather than the shape of the reply.
        assert_eq!(
            commands.claim(
                &Intent::WriteAck {
                    start: reg(256),
                    end: reg(256)
                },
                now
            ),
            Originator::Cloud
        );
    }

    #[test]
    fn a_stale_command_cannot_absorb_a_later_answer() {
        let mut commands = CloudCommands::default();
        let now = Instant::now();
        commands.remember(&Intent::ReadRequest { register: reg(250) }, now);
        let much_later = now + super::COMMAND_TTL + core::time::Duration::from_secs(1);
        assert_eq!(
            commands.claim(&Intent::ReadResponse { register: reg(250) }, much_later),
            Originator::Unknown,
            "an unanswered command must expire, or it waits forever to mislabel one of ours"
        );
    }

    #[test]
    fn a_config_write_is_not_remembered_because_nothing_answers_it() {
        let mut commands = CloudCommands::default();
        commands.remember(&Intent::WriteConfig { register: reg(31) }, Instant::now());
        assert_eq!(commands.outstanding(), 0);
    }

    #[test]
    fn only_answers_carry_an_originator() {
        assert!(
            Intent::WriteAck {
                start: reg(1),
                end: reg(1)
            }
            .needs_attribution()
        );
        assert!(Intent::ReadResponse { register: reg(1) }.needs_attribution());
        assert!(!Intent::Telemetry.needs_attribution());
        assert!(!Intent::WriteConfig { register: reg(31) }.needs_attribution());
    }
}
