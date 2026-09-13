# When the device sleeps, and what that looks like from here

The Nexa 2000 powers itself down most nights and brings itself back at first light. Nothing commands it:
there is no power-off in the datalogger's firmware, none in the vendor's cloud API, and none in this
program. What exists is a condition the device tests for itself, and this is what it is.

Worth knowing because a sleeping device is indistinguishable from a broken one for the first ten minutes,
and because the condition is easy to hold open by accident.

## The condition

Read out of the power controller's firmware, and confirmed against the device:

```c
if ((( battery_asks_to_power_off || state_of_charge <= charge_limit_lower )
      && solar_input < 25 )
    && inverter_input < 250 ) {
    if (6000 < ++counter) { …power down… }
} else { counter = 0; }
```

Four tests, and all four must hold:

| Test | In this program's names |
|---|---|
| The battery asks to power off, **or** charge is at or below the floor | `battery_soc_total` against the `charge_limit_lower` setting |
| Solar input below 25 | `pv_power_total` |
| Inverter input below 250 | not published; the device's own internal reading |
| …held without interruption for 6000 ticks | about **ten minutes** |

**The count resets to zero on any single tick where a test fails.** That is the part that surprises people.
It is not "ten minutes after the conditions were first met"; it is ten unbroken minutes. A sun that
flickers across 25 W at dusk restarts the clock from the beginning, every time.

That explains the spread. Across thirty days of recordings, eighteen of twenty-six shutdowns came ten
minutes after the last change in charge, and seven took between 31 and 87 minutes. Those are runs that were
interrupted once or twice, not a different timer.

**A second, much longer path exists** with the same power tests, no charge test at all, and an additional
requirement that the off-grid socket is drawing nothing. It counts sixteen unbroken hours. Sixteen hours
with no sun, no inverter activity and an idle socket is not a night — it is a unit in storage.

**One thing suppresses both**: a command in flight to a second, stacked unit clears the counters outright.
On a single unit this does not arise.

## What it costs, and what it does not

**Nothing measurable.** Ten hours asleep at 9 % cost two millivolts of cell voltage and no change in the
reported percentage. The cells are nowhere near empty when it happens either: a reported 0 % is the floor
you configured, and cells sit around 3.22 V, on the flat part of the curve.

**Sitting awake is the expensive state.** A device above the floor with nothing to do draws about 13.9 W,
which is roughly an hour and a half per percentage point. Reaching the floor is the cheap outcome.

## Waking

By light, not by clock. Across twenty-six shutdowns the device came back between 04:25 and 06:11 UTC, and
the time drifted about 48 minutes later over 29 days, against sunrise's 45. Adjacent days settle it: a
clear morning woke it at 05:04, the overcast day after at 06:19, the brighter day after that at 05:17.

**There is no way to wake it.** A command has to reach a listening device, and nothing is listening.

## What you will see from Heliobridge

**No disconnection.** The device does not close its MQTT session; the socket is simply left half-open. This
program reaps it on the keepalive:

```text
17:45:03Z  telemetry                    (the last frame)
17:45:33Z  no telemetry; reporting the device absent
17:55:33Z  session failed reason=no packet for 630s
17:55:33Z  device session ended
```

630 s is 1.5 × the negotiated 420 s keepalive. **For those ten minutes there is a session that looks live
and accepts writes that go nowhere.** Read-backs fail, which is how a write to a sleeping device announces
itself.

Entities go `unavailable` rather than flat-lining, because the telemetry watchdog reports the device absent
30 s after the last frame. Nothing publishes a substitute value.

## Using it deliberately

The condition can be met on purpose, which is the only way to park the device with charge still in it:
**once solar has reached zero, write `charge_limit_lower` up to the current state of charge.** It sleeps
within ten to forty minutes, holds the charge at no measurable cost, and wakes itself in the morning.

Two things to get right. **Restore the limit when it wakes**, or it costs you capacity the next day and
moves the release band up with it — the device stays latched until charge reaches the floor plus five.
And **the floor only goes to 30 %**, so this parks a nearly empty pack rather than a full one. Above that
there is no software route; the vendor's own instruction is to disconnect the solar input and hold the
power button.
