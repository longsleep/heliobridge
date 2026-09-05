# Heliobridge

A local MQTT server for the **Growatt Nexa 2000** balcony storage system: run it instead of the
vendor cloud, and keep the device working on your own network.

> **A personal weekend project, built with heavy AI assistance.** It runs against exactly one device
> — the author's — on a home network, and it is written to be honest about what has actually been
> observed rather than to be a product. Treat it accordingly. It is not affiliated with or endorsed
> by Growatt.

## Status

The device talks to it, and Home Assistant both shows it and drives it.

**Working, verified against real hardware:**

- **Is the MQTT server the device connects to.** TLS on port 7006, MQTT 3.1.1, taking the device's
  identity from its CONNECT — the credentials it sends are its serial and a firmware constant, so
  they identify rather than authenticate. No separate broker to run: this is the design decision the
  rest follows from.
- **Decodes the protocol.** Obfuscated legacy Growatt framing, CRC-16/MODBUS, the input register
  map for telemetry, the holding register map for settings, and the datalogger's own config space.
  Records the device replays from its internal archive after a reconnect are decoded and logged but
  never treated as current, since they can be over an hour old. The hourly settings snapshot is taken
  too, which is how a change made in the vendor app becomes visible without reconnecting.
- **Serves everything over a control API** on a Unix socket — telemetry, settings, identity,
  datalogger configuration — and accepts writes.
- **Writes settings with read-back confirmation**, from an allowlist, because the device silently
  clamps out-of-range values rather than rejecting them and acknowledges a range write without
  saying what it stored.
- **Sets the device's clock**, which is otherwise the vendor server's job.
- **Optionally relays to the vendor cloud**, so the phone app keeps working — with a policy
  deciding how much authority the cloud keeps.
- **Records raw frames** for later analysis, including the ones the relay policy refused.
- **Publishes to Home Assistant** over your own broker, with MQTT autodiscovery. Seventy entities
  per device, derived from the register maps rather than from a second list, and two availability
  topics — this program's own as a last will, the device's own as a telemetry watchdog — so a reading
  goes `unavailable` instead of flat-lining when the device drops off. Nothing publishes a substitute
  value, which is what keeps the Energy dashboard honest.
- **Accepts commands from Home Assistant**, through the same allowlist and the same read-back as the
  control API, so what appears in Home Assistant afterwards is what the device stored rather than what
  it was asked for.
- **Supplies the smart meter reading that smart self-use needs**, written straight into the inverter's
  holding registers. The device's own supported meters reach the same registers by polling a Shelly over
  the LAN, by Modbus, by a sub-GHz radio, or — for the vendor's documented integration — from the meter
  manufacturer's cloud by way of Growatt's. Writing the figure directly needs none of that: any source
  Home Assistant can read becomes usable, and no account anywhere is involved.
- **Reads and publishes the datalogger's own configuration** — its network settings, its endpoint, its
  signal strength and the interval it reports on — and identifies the product from the type code the
  device reports, so a device page names the model and one assembled firmware version rather than three
  register values.
- **Notices the firmware the vendor's cloud advertises**, logs the URL, and can keep the image. Nothing
  installs it; the campaign is refused either way.

**Retargeting the device's broker endpoint** by writing its config registers is deliberately
unimplemented, though it would remove the need for the DNS override below. The protocol for it is
understood; a wrong value there has no remote recovery.

**How fast and how faithfully the device follows a command** — latency for both ways of setting the
output, what each is accurate to, and what a loop holding your grid connection near zero actually needs —
is measured in [docs/output-response.md](docs/output-response.md).

## How it works

The Nexa 2000's datalogger speaks MQTT over TLS to a fixed cloud endpoint and performs **no
certificate validation**. So a local service can stand in for that endpoint with a DNS override —
point `mqtt.growatt.com` at the machine running Heliobridge — and no change to the device itself.
A destination NAT rule works equally well. Heliobridge generates its own certificate on first run.

With `--cloud-relay` it also dials the real endpoint and passes traffic both ways, so the vendor app
keeps working while everything is decoded locally.

## Configuration

One binary, no configuration file. Every option is a flag with a matching `HELIOBRIDGE_*`
environment variable; `--help` documents each one in full.

**Clearing a variable is the same as leaving it out.** `HELIOBRIDGE_MQTT_URL=` turns publishing off,
`HELIOBRIDGE_RECORD_DIR=` turns recording off, and a setting with a default falls back to it. The two
allowlists are the exception: empty is already their value, meaning admit everything.

The state directory holds the generated certificate, which the device does not verify and which is
regenerated when missing. Point `HELIOBRIDGE_STATE_DIR` somewhere durable to keep one across reboots.

| Variable | Default | What it does |
|---|---|---|
| `HELIOBRIDGE_LISTEN` | `0.0.0.0:7006` | Device-facing TLS listener |
| `HELIOBRIDGE_TLS_CERT` / `_KEY` | generated | Certificate presented to the device |
| `HELIOBRIDGE_STATE_DIR` | `$TMPDIR/heliobridge` | Generated certificate and cached state |
| `HELIOBRIDGE_CONTROL_SOCKET` | off | Unix socket for the control API, mode 0600 |
| `HELIOBRIDGE_ALLOW_FROM` | any | Addresses and networks the device may connect from |
| `HELIOBRIDGE_ALLOW_DEVICES` | any | Device serials to serve |
| `HELIOBRIDGE_SLOTS` | `1` | How many of the nine schedule slots to expose |
| `HELIOBRIDGE_MQTT_URL` | off | Broker to publish to: `mqtt://host[:port]` or `mqtts://host[:port]` |
| `HELIOBRIDGE_MQTT_USER` / `_PASS` | *(unset)* | Broker credentials |
| `HELIOBRIDGE_MQTT_PASS_FILE` | *(unset)* | File holding the password, read at startup. Takes precedence over `_PASS` |
| `HELIOBRIDGE_MQTT_CLIENT_CERT` / `_KEY` | *(unset)* | Client certificate, for a broker that authenticates by one |
| `HELIOBRIDGE_MQTT_BASE` | `heliobridge` | Root of this program's own topics |
| `HELIOBRIDGE_MQTT_DISCOVERY_PREFIX` | `homeassistant` | Root Home Assistant watches for discovery |
| `HELIOBRIDGE_MQTT_INSTANCE` | the host name | Distinguishes this bridge from another on the same broker |
| `HELIOBRIDGE_ALLOW_WRITES` | `true` | `false` publishes every setting as a read-only sensor and refuses every command |
| `HELIOBRIDGE_ALLOW_POWER_PLUS` | `true` | `false` does the same for `power_plus` alone |
| `HELIOBRIDGE_OFFLINE_AFTER` | `30` | Seconds without telemetry before the device is reported absent |
| `HELIOBRIDGE_CLOUD_RELAY` | off | Relay to the vendor cloud |
| `HELIOBRIDGE_RELAY_MODE` | `controls` | How much authority the cloud keeps |
| `HELIOBRIDGE_RELAY_ANSWERS` | `cloud-only` | Which answers to earlier commands reach the cloud |
| `HELIOBRIDGE_FIRMWARE_DIR` | off | Keep firmware the cloud advertises here |
| `HELIOBRIDGE_FETCH_FIRMWARE` | `false` | Download the advertised image, rather than only logging its URL |
| `HELIOBRIDGE_FIRMWARE_MAX_BYTES` | `16777216` | Cap on a single firmware download |
| `HELIOBRIDGE_RECORD_DIR` | off | Record raw frames for analysis |
| `HELIOBRIDGE_LOG` | `info` | Tracing filter, per subsystem |

### Who may connect

Both allowlists are empty by default, and empty admits everything — one device on an isolated VLAN needs
neither. They are comma-separated:

```sh
HELIOBRIDGE_ALLOW_FROM=192.168.2.238,192.168.2.0/24,2001:db8::/32,fe80::/10
HELIOBRIDGE_ALLOW_DEVICES=0EXAMPLE00000001
```

An address that is not allowed is dropped on `accept`, before the TLS handshake. A serial that is not
allowed is answered with a CONNACK refusal at connect, before the session registers — so it never reaches
the control API, never becomes a Home Assistant entity and never has a frame recorded.

Both lists say what is *allowed*, and nothing else is implicit. Listing only IPv4 does not deny IPv6, and
loopback is not admitted unless `127.0.0.1` or `::1` is listed. An entry that cannot be parsed is a startup
failure, because the failure mode of a mistyped list is a device that silently stops connecting.

Neither replaces network isolation. The protocol's credentials are the serial plus a fixed string, so they
identify rather than authenticate, and the serial crosses a connection whose certificate the device does not
verify — anyone positioned to capture one already has it.

A relative `HELIOBRIDGE_MQTT_PASS_FILE` is resolved inside `$CREDENTIALS_DIRECTORY`, which systemd sets
for a unit using `LoadCredential=`:

```ini
LoadCredential=mqtt-pass:/etc/heliobridge/mqtt.pass
Environment=HELIOBRIDGE_MQTT_PASS_FILE=mqtt-pass
```

An absolute path is used as given. Trailing newlines are stripped. A file that cannot be read is a startup
failure.

### Standard variables

| Variable | Default | Effect |
|---|---|---|
| `TZ` | the host's zone | The zone the device's clock is set to |
| `SSL_CERT_FILE` | *(unset)* | A PEM bundle replacing the shipped trust anchors for outbound TLS |
| `SSL_CERT_DIR` | *(unset)* | A directory of them, same effect |

The device is sent local wall time, not UTC, so `TZ` sets the time the device runs on and the times its
schedule slots fire. Set it where the process is defined; a container defaults to `UTC`.

```sh
TZ=Europe/Berlin heliobridge
```

Mozilla's roots ship in the binary. `SSL_CERT_FILE` or `SSL_CERT_DIR` replaces them entirely — use it to
trust a private authority, such as a broker with a self-signed certificate. Naming a store that holds no
usable certificate is a startup failure.

```sh
SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt heliobridge
```

### Relay modes

In every mode the vendor app keeps **displaying** correctly. What differs is what it may change:

- `full` — the app works as if this program were absent, including datalogger configuration. The
  cloud then also owns the clock, and could point the device away from here.
- `controls` (default) — the app still changes slots, output power, charge limits and the switches,
  but not the broker endpoint, DNS, timezone or clock, and nothing unrecognised. The vendor server
  was never observed sending anything outside the permitted set, so this costs no observed
  functionality.
- `observer` — the cloud sees everything and changes nothing. The right choice once settings are
  driven locally, since a second writer is only a way for two pictures to disagree.

Nothing the device sends is ever withheld from the cloud in any mode: a report cannot change the
device's behaviour, and withholding one only makes the app's picture wrong — which matters, because
the app writes whole register ranges back from that picture.

Worth remembering in every mode: "the cloud" is anyone who can reach the vendor broker knowing this
serial.

## Home Assistant

Set `HELIOBRIDGE_MQTT_URL` and the device appears through MQTT autodiscovery. Entities are derived from
the register maps, so a register gaining a name gains an entity.

```
heliobridge/<serial>/state              telemetry, JSON, one publish per cycle
heliobridge/<serial>/settings           holding-register values, retained
heliobridge/<serial>/status             connected, and when the last frame arrived — retained
heliobridge/<serial>/set                commands, JSON  {"slot1_output_power": 100}
heliobridge/<serial>/availability       online | offline — the device
heliobridge/bridge/<instance>/availability   online | offline — this program, as a last will
homeassistant/<component>/heliobridge/<serial>_<field>/config   discovery, retained
```

Each entity reads one field out of the shared object with a `value_template`, so a telemetry cycle is one
publish rather than sixty. Discovery is retained and republished on every broker connection, which makes a
broker restart, a network blip and a first start the same case; an entity that leaves the catalogue is
withdrawn with an empty payload rather than left behind.

**Two availability topics, listed by every reading with `availability_mode: all`.** This program dying is a
last will, which the broker publishes for us. A *device* going away is something only this program can see,
so it says so itself — after `HELIOBRIDGE_OFFLINE_AFTER` seconds without a telemetry frame, since the
device's own MQTT keepalive is 420 s and a half-open connection would otherwise leave stale readings on a
dashboard for seven minutes.

Nothing publishes a substitute value. No zero, no repeat of the last reading: on a `total_increasing` energy
sensor a zero reads as a counter reset and the next real value is counted as a day's worth of new energy, and
a repeated value is a flat line indistinguishable from a real one. The entity goes `unavailable` and Home
Assistant records a gap. Two entities carry only *this program's* availability, so they keep working through
an outage and say how stale everything else is: **Device connected** and **Last update**.

**Commands** arrive as a JSON object on `heliobridge/<serial>/set`, naming a setting and a value — which is
what the discovery messages tell Home Assistant to send, and what `mosquitto_pub` can send by hand:

```json
{"slot1_output_power": 100}
{"grid_power_allowed": 1}
{"slot1_work_mode": "smart_self_use"}
{"slot1_start_time": "23:59"}
{"supplied_meter_reading": -250}
{"withdraw_meter_reading": 1}
```

A command goes through the same allowlist and the same read-back as the control API. The value republished
afterwards is what the device *stored*, which is not always what was asked: it clamps silently rather than
rejecting, so asking for more than a setting's ceiling shows up as the lower figure. A value outside a
register's documented range is refused before anything is sent, and a payload naming something unknown is
logged with the reason rather than being coerced into a register. One bad field refuses the whole payload.

`HELIOBRIDGE_ALLOW_WRITES=false` publishes every setting as a read-only sensor and refuses every command;
`HELIOBRIDGE_ALLOW_POWER_PLUS=false` does the same for that one setting, which stays visible as a sensor.
Both close the entity and the command topic together, so a retained or hand-published command cannot reach
a control that was not offered.

For the Energy dashboard: `pv_energy_total` as solar production, `battery_charge_energy_today` and
`battery_discharge_energy_today` as the battery pair. The grid slots need a meter at the boundary, which this
device does not have: it cannot separate self-consumption from what crossed it. Supplying a reading (below)
does not change that — whatever measures the boundary is already the better source for those slots.

Several devices may share one broker: every device-facing topic and every `unique_id` carries the serial.
`HELIOBRIDGE_MQTT_INSTANCE` distinguishes two *bridges* on one broker, and appears in one topic only — this
program's own availability, where a shared name would make one bridge's shutdown mark another's entities
unavailable.

## Smart self-use

Work mode 2 regulates the inverter's output from a smart meter's reading of the grid connection. The device
accepts that reading written into four holding registers, so it needs no meter of its own:

```console
$ curl --unix-socket /run/heliobridge.sock -X PUT \
    -H 'content-type: application/json' -d '{"watts": 250}' \
    "http://local/devices/$SERIAL/meter-reading"
```

Positive is importing, negative is exporting. `DELETE` on the same path withdraws it. From Home Assistant the
same two operations are the **Supplied meter reading** number and the **Withdraw meter reading** button.

**Every write is a fresh submission, not a stored setting** — which the number does not look like, and one
consequence is worth knowing. Setting it from an automation or from *Developer tools → Actions* submits the
figure every time, whether or not it differs from the last. Typing into the box does not: the frontend keeps
the value it last sent and will not re-submit an unchanged one, so the same reading cannot be supplied twice
by hand that way. To send a figure again, call the action rather than retyping it:

```yaml
action: number.set_value
target:
  entity_id: number.nexa_2000_0example00000001_supplied_meter_reading
data:
  value: -250
```

or publish to the command topic directly with `mqtt.publish`, which is also what an automation should do if
it is already computing the figure.

**The reading is an error signal, not a target.** For each new reading the device adjusts its *own* output by
approximately that amount:

```text
new_output ≈ old_output + 0.75 × reading
```

The factor was measured at 0.68–0.81 for readings of 100–600 W, and closer to one-for-one at 10–20 W. It
matters only if you supply a figure and expect it as an output: with a real meter the shortfall is reported
back on the next reading and corrected then. Supply what a meter at the grid connection would read —
`household load − ac_output_power` — and the output converges on covering the house and keeps tracking it.
Supply a fixed target instead and the output walks in one direction until the battery limits it.

**Supplying the same figure again does nothing.** The device acts on the reading *changing*; a repeat keeps
it from expiring but moves nothing. [docs/output-response.md](docs/output-response.md) has the measured
latencies and what happens when readings stop — which is not what most people expect.

**A reading expires after about two minutes, and nothing here refreshes it.** Whatever holds the measurement
writes it again inside that window; stop writing and the device drops the reading and behaves as though no
meter were present.

**Two entities report what the device makes of it.** `meter_connected` says whether it currently holds a
reading at all — the only way to distinguish a genuine 0 W reading, meaning the grid is balanced, from no
meter, since both read 0 W. `meter_active_power` carries the reading it holds, and goes `unavailable` when it
holds none rather than reading a misleading zero.

Two behaviours worth expecting. Allow **three minutes** after selecting the mode before the output follows —
the vendor's own instructions say the same, and the first reading or two are ignored. And while a slot is in
work mode 2 the device **ignores that slot's `slot{n}_output_power`**; the entity is published `unavailable`
to say so, because the register still stores and reads back whatever is written to it.

⚠ Write a figure you have measured. The device acts on the reading without checking it against anything it
measures itself, so a wrong one is obeyed — an import that no load justifies will discharge the battery to
serve a load that is not there.

## Firmware the cloud advertises

The vendor's cloud advertises a firmware update by writing a URL into datalogger configuration register 80,
about once an hour until the device installs it. The relay policy refuses cloud writes to the configuration
space, so it never reaches the device.

**This needs `HELIOBRIDGE_CLOUD_RELAY`.** An advertisement arrives on the relay's cloud-to-device path;
without a relay the device never hears from the vendor's cloud through this program, so there is nothing to
notice and nothing to fetch.

With a relay, the advertisement is logged in full whether or not anything is kept, URL included:

```
the cloud advertised a firmware update source="configuration register 80"
  url=http://cdn.growatt.com/update/device/GB/manualUpgrade/…/WIFI/4.0.2.6.bin
  file=WIFI-4.0.2.6.bin refused=true fetch=false
```

`HELIOBRIDGE_FIRMWARE_DIR` keeps the image as well, and `HELIOBRIDGE_FETCH_FIRMWARE=true` downloads it.
Fetching is off by default: an advertisement is traffic that arrives anyway, while downloading reaches out
to a vendor host. An image already on disk is left alone, so an hourly campaign costs one download. The
transfer is capped, is written under a temporary name and renamed once complete, and its SHA-256 is logged
so the file can be compared with an image already held.

The request presents the same user agent and cache directive the datalogger's own firmware sends, and
nothing else — this program does not announce itself to the vendor's CDN.

**Nothing installs firmware.** The image is stored and that is all.

## Control API

HTTP over the Unix socket, so `curl --unix-socket` is the whole client. Errors are
`application/problem+json`.

```
GET    /healthz
GET    /devices                              connected devices
GET    /devices/{device}                     summary: model, firmware, endpoint, clock skew
GET    /devices/{device}/identity            the datalogger's self-report
GET    /devices/{device}/telemetry           every decoded input register
GET    /devices/{device}/telemetry/{key}
GET    /devices/{device}/settings            cached settings
GET    /devices/{device}/settings/{key}
PUT    /devices/{device}/settings/{key}      write, then read back to confirm
POST   /devices/{device}/settings/{key}/read refresh from the device
GET    /devices/{device}/config/{key}        datalogger configuration
PUT    /devices/{device}/config/{key}        write one config register
POST   /devices/{device}/config/{key}/read
POST   /devices/{device}/config/read         ?registers=a,b,c or ?all — streamed
GET    /devices/{device}/actions
POST   /devices/{device}/actions/{key}       restart the datalogger
PUT    /devices/{device}/meter-reading       supply a meter reading: {"watts": <signed>}
DELETE /devices/{device}/meter-reading       withdraw it
GET    /meter                                the simulated meter: what it reports, and polls served
PUT    /meter                                start answering polls: {"watts": <number>}
POST   /meter                                report a different figure, without starting it
DELETE /meter                                stop answering polls
```

A supplied meter reading expires after about two minutes and nothing here refreshes it, so a caller that
wants one to persist writes it again inside that window. The device reports what it holds as
`meter_active_power`, and `meter_connected` says whether it holds one at all.

A `{key}` is a field name or a register number. The config space is 146 registers, 0 to 145; the device
volunteers 32 of them on connect and answers the rest only when asked.

### Reading configuration in bulk

`config/read` answers `application/jsonl` — one JSON object per register as the device answers for it, then
a final summary line. Name the registers with `?registers=`, or the whole space with `?all`; one or the
other, not both. `?batch=N` sets how many registers go in each request frame.

```console
$ curl -N --unix-socket /run/heliobridge.sock -X POST \
    "http://local/devices/$SERIAL/config/read?registers=update_url,sdk_version,76"
{"register":76,"name":"wifi_signal","role":"dynamic","value":"-63"}
{"register":80,"name":"update_url","role":"dynamic","value":"http://cdn.growatt.com/update/…"}
{"register":61,"name":"sdk_version","role":"metadata","value":"IDFSDK:v4.4.3"}
{"requested":3,"answered":3,"silent":[]}
```

Answers arrive out of order and tens of seconds behind the request, which is why the response streams.
Reading the whole space takes about 37 s at `batch=1` and about 19 s at `batch=8`. Closing the connection
stops the reading.

`silent` lists registers that answered nothing; some are simply unpopulated. `role` is `identity` for
fields that carry the serial, the Wi-Fi passphrase or the Bluetooth handshake key.

## Running in a container

`ghcr.io/longsleep/heliobridge`, for `linux/amd64` and `linux/arm64`. Each release is tagged three ways:
the full version, `major.minor`, and `latest`, which points at the highest release. Pin a full version in
anything you deploy.

The device connects to port 7006, and the container needs one writable volume for the state directory.

```sh
podman run -d --name heliobridge \
  -p 7006:7006 \
  -v heliobridge-state:/state \
  -e HELIOBRIDGE_STATE_DIR=/state \
  -e HELIOBRIDGE_MQTT_URL=mqtt://broker.lan:1883 \
  -e HELIOBRIDGE_CONTROL_SOCKET=/state/control.sock \
  -e HELIOBRIDGE_RECORD_DIR= \
  -e HELIOBRIDGE_CLOUD_RELAY=false \
  -e HELIOBRIDGE_RELAY_MODE=controls \
  --read-only --cap-drop ALL --security-opt no-new-privileges \
  ghcr.io/longsleep/heliobridge:latest
```

`docker run` takes the same arguments. Under rootless Podman add `--userns=keep-id` if the volume is a
host path rather than a named volume, so the uid inside matches the owner outside.

The volume must be writable by uid **65532**, which the image runs as. A named volume inherits that on
first use; a host path needs `chown 65532:65532`.

### Compose

```yaml
services:
  heliobridge:
    image: ghcr.io/longsleep/heliobridge:latest
    restart: unless-stopped
    ports:
      - "7006:7006"
    environment:
      # Holds the generated certificate, and is the only path written to.
      HELIOBRIDGE_STATE_DIR: /state
      HELIOBRIDGE_MQTT_URL: mqtt://broker.lan:1883
      # Placed in the state volume, which is already mounted; replaced on each start. Omit the
      # variable to run without the control API.
      HELIOBRIDGE_CONTROL_SOCKET: /state/control.sock
      # Empty turns raw frame recording off. It writes about 10 MB a day, so name a path under a
      # volume only while diagnosing something.
      HELIOBRIDGE_RECORD_DIR: ""
      # Whether to also connect to the vendor cloud.
      HELIOBRIDGE_CLOUD_RELAY: "false"
      # What the cloud may change while relaying: full, controls, or observer.
      HELIOBRIDGE_RELAY_MODE: controls
      HELIOBRIDGE_LOG: info
    volumes:
      - state:/state
    read_only: true
    cap_drop:
      - ALL
    security_opt:
      - no-new-privileges:true

volumes:
  state:
```

Works with `docker compose` and `podman-compose`.

### Reaching the control API

The control API is a Unix socket, so it is not published as a port. In the examples above it sits in the
state volume, and is reached from the host through that volume's path:

```sh
curl --unix-socket /var/lib/docker/volumes/heliobridge_state/_data/control.sock \
     http://localhost/devices
```

Or from another container that mounts the same volume. There is no shell to `exec` into.

### Health

`GET /healthz` on the device-facing port answers `ok` when the server is serving, and is accepted only
from loopback:

```sh
curl http://127.0.0.1:7006/healthz
```

`heliobridge healthz` asks the same question and exits 0 or 1, which is what the image's `HEALTHCHECK`
runs. It reports whether the server is serving — not whether a device is connected, which is expected to
be absent for hours at a time.

### Relaying to the vendor cloud

Off by default in a container as everywhere else. `HELIOBRIDGE_CLOUD_RELAY=true` needs outbound TCP to
`mqtt.growatt.com:7006` and no further setup. To trust a private authority for your own *broker*, mount
it and set `SSL_CERT_FILE`.

## Building for another machine

`cargo build --release` links against the build host's glibc, so the result will not start on a distribution
older than that host. The release targets are **musl** instead, which links statically: one binary per
architecture, running on any Linux of that architecture.

```sh
cargo install cargo-zigbuild
pip install ziglang        # or Zig from ziglang.org, or a package manager

cargo zigbuild --release --target x86_64-unknown-linux-musl
cargo zigbuild --release --target aarch64-unknown-linux-musl
```

Both targets install with the toolchain, and the command is the same for either — including the host's own
architecture, so there is one recipe rather than one per machine.

Zig is there because the crypto provider that rustls and rcgen pull in compiles C and assembly, which a
cross build needs a C toolchain for. Zig ships a complete one for every target and `cargo-zigbuild` puts it
where cargo expects a linker; the alternative is a separate C cross compiler per architecture, and nothing
packages one for aarch64-musl. Installed as a Python package, Zig has no `zig` executable, so
`cargo-zigbuild` finds it through `python3 -m ziglang` — put the environment holding it on `PATH`.

## Design

- Library plus thin binary. The protocol layer is pure `bytes → values` with no I/O, so it is
  tested against recorded frames rather than against hardware.
- `#![forbid(unsafe_code)]`, edition 2024, and lints that deny `unwrap`, `expect`, slice indexing
  and unchecked arithmetic in the library.
- One seam between the server and the manufacturer: framing, decoding, commands, the register
  catalogue, the relay policy, the cloud endpoint and firmware are each a trait the server owns and a
  driver implements. Only the binary names Growatt, and a test fails the build if anything else does.

## Safety

Writing to registers this device does not document is **not** safe. Vendor guidance is explicit that
bypassing the AC charging controller's limits risks thermal runaway. Heliobridge restricts writes to
an allowlist of settings the vendor app itself exposes, and that restriction is structural — an
allowlist expressed as a type — rather than a runtime check.

The datalogger's config space is treated more cautiously still: the endpoint registers can be read
but are not exposed for writing, because a wrong value there strands the device somewhere only
Bluetooth can reach it.

## Related projects

Two existing projects with overlapping goals, both worth looking at first — either may suit you
better than this one:

- **[GroBro](https://github.com/robertzaage/GroBro)** — an MQTT bridge for Growatt NOAH and NEXA
  devices with Home Assistant autodiscovery, including local-only operation.
- **[nexa-mqtt](https://github.com/mgerczuk/nexa-mqtt)** — bridges the Nexa into Home Assistant
  through Growatt's **cloud** API. Different trade-off: it needs the vendor cloud and an account,
  but it needs nothing on your network.

## Licence

Licensed under the Apache License, Version 2.0 — see [LICENSE](LICENSE).

Copyright 2026 Simon Eisenmann. See [NOTICE](NOTICE).

