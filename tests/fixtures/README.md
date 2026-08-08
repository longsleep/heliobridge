# Test fixtures

Real frames from a Growatt Nexa 2000, captured on the author's own network and **redacted** before
being committed here.

## Redaction

The device serial appears **three times** in a telemetry frame, and all three are replaced with the
placeholder `0EXAMPLE00000001`:

| Offset | Field |
|---|---|
| 8 | Device ID |
| 38 | Device ID, second copy |
| 121 | input registers 21–28, `serial_number_part_1..4` |

Redacting only the header field is the obvious mistake — it leaves an intact copy at offset 121 that
nothing about the frame's appearance would reveal.

The CRC is **recomputed** over the re-obfuscated result, so every fixture still validates as a
genuine frame and is usable as a decoder input. Each was checked before being written: no occurrence
of the real serial or any 8-octet substring of it, correct placeholder count, `Length = total − 8`,
and a valid CRC-16/MODBUS.

Nothing else in a telemetry frame identifies the device. The remaining contents are register values —
instantaneous power, state of charge, temperatures, energy counters — plus a timestamp.

**The identity report needs more than the serial removed.** It carries a password field and a MAC-shaped
constant as well, so the values of config registers 7 and 16 are overwritten with `X`. The length is kept
identical, which matters: every TLV offset, every declared length and the frame length stay exactly what the
device sent, so the fixture still tests the layout rather than a rebuilt approximation of it.

Redaction is a property of **these files**, not of the software. `heliobridge` reports every field it
decodes, the serial included — it runs on the owner's own machine, against their own device, behind a Unix
socket whose routes already name the serial. What must not happen is a captured frame reaching version
control unchanged.

## The fixtures

All four are function `0x04` telemetry, 585 octets, sampled across a single capture so that they
disagree with each other. Consecutive frames are five seconds apart and nearly identical, which
would make for a corpus that tests very little.

| File | Timestamp | PV | AC | SoC | Battery | State |
|---|---|---|---|---|---|---|
| `telemetry-night-discharge.bin` | 2026-08-06 23:42:45 | 0 W | −49 W | 7 % | −49 W | discharging, nearly empty |
| `telemetry-midday-charge.bin` | 2026-08-07 12:12:02 | 413 W | −49 W | 98 % | +364 W | charging, nearly full |
| `telemetry-dusk-low-pv.bin` | 2026-08-07 17:19:20 | 57 W | −100 W | 90 % | −43 W | discharging while PV still contributes |
| `telemetry-evening-discharge.bin` | 2026-08-07 22:39:51 | 0 W | −99 W | 56 % | −99 W | discharging, mid charge |

Between them they cover both battery directions, the charging and discharging status enum values, PV
from zero to a few hundred watts, and state of charge from 7 % to 98 %.

`telemetry-midday-charge.bin` is the only one with a **positive** battery power and
`battery_charge_status = 1`. It is the fixture that catches a sign error in the `delta = −30000`
encoding, which every other frame here would pass.

All four satisfy the derived-value rule `battery_charge_power == pv_power_total − |ac_power|`.

A fifth telemetry frame is here for a different reason:

| File | Timestamp | Function | Why |
|---|---|---|---|
| `telemetry-buffered-replay.bin` | 2026-08-06 23:41:35 | `0x50` | A record the device held in its archive and replayed on connect |

It is the same 585-octet record as the four above and decodes with the same map, but it was sampled
**70 seconds before** `telemetry-night-discharge.bin` and delivered alongside it. That is the whole
point of the fixture: only the embedded timestamp distinguishes a replayed sample from a current one,
and a decoder that merges it into live state publishes stale readings after every reconnect.

## Server-originated frames

Frames the **vendor server** sent to the device, used to check that the encoder produces byte-identical
output. These carry the serial once, in the device ID field.

| File | Octets | Command |
|---|---|---|
| `write-single-grid-power-allowed.bin` | 44 | `0x06`, register 326 = 1 |
| `write-range-charge-limits.bin` | 48 | `0x10`, registers 250–251 = 100, 5 |
| `write-range-default-output-power.bin` | 48 | `0x10`, registers **321–322** = 0, 1000 |
| `write-range-slot1.bin` | 54 | `0x10`, registers 254–258 — a whole schedule slot |
| `time-push.bin` | 67 | `0xFE18`, the server's clock as ASCII |

`write-range-default-output-power.bin` is the most valuable of these. The vendor does not write
`default_output_power` as itself: it writes a **range covering register 321**, whose meaning is unknown,
with a zero in it. An encoder that writes register 322 alone produces a different frame, and only a
comparison against a vendor-generated frame catches that.

`time-push.bin` earned its place the same way, before its body was understood. Its leading eight octets were
reproduced as an opaque constant and verified only by byte equality; they are now known to be a config-space
TLV — entry count, entry length, register 31, value length — which is how the clock turned out to be an
instance of a config write rather than a message of its own. The byte-equality test kept working across that
explanation, which is the point of writing it that way.

## Device identity

| File | Octets | Contents |
|---|---|---|
| `identity-report-32-entries.bin` | 401 | `0xFE19`, the config registers as a TLV list: 32 entries in 361 octets |

The frame the device sends on every connect. Its body is a 2-octet entry count, one pad octet, then entries
of `register(2) length(2) value`, and the values are **ASCII** whatever the field means — the MQTT port
arrives as `"7006"`.

Worth having as a fixture for two reasons. It pins the layout: 32 declared entries consume the body exactly,
so an off-by-one in the preamble or the entry header shows up as a leftover rather than as a plausible parse.
And it is the only frame carrying the firmware version, the model and the endpoint the device believes it
should dial — the last of which is how a broker retarget would ever be confirmed.

The specification called the leading `0x0020` a subtype meaning "full configuration". It is not: it is the
entry count, which a single-entry report (`0x0001`) makes obvious. There is one message shape, not two.

Byte equality against these is a stronger claim than any self-consistency check: it says the encoder
agrees with the server it replaces, not merely with itself.

## Regenerating

Produced by `tools/make_fixture.py` in the research repository, which is where the raw captures live.
Regeneration is **not byte-reproducible**: the tool samples evenly across a capture that was still
growing, so a later run picks different frames. Treat these files as the evidence, not as an
intermediate artifact — if a test needs different conditions, add a fixture rather than regenerating
the set.
