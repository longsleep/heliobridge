# Heliobridge

A local MQTT server for the **Growatt Nexa 2000** balcony storage system: run it instead of the
vendor cloud, and keep the device working on your own network.

> **A personal weekend project, built with heavy AI assistance.** It runs against exactly one device
> — the author's — on a home network, and it is written to be honest about what has actually been
> observed rather than to be a product. Treat it accordingly. It is not affiliated with or endorsed
> by Growatt.

## Status

The device talks to it. What is missing is the last hop to Home Assistant.

**Working, verified against real hardware:**

- **Is the MQTT server the device connects to.** TLS on port 7006, MQTT 3.1.1, taking the device's
  identity from its CONNECT — the credentials it sends are its serial and a firmware constant, so
  they identify rather than authenticate. No separate broker to run: this is the design decision the
  rest follows from.
- **Decodes the protocol.** Obfuscated legacy Growatt framing, CRC-16/MODBUS, the input register
  map for telemetry, the holding register map for settings, and the datalogger's own config space.
  Records the device replays from its internal archive after a reconnect are decoded and logged but
  never treated as current, since they can be over an hour old.
- **Serves everything over a control API** on a Unix socket — telemetry, settings, identity,
  datalogger configuration — and accepts writes.
- **Writes settings with read-back confirmation**, from an allowlist, because the device silently
  clamps out-of-range values rather than rejecting them and acknowledges a range write without
  saying what it stored.
- **Sets the device's clock**, which is otherwise the vendor server's job.
- **Optionally relays to the vendor cloud**, so the phone app keeps working — with a policy
  deciding how much authority the cloud keeps.
- **Records raw frames** for later analysis, including the ones the relay policy refused.

**Not done yet:** publishing to your broker with Home Assistant autodiscovery. That is the headline
feature and it is the next piece of work. Until then this is a decoder and a control API, not a
Home Assistant integration.

Also unimplemented: **retargeting the device's broker endpoint** by writing its config registers,
which would remove the need for the DNS override below. The protocol for it is understood; it stays
unimplemented because a wrong value there has no remote recovery.

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

| Variable | Default | What it does |
|---|---|---|
| `HELIOBRIDGE_LISTEN` | `0.0.0.0:7006` | Device-facing TLS listener |
| `HELIOBRIDGE_TLS_CERT` / `_KEY` | generated | Certificate presented to the device |
| `HELIOBRIDGE_STATE_DIR` | `/var/lib/heliobridge` | Generated certificate and cached state |
| `HELIOBRIDGE_CONTROL_SOCKET` | off | Unix socket for the control API, mode 0600 |
| `HELIOBRIDGE_SLOTS` | `1` | How many of the nine schedule slots to expose |
| `HELIOBRIDGE_CLOUD_RELAY` | off | Relay to the vendor cloud |
| `HELIOBRIDGE_RELAY_MODE` | `controls` | How much authority the cloud keeps |
| `HELIOBRIDGE_RELAY_ANSWERS` | `cloud-only` | Which answers to earlier commands reach the cloud |
| `HELIOBRIDGE_RECORD_DIR` | off | Record raw frames for analysis |
| `HELIOBRIDGE_LOG` | `info` | Tracing filter, per subsystem |

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

## Control API

HTTP over the Unix socket, so `curl --unix-socket` is the whole client. Errors are
`application/problem+json`.

```
GET  /healthz
GET  /devices                                  connected devices
GET  /devices/{device}                         summary: model, firmware, endpoint, clock skew
GET  /devices/{device}/identity                the datalogger's self-report
GET  /devices/{device}/telemetry               every decoded input register
GET  /devices/{device}/telemetry/{key}
GET  /devices/{device}/settings                cached settings
GET  /devices/{device}/settings/{key}
PUT  /devices/{device}/settings/{key}          write, then read back to confirm
POST /devices/{device}/settings/{key}/read     refresh from the device
GET  /devices/{device}/config/{key}            datalogger configuration
POST /devices/{device}/config/{key}/read
GET  /devices/{device}/actions
POST /devices/{device}/actions/{key}           restart the datalogger, clear its log
```

## Design

- Library plus thin binary. The protocol layer is pure `bytes → values` with no I/O, so it is
  tested against recorded frames rather than against hardware.
- `#![forbid(unsafe_code)]`, edition 2024, and lints that deny `unwrap`, `expect`, slice indexing
  and unchecked arithmetic in the library.
- Vendor- and generation-neutral seams: the relay policy speaks in intents, and the Growatt
  generation-7 codec translates into them.

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
