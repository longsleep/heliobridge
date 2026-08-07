# Heliobridge

A local MQTT bridge for the **Growatt Nexa 2000** balcony storage system: run it instead of the
vendor cloud, and get the device into Home Assistant over your own broker.

> **Status: 0.0.1 is a placeholder.** The wire protocol has been reverse engineered and written up
> as a specification, but the implementation is not published yet. There is nothing usable here.

## What it will do

The Nexa 2000's datalogger speaks MQTT over TLS to a fixed cloud endpoint. It performs no
certificate validation, which means a local service can stand in for that endpoint with a DNS
override and no change to the device.

Heliobridge is that service:

- **Is the MQTT server the device connects to.** No separate TLS-enabled broker to run — this is
  the design decision the rest follows from.
- **Decodes the protocol** — obfuscated legacy Growatt framing, CRC-16/MODBUS, register maps for
  telemetry and settings.
- **Publishes to your existing broker** with Home Assistant autodiscovery: telemetry as sensors,
  every documented setting as a read/write entity.
- **Accepts commands back**, with read-back confirmation, because the device silently clamps
  writes rather than rejecting them.
- **Optionally relays to the vendor cloud**, so the phone app keeps working if you want it to.

One binary, environment-variable configuration, `#![forbid(unsafe_code)]`.

## Design

- Single statically-configured binary; all configuration via `HELIOBRIDGE_*` environment variables.
- Library plus thin binary: the protocol layer is pure `bytes → values`, with no I/O, so it is
  testable against recorded frames without a device.
- Per-subsystem log control through `HELIOBRIDGE_LOG`, with module paths as tracing targets.

## Safety

Writing to registers this device does not document is **not** safe. Vendor guidance is explicit
that bypassing the AC charging controller's limits risks thermal runaway. Heliobridge restricts
writes to an allowlist of settings the vendor app itself exposes, and that restriction is
structural rather than a runtime check.

## Licence

Licensed under the Apache License, Version 2.0 — see [LICENSE](LICENSE).

Copyright 2026 Simon Eisenmann. See [NOTICE](NOTICE).
