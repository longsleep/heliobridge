//! The tasks that keep Home Assistant informed.
//!
//! [`Publisher`] owns the broker connection and does the two things that are not about any one device:
//! saying whether this program is running, and starting a [`Link`] for each device that connects.
//! Everything else belongs to a link, one task per device, so a device that goes quiet holds nothing up.
//!
//! # A device task, not a loop over devices
//!
//! Each link watches one session's telemetry, settings and identity, and holds its own watchdog timer. A
//! shared loop would have to poll every device on a common tick, which is the wrong shape twice over: it
//! adds latency to a frame that already arrived, and it makes one device's silence everyone's problem.
//!
//! # Nothing is published for a device that is not reporting
//!
//! Not zero, and not the last value again. A zero on an energy counter reads as a counter reset and gives
//! the Energy dashboard a spike the size of the day's total; a repeated value is a flat line
//! indistinguishable from a real one. Instead the device's availability topic goes to `offline` and Home
//! Assistant records a gap, which is what actually happened.

use core::time::Duration;
use std::collections::HashSet;
use std::sync::Arc;

use tokio::sync::{mpsc, watch};
use tokio::time::Instant;

use crate::control::{Action as ControlAction, Connected, Registry, SessionHandle, TelemetryView};
use crate::homeassistant::broker::{Broker, BrokerConfig, BrokerError, Event, Publication, Publications};
use crate::homeassistant::command::{Change, Delivery, Permitted};
use crate::homeassistant::discovery::{DeviceBlock, Discovery};
use crate::homeassistant::entity::{Catalogue, Component, Entity};
use crate::homeassistant::state::{Fields, StatePayload};
use crate::homeassistant::topics::{OFFLINE, ONLINE, Topics};

/// How long without a telemetry frame before the device is reported absent.
///
/// Six missed cycles. The device's own MQTT keepalive is 420 s, so waiting for the protocol to notice a
/// half-open connection would leave stale readings on a dashboard for seven minutes; telemetry every five
/// seconds is a far better heartbeat than the keepalive.
pub const OFFLINE_AFTER: Duration = Duration::from_secs(30);

/// How many devices may end a session between one wake-up of the publisher and the next.
///
/// Sessions end at human pace even for a device that reconnects aggressively.
const FAREWELL_DEPTH: usize = 16;

/// What to publish, as against where.
#[derive(Debug, Clone, Copy)]
pub struct PublisherOptions {
    /// How many schedule slots get entities, 1–9.
    pub slots: u16,
    /// What a command topic may change.
    ///
    /// The same answer decides both halves: a setting that may not be written is not published as a
    /// control, and a command naming it is refused. Publishing a control that would be refused, or
    /// accepting a command for a control that was never offered, are the two ways for those to disagree.
    pub permitted: Permitted,
    /// How long without a telemetry frame before the device is reported absent.
    pub offline_after: Duration,
}

impl Default for PublisherOptions {
    fn default() -> Self {
        Self {
            slots: 1,
            permitted: Permitted::default(),
            offline_after: OFFLINE_AFTER,
        }
    }
}

/// Which broker connection is current.
///
/// Zero means there has never been one, and a link publishes nothing until there has: queueing a discovery
/// burst at a broker that is down would fill the queue with messages that are stale by the time it comes
/// back. Every change means "say everything again", which is what makes a broker restart, a network blip
/// and a first start one case instead of three.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, PartialOrd, Ord)]
struct Generation(u64);

impl Generation {
    /// Whether the broker has ever been reached.
    const fn is_established(self) -> bool {
        self.0 > 0
    }

    /// The next one. Saturating: after 2^64 broker connections, staying at the last is harmless — links
    /// republish on change, and a counter that stopped changing only means they stop being asked to.
    const fn next(self) -> Self {
        Self(self.0.saturating_add(1))
    }
}

/// Publishes device state to Home Assistant, and accepts commands back.
#[derive(Debug)]
pub struct Publisher {
    broker: Broker,
    topics: Arc<Topics>,
    registry: Registry,
    options: PublisherOptions,
    devices: watch::Receiver<Connected>,
    /// Bumped on every broker connection, so every link republishes what the broker retains.
    generation: watch::Sender<Generation>,
    /// Devices with a link running, so a second one is never started for the same device.
    linked: HashSet<String>,
    /// How a link reports that its session ended.
    farewell: mpsc::Sender<String>,
    /// Where those reports arrive.
    ended: mpsc::Receiver<String>,
}

impl Publisher {
    /// Connect to the broker and start publishing.
    ///
    /// # Errors
    ///
    /// Whatever [`Broker::connect`] reports. A broker that is merely unreachable is not an error: the
    /// client retries in the background.
    pub fn start(
        mut config: BrokerConfig,
        topics: Topics,
        registry: Registry,
        options: PublisherOptions,
    ) -> Result<Self, BrokerError> {
        config.will = Some(topics.will());
        config.subscriptions = vec![topics.command_filter()];

        let devices = registry.watch();
        let (farewell, ended) = mpsc::channel(FAREWELL_DEPTH);
        Ok(Self {
            broker: Broker::connect(config)?,
            topics: Arc::new(topics),
            registry,
            options,
            devices,
            generation: watch::Sender::new(Generation::default()),
            linked: HashSet::new(),
            farewell,
            ended,
        })
    }

    /// Run until the broker client stops.
    pub async fn run(mut self) {
        loop {
            tokio::select! {
                event = self.broker.next_event() => {
                    let Some(event) = event else {
                        tracing::warn!("the broker client stopped; Home Assistant will not be updated");
                        return;
                    };
                    self.handle(event);
                }

                changed = self.devices.changed() => {
                    if changed.is_err() {
                        // The registry is gone, which means the program is shutting down.
                        return;
                    }
                    self.devices.borrow_and_update();
                    self.link_devices();
                }

                ended = self.ended.recv() => {
                    // The channel cannot close while this holds the sender.
                    if let Some(device) = ended {
                        self.session_ended(&device);
                    }
                }
            }
        }
    }

    /// React to something the broker reported.
    fn handle(&mut self, event: Event) {
        match event {
            Event::Connected => {
                self.publish(Publication::retained(self.topics.bridge_availability(), ONLINE));
                // Every link now says everything again, which is also what starts the first one talking.
                self.generation.send_modify(|current| *current = current.next());
                self.link_devices();
            }
            Event::Message { topic, payload } => {
                if let Some(device) = self.topics.device_of_command(&topic) {
                    self.handle_command(&device, &payload);
                } else {
                    tracing::debug!(%topic, "ignoring a message on an unexpected topic");
                }
            }
        }
    }

    /// Start a link for every connected device that has none.
    ///
    /// Called whenever anything might have changed rather than only on a device arriving, because the
    /// connected *set* does not change when a device reconnects — the registry replaces the session behind
    /// the same serial. A link ending is what reveals that, so this runs then too.
    fn link_devices(&mut self) {
        for device in self.devices.borrow().devices() {
            if self.linked.contains(device) {
                continue;
            }
            let Some(handle) = self.registry.handle(device) else {
                continue;
            };
            // A session that has already dropped its request channel is on its way out. Its own link would
            // exit immediately; skipping it leaves the work to the registration that replaces it.
            if handle.requests.is_closed() {
                tracing::debug!(%device, "not linking a session that is ending");
                continue;
            }

            tracing::info!(%device, "device connected; announcing it to Home Assistant");
            self.linked.insert(device.clone());
            tokio::spawn(
                Link {
                    device: device.clone(),
                    session: handle,
                    publications: self.broker.publications(),
                    topics: Arc::clone(&self.topics),
                    options: self.options,
                    generation: self.generation.subscribe(),
                    farewell: self.farewell.clone(),
                    published_for: None,
                    announced: None,
                    arrived: None,
                    last_update: None,
                    present: false,
                    fields: Fields::default(),
                }
                .run(),
            );
        }
    }

    /// A device's session ended.
    ///
    /// Its readings are reported gone rather than left at their last value, and a link is started again if
    /// the device has meanwhile reconnected.
    fn session_ended(&mut self, device: &str) {
        tracing::info!(%device, "device session ended");
        self.linked.remove(device);
        self.publish(Publication::retained(self.topics.device_availability(device), OFFLINE));
        self.publish(StatePayload::status(false, None).retained(self.topics.status(device)));
        self.link_devices();
    }

    /// Carry out a command that arrived on a device's command topic.
    ///
    /// Parsed here so a refusal is logged next to the message that caused it, and applied in a task of its
    /// own because a write waits for the device to be read back — up to twenty seconds, which is far too
    /// long to hold a loop that is also publishing telemetry.
    ///
    /// Nothing is published in reply. The read-back updates the session's settings cache, the link watching
    /// it publishes the confirmed value, and Home Assistant learns what the device actually stored rather
    /// than what it was asked for.
    fn handle_command(&self, device: &str, payload: &[u8]) {
        let Some(session) = self.registry.handle(device) else {
            // Expected on a broker carrying more than one bridge: the command topic is a wildcard, so
            // every bridge sees every device's commands and answers only for its own.
            tracing::debug!(%device, "ignoring a command for a device this bridge does not serve");
            return;
        };

        let changes = match Change::from_payload(payload, self.options.permitted) {
            Ok(changes) => changes,
            Err(error) => {
                tracing::warn!(%device, %error, "refusing a command");
                return;
            }
        };

        let device = device.to_owned();
        tokio::spawn(async move {
            for change in changes {
                apply(&session, &device, change).await;
            }
        });
    }

    /// Queue a publication.
    fn publish(&mut self, publication: Publication) {
        self.broker.try_publish(publication);
    }
}

/// Everything published about one device.
///
/// One of these per connected session, holding what has been said so far so that a change can be published
/// as a difference rather than as a repetition: discovery goes out again only when the device's own
/// description changes, and availability only when presence does.
#[derive(Debug)]
struct Link {
    device: String,
    session: SessionHandle,
    publications: Publications,
    topics: Arc<Topics>,
    options: PublisherOptions,
    generation: watch::Receiver<Generation>,
    farewell: mpsc::Sender<String>,
    /// The broker connection everything currently published was published for.
    published_for: Option<Generation>,
    /// What the discovery messages on the broker describe.
    announced: Option<Announcement>,
    /// When the last telemetry frame arrived, on the timer's clock. `None` before the first.
    arrived: Option<Instant>,
    /// The same moment as a timestamp, for the sensor that reports staleness.
    last_update: Option<String>,
    /// Whether the device is currently reported as reporting.
    present: bool,
    /// Which fields the announced entities read.
    fields: Fields,
}

/// What the discovery messages on the broker describe.
#[derive(Debug)]
struct Announcement {
    device: DeviceBlock,
    catalogue: Catalogue,
    /// Kept so an entity that leaves the catalogue can be withdrawn by name and component.
    entities: Vec<Entity>,
}

impl Announcement {
    /// Whether this is still what a discovery message would say.
    fn describes(&self, device: &DeviceBlock, catalogue: Catalogue) -> bool {
        self.device == *device && self.catalogue == catalogue
    }
}

impl Link {
    /// Publish for this device until its session ends.
    async fn run(mut self) {
        // Marked seen without being published: a frame that was already in the channel arrived at an
        // unknown time, and treating it as fresh would be inventing a timestamp.
        self.session.telemetry.borrow_and_update();

        // The connection that is already up counts. A link is started when a device connects, which is
        // usually *after* the broker connection it publishes over, and `changed()` reports only what
        // happens from here on — so waiting for one would mean waiting for the broker to drop.
        let current = *self.generation.borrow_and_update();
        self.republish(current);

        loop {
            // Recomputed each time round, because each frame moves it.
            let quiet_at = self.arrived.and_then(|at| at.checked_add(self.options.offline_after));

            tokio::select! {
                changed = self.generation.changed() => {
                    if changed.is_err() {
                        // The publisher is gone, so there is nowhere to publish.
                        return;
                    }
                    let generation = *self.generation.borrow_and_update();
                    self.republish(generation);
                }

                changed = self.session.telemetry.changed() => {
                    if changed.is_err() {
                        break;
                    }
                    self.telemetry_arrived();
                }

                changed = self.session.settings.changed() => {
                    if changed.is_err() {
                        break;
                    }
                    self.publish_settings();
                }

                changed = self.session.identity.changed() => {
                    if changed.is_err() {
                        break;
                    }
                    // A device that has just said what model it is deserves a device page that says so.
                    self.announce();
                    self.publish_config();
                }

                () = async move {
                    match quiet_at {
                        Some(deadline) => tokio::time::sleep_until(deadline).await,
                        // Nothing has arrived yet, so there is nothing this timer could change: the device
                        // is already reported absent.
                        None => core::future::pending().await,
                    }
                } => {
                    self.went_quiet();
                }
            }
        }

        // Not the availability topic: the publisher owns that, so the same message is not published from
        // two places, and so it is still published if this task is torn down rather than ending.
        drop(self.farewell.send(self.device).await);
    }

    /// Say everything the broker retains, for a connection that has just been established.
    fn republish(&mut self, generation: Generation) {
        if !generation.is_established() {
            return;
        }
        tracing::debug!(device = %self.device, "republishing everything for a new broker connection");
        // Discovery is keyed on what it describes, not on the connection, so the record of what was
        // announced is cleared to force it out again.
        self.announced = None;
        self.published_for = Some(generation);

        self.announce();
        self.publish_presence();
        self.publish_settings();
        // Only if it is still current. A frame from before the outage is not state.
        let current = self.fresh().then(|| self.session.telemetry.borrow().clone()).flatten();
        if let Some(view) = current {
            self.publish_telemetry(&view);
        }
    }

    /// A telemetry frame arrived.
    ///
    /// Buffered records never reach here: the session decodes those and publishes nothing, since one is a
    /// sample the device took up to an hour earlier and replayed on connect.
    fn telemetry_arrived(&mut self) {
        let view = self.session.telemetry.borrow_and_update().clone();
        let Some(view) = view else {
            return;
        };

        self.arrived = Some(Instant::now());
        // Arrival here rather than the device's own clock, which is set by whoever pushed it last and has
        // been seen disagreeing. A staleness sensor that jumps because the device is wrong about the time
        // is worse than none.
        self.last_update = Some(chrono::Local::now().to_rfc3339());

        // A pack count that has changed adds or removes per-pack entities, so this comes first.
        self.announce();
        if !self.present {
            self.present = true;
            tracing::info!(device = %self.device, "device is reporting");
        }
        // Every frame, not only the one that changes presence: the staleness sensor is what makes a reading
        // that looks wrong answerable, and it moves each time.
        self.publish_presence();
        self.publish_telemetry(&view);
    }

    /// The device has been quiet too long.
    ///
    /// The socket may still look alive — that is the case this exists for — so nothing about the session is
    /// touched. Only what is said about it changes.
    fn went_quiet(&mut self) {
        if !self.present {
            return;
        }
        self.present = false;
        tracing::warn!(
            device = %self.device,
            quiet_for_s = self.options.offline_after.as_secs(),
            "no telemetry; reporting the device absent so its readings go unavailable rather than stale"
        );
        self.publish_presence();
    }

    /// Whether the last frame is recent enough to be current state.
    fn fresh(&self) -> bool {
        self.arrived.is_some_and(|at| at.elapsed() < self.options.offline_after)
    }

    /// Publish discovery, if what it would say has changed.
    ///
    /// Idempotent by design: it is called whenever anything that feeds a discovery message might have
    /// moved, and compares before publishing. Announcing sixty entities on every telemetry frame would be
    /// sixty retained writes every five seconds.
    fn announce(&mut self) {
        if self.published_for.is_none() {
            return;
        }
        let device = DeviceBlock::new(&self.device, self.session.identity.borrow().as_ref());
        let catalogue = Catalogue {
            slots: self.options.slots,
            permitted: self.options.permitted,
            packs: self.packs(),
        };
        if self
            .announced
            .as_ref()
            .is_some_and(|announced| announced.describes(&device, catalogue))
        {
            return;
        }

        let entities = catalogue.entities();
        let withdrawn = self.withdraw(&entities);
        tracing::info!(
            device = %self.device,
            entities = entities.len(),
            withdrawn,
            packs = catalogue.packs,
            "announcing entities to Home Assistant"
        );
        for entity in &entities {
            let publication = Discovery {
                entity,
                topics: &self.topics,
                device: &device,
            }
            .publication();
            self.publish(publication);
        }

        self.fields = Fields::of(&entities);
        self.announced = Some(Announcement {
            device,
            catalogue,
            entities,
        });
    }

    /// Withdraw the entities Home Assistant has been told about that are no longer in the catalogue.
    ///
    /// An empty retained payload on a discovery topic is how a discovered entity is removed; without it the
    /// entity stays on the device page forever, with a state nothing will ever publish again.
    ///
    /// The first announcement of a session reconciles against **every entity the register maps can
    /// produce**, not against what this process said, because it has said nothing yet — and the broker may
    /// still be holding whatever a previous run left there. A second battery announced before the device
    /// said it has one is exactly that case.
    fn withdraw(&mut self, keeping: &[Entity]) -> usize {
        // Whether this is the first announcement of the session, which is the one that has to assume the
        // broker holds whatever some other run left there.
        let first = self.announced.is_none();
        let previous = match self.announced.take() {
            Some(announced) => announced.entities,
            None => Catalogue::everything(),
        };

        let retired = if first { Catalogue::RETIRED } else { &[] };
        let gone = Self::retractions(&previous, keeping, retired);

        for (component, key) in &gone {
            let topic = self.topics.discovery(*component, &self.device, key);
            self.publish(Publication::retained(topic, Vec::new()));
        }
        gone.len()
    }

    /// Which discovery topics need an empty retained payload.
    ///
    /// Pure, and separated from the publishing for one reason: [`Catalogue::RETIRED`] is empty in every
    /// build that has shipped, so the branch that sweeps it would otherwise never execute and could rot
    /// unnoticed until the day it mattered. A test passes a synthetic entry through here instead.
    fn retractions(
        previous: &[Entity],
        keeping: &[Entity],
        retired: &[(Component, &'static str)],
    ) -> Vec<(Component, &'static str)> {
        let kept = |component: Component, key: &str| {
            keeping
                .iter()
                .any(|entity| entity.key == key && entity.component == component)
        };

        let mut gone: Vec<(Component, &'static str)> = previous
            .iter()
            .filter(|entity| !kept(entity.component, entity.key))
            .map(|entity| (entity.component, entity.key))
            .collect();

        // Entities this program used to publish. They cannot appear in `previous` — the catalogue is built
        // from the code and their constructors are gone — so without this the retained discovery message
        // outlives every trace of the entity that produced it.
        for entry in retired {
            if !kept(entry.0, entry.1) && !gone.contains(entry) {
                gone.push(*entry);
            }
        }
        gone
    }

    /// How many battery packs to publish entities for.
    ///
    /// What the device reports, and one until it has: it is a battery, so it has at least that. Not an
    /// `Option`, because "unknown" and "one" would build two catalogues with identical contents — and each
    /// distinct catalogue costs an announcement of every entity in it.
    fn packs(&self) -> u16 {
        self.session
            .telemetry
            .borrow()
            .as_ref()
            .and_then(|view| reading(view, "battery_pack_count"))
            .unwrap_or(1)
    }

    /// Publish whether the device is reporting, and how stale its readings are.
    fn publish_presence(&mut self) {
        let availability = if self.present { ONLINE } else { OFFLINE };
        let topic = self.topics.device_availability(&self.device);
        self.publish(Publication::retained(topic, availability));

        let status =
            StatePayload::status(self.present, self.last_update.as_deref()).retained(self.topics.status(&self.device));
        self.publish(status);
    }

    /// Publish the datalogger configuration worth showing.
    fn publish_config(&mut self) {
        let entries = self.session.identity.borrow().as_ref().map(|view| view.entries.clone());
        let Some(entries) = entries else { return };
        let payload = StatePayload::config(&entries, &self.fields);
        if payload.is_empty() {
            return;
        }
        // Retained, unlike telemetry: it arrives once per connect, so a subscriber joining mid-session
        // would otherwise see nothing until the device reconnected.
        self.publish(payload.retained(self.topics.config(&self.device)));
    }

    /// Publish one telemetry frame.
    fn publish_telemetry(&mut self, view: &TelemetryView) {
        let payload = StatePayload::telemetry(view, &self.fields);
        if payload.is_empty() {
            return;
        }
        let publication = payload.publication(self.topics.state(&self.device));
        self.publish(publication);
    }

    /// Publish the settings a session has read back.
    ///
    /// Retained, unlike telemetry: a setting is not replaced on a cycle, so a subscriber that arrives
    /// between two changes would otherwise see nothing until someone changed something.
    fn publish_settings(&mut self) {
        let payload = StatePayload::settings(&self.session.settings.borrow(), &self.fields);
        if payload.is_empty() {
            return;
        }
        let publication = payload.retained(self.topics.settings(&self.device));
        self.publish(publication);
    }

    /// Queue a publication, unless there is nothing to publish over.
    ///
    /// The single gate on everything a link says. A device reports every five seconds whether or not the
    /// broker is reachable, so without this an outage would fill the queue with messages that were
    /// superseded long before it returned — and the first thing a new connection does is say everything
    /// again anyway.
    fn publish(&mut self, publication: Publication) {
        if self.published_for.is_none() {
            return;
        }
        self.publications.try_publish(publication);
    }
}

/// Apply one change and log what the device made of it.
///
/// The outcome is not published from here. A write is followed by a read-back, which updates the session's
/// settings cache, which the device's link is watching — so the value Home Assistant sees is the one the
/// device actually holds, by the same path as any other settings change.
async fn apply(session: &SessionHandle, device: &str, change: Change) {
    let also_read = change.also_read();
    tracing::info!(%device, setting = %change.key, "applying a command from Home Assistant");

    // An action is transmitted and answered as sent. Putting it through `Apply` would wait for a read-back
    // that the config space never produces, and report a working restart as a failure to confirm.
    if change.delivery == Delivery::FireAndForget {
        match session.carry_out(ControlAction::Send(change.command)).await {
            Ok(_outcome) => tracing::info!(%device, action = %change.key, "sent"),
            Err(error) => tracing::warn!(%device, action = %change.key, %error, "could not be sent"),
        }
        return;
    }

    match session.carry_out(ControlAction::Apply(change.command)).await {
        Ok(outcome) if outcome.confirmed => {
            tracing::info!(%device, setting = %change.key, value = ?outcome.value, "the device stored it");
        }
        // Not an error: the device silently clamps rather than rejecting, and reporting what it *did* store
        // is the whole reason a write is read back. Home Assistant will show the stored value.
        Ok(outcome) => {
            tracing::warn!(
                %device,
                setting = %change.key,
                requested = ?outcome.requested,
                stored = ?outcome.stored,
                "the device did not store what was asked"
            );
        }
        Err(error) => {
            tracing::warn!(%device, setting = %change.key, %error, "the command could not be carried out");
            return;
        }
    }

    // A gating flag moves another register with it, with no write to that register at all. Read it rather
    // than model the dependency, so the published value is the device's own.
    if let Some(register) = also_read {
        match session.carry_out(ControlAction::Refresh(register)).await {
            Ok(outcome) => {
                tracing::info!(
                    %device,
                    register = register.number(),
                    value = ?outcome.value,
                    "re-read the register the flag gates"
                );
            }
            Err(error) => {
                tracing::warn!(
                    %device,
                    register = register.number(),
                    %error,
                    "could not re-read the register the flag gates; its published value may be stale"
                );
            }
        }
    }
}

/// One numeric reading out of a frame, by name.
fn reading(view: &TelemetryView, name: &str) -> Option<u16> {
    view.readings
        .iter()
        .find(|reading| reading.name == name)
        .and_then(|reading| reading.value.parse().ok())
}

#[cfg(test)]
mod tests {
    use super::{
        Catalogue, Component, FAREWELL_DEPTH, Fields, Generation, Link, OFFLINE_AFTER, PublisherOptions, reading,
    };
    use crate::control::{IdentityView, ReadingView, SessionHandle, SettingView, StatusView, TelemetryView};
    use crate::homeassistant::broker::{Publication, Publications};
    use crate::homeassistant::topics::Topics;
    use core::time::Duration;
    use std::sync::Arc;
    use tokio::sync::{mpsc, watch};

    /// The device's own serial in these tests, matching the one used across the documentation.
    const DEVICE: &str = "0EXAMPLE00000001";

    /// The ends a session would hold, kept alive so nothing closes underneath the link.
    struct Session {
        telemetry: watch::Sender<Option<TelemetryView>>,
        settings: watch::Sender<Vec<SettingView>>,
        _identity: watch::Sender<Option<IdentityView>>,
        _requests: mpsc::Receiver<crate::control::Request>,
    }

    /// Everything the link publishes into, drained by the test.
    struct Wire {
        published: mpsc::Receiver<Publication>,
        generation: watch::Sender<Generation>,
        farewells: mpsc::Receiver<String>,
        session: Session,
    }

    impl Wire {
        /// Every message published so far, in order.
        fn drain(&mut self) -> Vec<Publication> {
            let mut out = Vec::new();
            while let Ok(publication) = self.published.try_recv() {
                out.push(publication);
            }
            out
        }

        /// Every message published to one topic.
        fn on(&mut self, topic: &str) -> Vec<Publication> {
            self.drain()
                .into_iter()
                .filter(|publication| publication.topic == topic)
                .collect()
        }
    }

    /// A link over channels a test controls, already running.
    fn link(generation: Generation, options: PublisherOptions) -> Wire {
        let (publications, published) = Publications::channel(256);
        let (telemetry, telemetry_rx) = watch::channel(None);
        let (settings, settings_rx) = watch::channel(Vec::new());
        let (identity, identity_rx) = watch::channel(None);
        let (_status, status_rx) = watch::channel(StatusView::default());
        let (requests, requests_rx) = mpsc::channel(4);
        let (farewell, farewells) = mpsc::channel(FAREWELL_DEPTH);
        let generation = watch::Sender::new(generation);

        tokio::spawn(
            Link {
                device: DEVICE.to_owned(),
                session: SessionHandle {
                    requests,
                    settings: settings_rx,
                    identity: identity_rx,
                    telemetry: telemetry_rx,
                    status: status_rx,
                },
                publications,
                topics: Arc::new(Topics {
                    instance: "attic".to_owned(),
                    ..Topics::default()
                }),
                options,
                generation: generation.subscribe(),
                farewell,
                published_for: None,
                announced: None,
                arrived: None,
                last_update: None,
                present: false,
                fields: Fields::default(),
            }
            .run(),
        );

        Wire {
            published,
            generation,
            farewells,
            session: Session {
                telemetry,
                settings,
                _identity: identity,
                _requests: requests_rx,
            },
        }
    }

    /// Let the link run until it is waiting again.
    ///
    /// Time is paused in these tests, so there is nothing to wait *for* — only the scheduler to give the
    /// task its turn, which the timer advance below also does.
    async fn settle() {
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
    }

    /// One telemetry frame, with the fields these tests read.
    fn frame(power: &str) -> TelemetryView {
        TelemetryView {
            timestamp: None,
            readings: vec![
                ReadingView {
                    register: 5,
                    name: "ac_power",
                    raw: 30_000,
                    value: power.to_owned(),
                    unit: "W",
                    confidence: "verified",
                },
                ReadingView {
                    register: 12,
                    name: "battery_pack_count",
                    raw: 1,
                    value: "1".to_owned(),
                    unit: "",
                    confidence: "observed",
                },
            ],
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_link_started_after_the_broker_connected_still_announces_itself() {
        // The device connects after the broker does, so a link subscribes to the generation only once it is
        // already current — and `changed()` reports nothing that happened before. Waiting for a change
        // would mean waiting for the broker to drop.
        let mut wire = link(Generation::default().next(), PublisherOptions::default());
        settle().await;

        let discovery: Vec<Publication> = wire
            .drain()
            .into_iter()
            .filter(|publication| publication.topic.starts_with("homeassistant/"))
            .collect();
        assert!(discovery.len() > 40, "announced only {} entities", discovery.len());
        assert!(discovery.iter().all(|publication| publication.retain));
        assert!(discovery.iter().any(
            |publication| publication.topic == "homeassistant/sensor/heliobridge/0EXAMPLE00000001_ac_power/config"
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn entities_a_previous_run_announced_are_withdrawn() {
        // The broker keeps discovery messages, so a run configured for nine slots leaves entities behind
        // that a run configured for one must take away — as does one that announced four battery packs
        // before the device said it has one. An empty retained payload is how that is done; without it the
        // entity stays on the device page with a state nothing will ever publish again.
        let mut wire = link(Generation::default().next(), PublisherOptions::default());
        settle().await;

        let withdrawn: Vec<Publication> = wire
            .drain()
            .into_iter()
            .filter(|publication| publication.topic.starts_with("homeassistant/") && publication.payload.is_empty())
            .collect();
        assert!(
            withdrawn.iter().any(|publication| {
                publication.topic == "homeassistant/sensor/heliobridge/0EXAMPLE00000001_battery4_temp/config"
            }),
            "a fourth battery pack was never in this catalogue and must be taken away"
        );
        assert!(
            withdrawn.iter().any(|publication| {
                publication.topic == "homeassistant/number/heliobridge/0EXAMPLE00000001_slot9_output_power/config"
            }),
            "only one slot is exposed, so the ninth must be taken away"
        );
        assert!(
            withdrawn.iter().all(|publication| publication.retain),
            "a withdrawal has to replace the retained message, so it must be retained itself"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_pack_the_device_reports_is_announced_rather_than_withdrawn() {
        // The other direction, and the reason one pack is assumed before the device has said: a pack that
        // appears only adds entities, where a pack that was never there has to be taken away again.
        let mut wire = link(Generation::default().next(), PublisherOptions::default());
        settle().await;
        wire.drain();

        let mut two_packs = frame("-100");
        if let Some(count) = two_packs
            .readings
            .iter_mut()
            .find(|reading| reading.name == "battery_pack_count")
        {
            count.value = "2".to_owned();
        }
        wire.session.telemetry.send_replace(Some(two_packs));
        settle().await;

        let announced = wire.on("homeassistant/sensor/heliobridge/0EXAMPLE00000001_battery2_temp/config");
        let published = announced.last().expect("the second pack should be announced");
        assert!(!published.payload.is_empty(), "announced, not withdrawn");
    }

    #[tokio::test(start_paused = true)]
    async fn a_link_publishes_nothing_until_the_broker_has_been_reached() {
        // Otherwise the queue fills with a discovery burst nobody can deliver, and what does arrive when
        // the broker returns was superseded while it was away.
        let mut wire = link(Generation::default(), PublisherOptions::default());
        settle().await;
        assert!(wire.drain().is_empty(), "published before the broker was reachable");

        // A frame arriving in the meantime is still not published.
        wire.session.telemetry.send_replace(Some(frame("-100")));
        settle().await;
        assert!(wire.drain().is_empty());

        // And the first connection says everything, including what arrived while it was down.
        wire.generation.send_modify(|current| *current = current.next());
        settle().await;
        let published = wire.drain();
        assert!(
            published
                .iter()
                .any(|publication| publication.topic.starts_with("homeassistant/")),
            "the first connection should announce the entities"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_device_is_reported_present_on_its_first_frame_and_not_before() {
        let mut wire = link(Generation::default().next(), PublisherOptions::default());
        settle().await;
        let availability = "heliobridge/0EXAMPLE00000001/availability";
        assert_eq!(
            wire.on(availability)
                .last()
                .map(|publication| publication.payload.clone()),
            Some(b"offline".to_vec()),
            "a connected device that has not reported yet has no readings to offer"
        );

        wire.session.telemetry.send_replace(Some(frame("-100")));
        settle().await;
        assert_eq!(
            wire.on(availability)
                .last()
                .map(|publication| publication.payload.clone()),
            Some(b"online".to_vec())
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_frame_is_published_as_transient_state_carrying_its_fields() {
        let mut wire = link(Generation::default().next(), PublisherOptions::default());
        settle().await;
        wire.drain();

        wire.session.telemetry.send_replace(Some(frame("-100")));
        settle().await;

        let state = wire.on("heliobridge/0EXAMPLE00000001/state");
        let published = state.first().expect("a state message");
        assert!(!published.retain, "state is replaced within seconds");
        let object: serde_json::Value = serde_json::from_slice(&published.payload).expect("valid JSON");
        assert_eq!(object["ac_power"], serde_json::json!(-100));
    }

    #[tokio::test(start_paused = true)]
    async fn going_quiet_reports_the_device_absent_and_publishes_no_substitute() {
        // The rule the Energy dashboard depends on: no zero, no repeat of the last value. A zero on a
        // `total_increasing` counter reads as a reset, and the next real value is counted as new energy.
        let mut wire = link(Generation::default().next(), PublisherOptions::default());
        settle().await;
        wire.session.telemetry.send_replace(Some(frame("-100")));
        settle().await;
        wire.drain();

        tokio::time::advance(OFFLINE_AFTER.saturating_sub(Duration::from_secs(1))).await;
        settle().await;
        assert!(wire.drain().is_empty(), "one late frame is not an outage");

        tokio::time::advance(Duration::from_secs(2)).await;
        settle().await;
        let published = wire.drain();
        assert_eq!(
            published
                .iter()
                .find(|publication| publication.topic.ends_with("/availability"))
                .map(|publication| publication.payload.clone()),
            Some(b"offline".to_vec())
        );
        assert!(
            !published
                .iter()
                .any(|publication| publication.topic.ends_with("/state")),
            "nothing may be published as the device's current reading"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_staleness_of_a_reading_stays_answerable_while_the_device_is_absent() {
        // Status carries the last update and is retained, so the one sensor that keeps working through an
        // outage says how old the readings on the dashboard are.
        let mut wire = link(Generation::default().next(), PublisherOptions::default());
        settle().await;
        wire.session.telemetry.send_replace(Some(frame("-100")));
        settle().await;
        tokio::time::advance(OFFLINE_AFTER.saturating_add(Duration::from_secs(1))).await;
        settle().await;

        let status = wire.on("heliobridge/0EXAMPLE00000001/status");
        let last = status.last().expect("a status message");
        assert!(last.retain, "nothing will republish it while the device is away");
        let object: serde_json::Value = serde_json::from_slice(&last.payload).expect("valid JSON");
        assert_eq!(object["connected"], "offline");
        assert!(
            object["last_update"].is_string(),
            "the timestamp of the last frame outlives the device"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_reconnecting_broker_is_told_everything_again() {
        let mut wire = link(Generation::default().next(), PublisherOptions::default());
        settle().await;
        wire.session.telemetry.send_replace(Some(frame("-100")));
        settle().await;
        wire.drain();

        wire.generation.send_modify(|current| *current = current.next());
        settle().await;

        let published = wire.drain();
        let announced = published
            .iter()
            .filter(|publication| publication.topic.starts_with("homeassistant/"))
            .count();
        assert!(announced > 40, "only {announced} entities were announced again");
        assert!(
            published
                .iter()
                .any(|publication| publication.topic.ends_with("/state")),
            "a frame that is still current is state, so it is published again"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_stale_frame_is_not_republished_as_current_state() {
        // A broker that returns after the device went quiet must not be handed the reading from before the
        // outage. It is the same frame, and it is no longer true.
        let mut wire = link(Generation::default().next(), PublisherOptions::default());
        settle().await;
        wire.session.telemetry.send_replace(Some(frame("-100")));
        settle().await;
        tokio::time::advance(OFFLINE_AFTER.saturating_add(Duration::from_secs(1))).await;
        settle().await;
        wire.drain();

        wire.generation.send_modify(|current| *current = current.next());
        settle().await;

        let published = wire.drain();
        assert!(
            !published
                .iter()
                .any(|publication| publication.topic.ends_with("/state")),
            "the frame from before the outage is not current state"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn settings_are_retained_because_nothing_republishes_them_on_a_cycle() {
        let mut wire = link(Generation::default().next(), PublisherOptions::default());
        settle().await;
        wire.drain();

        wire.session.settings.send_replace(vec![SettingView {
            register: 326,
            name: "grid_power_allowed",
            raw: 1,
            value: "1".to_owned(),
            unit: "",
        }]);
        settle().await;

        let settings = wire.on("heliobridge/0EXAMPLE00000001/settings");
        let published = settings.last().expect("a settings message");
        assert!(published.retain);
        let object: serde_json::Value = serde_json::from_slice(&published.payload).expect("valid JSON");
        assert_eq!(object["grid_power_allowed"], serde_json::json!(1));
    }

    #[tokio::test(start_paused = true)]
    async fn a_session_ending_is_reported_to_the_publisher_rather_than_announced_here() {
        // The publisher owns the availability topic on this path, so the same message is not published from
        // two places — and so it is still published if the link is torn down instead of ending.
        let mut wire = link(Generation::default().next(), PublisherOptions::default());
        settle().await;

        drop(wire.session);
        settle().await;
        assert_eq!(wire.farewells.try_recv().ok().as_deref(), Some(DEVICE));
    }

    #[test]
    fn nothing_is_published_before_the_broker_has_ever_been_reached() {
        // Otherwise a discovery burst is queued at a broker that is down, and by the time it comes back the
        // queue holds messages that were superseded while it was away.
        assert!(!Generation::default().is_established());
        assert!(Generation::default().next().is_established());
    }

    #[test]
    fn every_broker_connection_is_a_new_generation() {
        // What makes a link say everything again. Equality is the whole mechanism: a link republishes when
        // the value it holds differs from the current one.
        let first = Generation::default().next();
        let second = first.next();
        assert_ne!(first, second);
        assert!(second > first);
    }

    #[test]
    fn the_watchdog_is_six_telemetry_cycles() {
        // Long enough not to declare the device dead on one late frame, short enough that a half-open
        // connection does not leave stale readings on a dashboard for the device's 420-second keepalive.
        assert_eq!(OFFLINE_AFTER.as_secs(), 30);
        assert_eq!(PublisherOptions::default().offline_after, OFFLINE_AFTER);
        assert_eq!(PublisherOptions::default().slots, 1);
        assert!(PublisherOptions::default().permitted.writes);
    }

    #[test]
    fn the_pack_count_is_read_out_of_the_frame_that_carries_it() {
        // What decides whether a second battery's entities exist at all.
        let view = TelemetryView {
            timestamp: None,
            readings: vec![ReadingView {
                register: 12,
                name: "battery_pack_count",
                raw: 2,
                value: "2".to_owned(),
                unit: "",
                confidence: "observed",
            }],
        };
        assert_eq!(reading(&view, "battery_pack_count"), Some(2));
        assert_eq!(reading(&view, "ac_power"), None);
    }

    #[test]
    fn a_retired_entity_is_withdrawn_even_though_the_catalogue_forgot_it() {
        // The case the retired list exists for: an entity deleted from the code, so it appears in neither
        // what is being kept nor what the catalogue can describe. Without the list nothing would ever
        // retract its retained discovery message.
        let keeping = Catalogue::everything();
        let retired = [(Component::Sensor, "a_deleted_sensor")];

        let gone = Link::retractions(&[], &keeping, &retired);
        assert!(
            gone.contains(&(Component::Sensor, "a_deleted_sensor")),
            "a retired entity must be retracted: {gone:?}"
        );
    }

    #[test]
    fn a_retired_key_that_came_back_is_not_withdrawn() {
        // Reusing a key would otherwise retract the live entity moments after announcing it.
        let revived = Catalogue::everything()
            .into_iter()
            .find(|entity| entity.component == Component::Sensor)
            .expect("a sensor exists");
        let retired = [(revived.component, revived.key)];

        let gone = Link::retractions(&[], core::slice::from_ref(&revived), &retired);
        assert!(
            !gone.contains(&(revived.component, revived.key)),
            "a key in use must not be retracted: {gone:?}"
        );
    }

    #[test]
    fn the_shipped_retired_list_names_nothing_still_published() {
        // A guard for the future rather than for today, since the list is empty: an entry left in place
        // after its key was reintroduced would silently retract a live entity on every reconnect.
        let live = Catalogue::everything();
        for (component, key) in Catalogue::RETIRED {
            assert!(
                !live
                    .iter()
                    .any(|entity| entity.key == *key && entity.component == *component),
                "{component}/{key} is both retired and published"
            );
        }
    }
}
