# How quickly the device follows a command

If you want the device to hold your grid connection near zero — cover the house, import nothing, export
nothing — the first question is how fast and how faithfully it can be driven. This is that, measured.

**How it was driven and watched.** Every command went out through **Heliobridge's control API**, and the
device's own readings came back through its telemetry — no vendor cloud in either direction, so these are
the numbers you get driving the device the way this program drives it. Telemetry arrives once every five
seconds, which is too coarse for most of the timings below, so the response was watched on an **independent
watt-meter** as well: an ESPHome smart plug between the device and the socket, sampling about once a
second. The timings come from that meter; the output figures are quoted from the device's own reading,
which the meter confirms to about 1 %.

| | |
|---|---|
| Device | Growatt Nexa 2000, model `GTSW0000`, hardware `V1.0` |
| Datalogger firmware | `4.0.1.9` |
| Driven by | Heliobridge 0.5.0, over its control API |
| Watched by | the device's own telemetry (5 s) and an ESPHome plug (~1 s) |
| Conditions | after dark, no PV, battery from ~50 % to ~25 %, nothing else writing |

One device, one firmware. Treat the exact figures as indicative and the shapes as the point.
[Reproduce this yourself](#reproduce-this-yourself) at the end says how to check any of it.

There are two ways to set the output, and they behave nothing alike. **One is a target the device tracks.
The other is an error signal it adds to whatever it is already doing.**

## The two levers at a glance

| | **Commanded output** | **Supplied meter reading** |
|---|---|---|
| Setting | `slot1_output_power` | `/meter-reading` (four holding registers) |
| Slot work mode | `load_first` | `smart_self_use` |
| Means | "produce 400 W" | "the house is importing 400 W" |
| You get | 400 W | *current output* + about 300 W |
| Latency, device already running | **0.3–4 s** to first movement, settled within 10 s | 2–3 s to first movement |
| Latency, device idle | **38–150 s** | — |
| Accuracy | within 1 W of the figure written | about ¾ of the figure supplied |
| Persistence | holds until written again | expires after ~2 minutes |
| Writing the same value again | no effect (already there) | **no effect at all** |
| Ceiling observed | 800 W | 800 W |

**For zero-grid operation, use the commanded output.** Measure the house yourself, work out what the device
should produce, and write that number. The supplied reading exists to emulate a meter the device would
otherwise poll, and it is a poor way to command a setpoint — the reasons are below.

## Lever 1 — commanded output

```console
$ curl --unix-socket /run/heliobridge.sock -X PUT \
    -H 'content-type: application/json' -d '{"value": 400}' \
    "http://local/devices/$SERIAL/settings/slot1_output_power"
```

From Home Assistant this is the **Slot 1 output power** number. The slot must be in `load_first`; in
`smart_self_use` the value is stored, read back, and ignored.

**Latency, once the device is producing.** Twenty-one steps between 150 W and 800 W:

| | Time to first movement | Time to settle |
|---|---|---|
| Increasing output | median 3.2 s (0.3–4.1) | median 6.9 s (4.1–9.3) |
| Decreasing output | median 0.3 s (0.2–9.3) | median 2.2 s (0.2–9.3) |

Nothing took longer than 9.3 s to settle. **Reductions are about three times faster than increases**, which
is the useful asymmetry: the device sheds output promptly and adds it gently.

**Accuracy.** It produces what it is told. Commanded 150 W, it reported 149–150; commanded 400, 399–400;
commanded 600, 599–600; commanded 800, 799. Returning to a figure landed within 1.4 W of the previous
visit, so it is repeatable as well as accurate. An external watt-meter agreed to within about 1 %.

**From a standstill it is slow, and unpredictably so.** With the device producing nothing, a 400 W command
took **38 s** to do anything and a 150 W command took **150 s**. Both are an order of magnitude beyond the
warm figures. If you have just enabled the slot, or the device has been idle, **allow three minutes before
concluding a command was ignored** — and do not design a control loop that expects its first correction to
land in seconds.

**Ceiling.** 800 W was accepted and delivered with `power_plus` clear.

## Lever 2 — supplied meter reading

```console
$ curl --unix-socket /run/heliobridge.sock -X PUT \
    -H 'content-type: application/json' -d '{"watts": 250}' \
    "http://local/devices/$SERIAL/meter-reading"
```

Positive is importing, negative is exporting. This is the reading the device would otherwise get from a
meter of its own, and it regulates from it.

**Order matters when switching modes.** The device **refuses** to enter `smart_self_use` unless a reading
is already live. Supply one first — 0 W will do — and then set the mode, or the write is rejected and the
slot quietly stays where it was.

**It adds, it does not set.** For each new reading:

```text
new_output ≈ old_output + 0.75 × reading
```

Measured over eight steps of 100–600 W in both directions, the factor ran 0.68–0.81. Smaller corrections of
10–20 W have been seen to apply roughly one-for-one, so the shortfall matters most at the magnitudes you
would use to command a setpoint. With a real meter it is invisible: whatever is left over is reported on
the next reading and corrected then.

**Only a *change* acts.** Supplying the same figure again, inside the reading's lifetime, does nothing —
repeats moved the output by less than a watt-meter's noise. Re-sending keeps the reading from expiring; it
does not accumulate.

**Latency** is 2–3 s to first movement, the same order as the commanded lever.

**Ceiling.** 600 W supplied to a device already producing 369 W gave 799 W, not the ~820 W the factor
predicts.

### What happens when you stop supplying

Three behaviours, and the third surprises people:

1. Output **holds** for the reading's lifetime — about two minutes from the last write.
2. Then it drops to **`default_output_power`** and stays there. **Not to zero.** Whatever that setting says
   is what the device produces indefinitely once it believes it has no meter.
3. The next reading of **any value, including zero**, restores the output it was producing before the lapse.
   A device sitting at its 100 W default was sent a single `0 W` reading — "the grid is balanced, nothing to
   do" — and went back to 799 W within twenty seconds.

A reading after a lapse does both at once: from a lapsed 179 W, supplying 200 W produced 330 W — the
remembered 179 W, then the correction on top.

So **withholding readings is not a way to stop the device**, and there is no such thing as a harmless zero.
To park the output somewhere specific, set that level with the commanded lever and leave smart self-use.

## Closing the loop for zero-grid operation

The device is not the slow part. A practical loop looks like this:

| Stage | Cost |
|---|---|
| Your meter reports | its own interval — 1 s for most smart plugs and CT clamps |
| You compute and write | milliseconds over the control API, one MQTT publish from Home Assistant |
| Device begins to move | 0.3–4 s |
| Device settles | under 10 s |
| You see it in telemetry | the device reports every 5 s |

**A loop period of 5–10 seconds is comfortable**, and the limit on how closely the grid sits at zero is
your meter's interval and how abruptly your house load changes — not the inverter. A kettle switching on is
covered within about ten seconds; the energy the grid supplies in the meantime is the cost of the loop
period, not of the device being slow.

Two ways to run it:

- **Command the output (recommended).** Keep the slot in `load_first` and write
  `slot1_output_power = house load − PV going to the house`, or simply the previous output plus whatever
  the grid is importing. You get exactly what you ask for, it stays there if your loop dies, and nothing
  expires. The device's own reported `ac_power` closes the loop for you.
- **Emulate the meter.** Keep the slot in `smart_self_use` and supply `house load − device AC output`,
  which is what a meter at the grid connection reads. Supply it again whenever it *changes*, and at least
  once every two minutes so it does not lapse. Do not supply a fixed target: the device cannot discover it
  has overshot, and it will walk until the battery stops it.

**What limits it.** The ceiling is 800 W unless `power_plus` is set, so a house drawing more than that
imports the difference whatever you do. And the first watts after an idle period take up to two and a half
minutes, so a loop that lets the output fall to zero overnight will look broken for the first few minutes
of the morning.

## How this was measured

Twenty-one commanded steps of 150–800 W with 90-second holds, three times over, and nine supplied readings
of 100–600 W in both directions, each held for 150 seconds with the value repeated every 30 seconds — the
repeats being how "only a change acts" was established.

The watt-meter was calibrated against the device's own reported power across twenty steady plateaus
spanning 150–800 W. The fit is a straight line with a worst residual of 2.3 W: the two instruments agree to
about 1 % plus a 6 W offset. That offset is why the accuracy figures above are quoted from the device — an
apparent 5 W error at 150 W turned out to be the meter, not the inverter.

## What is not established

- **The exact shape of the sub-unity factor** on supplied readings: three-quarters at 100–600 W and roughly
  one-for-one at 10–20 W are both observed. It does not matter for meter emulation, where the residual is
  corrected on the next reading.
- **Why a cold start takes 38 s in one case and 150 s in another.** Two observations, no pattern.
- **Whether the remembered working point survives a device restart**, or only a lapsed reading.
- **Behaviour with PV present**, and above 800 W with `power_plus` set. Everything here was measured after
  dark, below the ceiling.

## Reproduce this yourself

Nothing here needs special tooling. The device's own telemetry shows every effect described above; an
external watt-meter only adds sub-second timing and an independent check that the device's reported figure
is true.

**Before you start.** Measure after dark, or PV will move the output while you are attributing it to your
commands. Disable anything else that writes to the device — an automation adjusting output power will
fight you and the device will look like it is disobeying. Leave charge to spare: these programmes discharge
the battery, roughly 200 Wh for the commanded-output sweep below.

Two shell helpers, and everything else is one line each:

```console
$ SERIAL=0EXAMPLE00000001; SOCK=/run/heliobridge.sock
$ ac()  { curl -s --unix-socket $SOCK "http://local/devices/$SERIAL/telemetry" \
            | jq -r '.readings[] | select(.name=="ac_power") | .value'; }
$ put() { curl -s --unix-socket $SOCK -X PUT -H 'content-type: application/json' \
            -d "{\"value\": $2}" "http://local/devices/$SERIAL/settings/$1" | jq -c '{value, confirmed}'; }
$ meter() { curl -s --unix-socket $SOCK -X PUT -H 'content-type: application/json' \
            -d "{\"watts\": $1}" "http://local/devices/$SERIAL/meter-reading" >/dev/null; }
```

`ac_power` is negative when the device is producing.

**Latency of a commanded output.** Put the slot in `load_first`, then step it and watch. Give the first
step three minutes — from a standstill the device is slow, which is the single most misleading thing about
it:

```console
$ put slot1_work_mode 0
$ for w in 150 400 150 600 300; do
    put slot1_output_power $w
    for i in $(seq 8); do sleep 5; echo "$w -> $(ac)"; done
  done
```

You should see each new figure reached by the second or third reading, and the value land within a watt or
so of what you asked for.

**The meter lever, and why repeats do nothing.** Supply a reading *before* selecting the mode — the mode
write is refused otherwise:

```console
$ meter 0; sleep 2; put slot1_work_mode 2      # refused if no reading is live
$ ac                                          # note the level
$ meter 150; sleep 20; ac                     # rose by about 0.75 × 150
$ meter 150; sleep 20; ac                     # unchanged: a repeat is not a change
$ meter -100; sleep 20; ac                    # fell by about 0.75 × 100
```

**What happens when you stop.** The behaviour most worth seeing for yourself:

```console
$ sleep 150; ac      # the reading lapsed: now at default_output_power, not zero
$ meter 0; sleep 20; ac  # a zero reading restores what it was producing before the lapse
```

**Put it back when you are done**, or the device keeps doing whatever you left it doing:

```console
$ curl -s --unix-socket $SOCK -X DELETE "http://local/devices/$SERIAL/meter-reading"
$ put slot1_work_mode 0        # or whichever mode you normally run
$ put slot1_output_power 100   # or whatever it was before you started
```

From Home Assistant the same experiments are the **Slot 1 work mode** select, the **Slot 1 output power**
number, and the **Supplied meter reading** number — with the caveat in the README that the number box will
not re-submit an unchanged value, so a repeat has to go through `number.set_value` or the command topic.
