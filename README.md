# diald

A daemon that connects your Microsoft Surface Dial to MQTT via Bluetooth for home automation.

## How it works

Diald reads rotation and click events from the Surface Dial's evdev input device and translates them into volume changes (0-100). It provides haptic feedback through the dial's hidraw interface and syncs state with MQTT.

### Volume control

- Rotation is converted to volume: 400 raw input units = 10 volume units
- Haptic buzz at boundaries (0 or 100), on wake from idle, and on direction changes
- Volume changes are published to MQTT and printed to stdout

### MQTT integration

- **Publishes to** `home/diald/volume` when volume changes
- **Publishes to** `home/diald/click` on button press (with click count)
- **Subscribes to** `home/diald/volume/set` for external volume updates (e.g., from Spotify)
- External updates are ignored while the dial is actively being used

## Building

```bash
nix build
# or
nix develop -c cargo build
```

<details>
<summary>Usage</summary>

```bash
diald --device /dev/input/event2
```

Or via environment variable:

```bash
DIALD_DEVICE=/dev/input/event2 diald
```

### MQTT configuration

Set via environment variables:

```bash
MQTT_HOST=localhost
MQTT_PORT=1883
MQTT_USERNAME=user
MQTT_PASSWORD=secret
```

### NixOS module

```nix
{
  inputs.diald.url = "github:yourusername/diald";

  outputs = { self, nixpkgs, diald }: {
    nixosConfigurations.myhost = nixpkgs.lib.nixosSystem {
      modules = [
        diald.nixosModules.default
        {
          services.diald = {
            enable = true;
            device = "/dev/input/event2";
            environmentFile = config.age.secrets.diald-mqtt.path;  # optional, for MQTT
          };
        }
      ];
    };
  };
}
```

The environment file should contain:

```
MQTT_HOST=mqtt.example.com
MQTT_PORT=1883
MQTT_USERNAME=myuser
MQTT_PASSWORD=secret
```

</details>

<details>
<summary>The Algorithm</summary>

## State Machine

```
                    ┌─────────────────────────────────┐
                    │                                 │
                    ▼                                 │
              ┌──────────┐                            │
              │   IDLE   │◄───────────────────────────┤
              └────┬─────┘        30s timeout         │
                   │                                  │
                   │ any input event                  │
                   │ (buzz)                           │
                   ▼                                  │
              ┌──────────┐                            │
         ┌───►│  ACTIVE  │────────────────────────────┘
         │    └────┬─────┘
         │         │
         │         │ direction change detected
         │         ▼
         │    ┌──────────┐
         │    │ BACKLASH │
         │    └────┬─────┘
         │         │
         │         ├── 50 events in NEW direction ──► exit + buzz
         │         │
         └─────────┴── 10 events in OLD direction ──► cancel (no buzz)
```

- **Idle**: Waiting for user input. Accepts external MQTT volume updates.
- **Active**: User is interacting. Ignores MQTT updates to prevent conflicts.
- **Backlash**: Temporary state during direction changes (see below).

## Volume Accumulation

Raw encoder events are accumulated and converted to volume units:
- 40 raw units = 1 volume unit
- 400 raw units = 10 volume units (one "notch")

This provides a smooth, continuous feel rather than discrete steps.

---

## Backlash Compensation

### What is Mechanical Backlash?

Rotary encoders have tiny gaps in their mechanical components. When you reverse
direction, the mechanism must "take up the slack" before it starts registering
the new direction. During this transition, the encoder outputs garbage.

```
  TURNING RIGHT                 REVERSING                  TURNING LEFT
       ↓                           ↓                            ↓
   ┌───────┐                  ┌───────┐                    ┌───────┐
   │encoder│ ──► +5 +5 +5     │encoder│ ──► +2 -1 +1 -3    │encoder│ ──► -5 -5 -5
   │ wheel │     (correct)    │ wheel │     (GARBAGE!)     │ wheel │     (correct)
   └───────┘                  └───────┘                    └───────┘

  User input:  ████████████░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░████████████
  Raw events:  +5 +5 +5 +5 +3 +2 -1 +1 -3 -2 -4 -5 -5 -5 -5 -5 -5 -5
                           └──────────────┘
                            backlash region
                           (hardware garbage)
```

The Surface Dial's capacitive encoder is especially prone to this.

### The Naive Approach (Why It Fails)

A simple approach: "when we see a direction change, enter backlash mode."

```
  Events:     +5  +5  +5  +3  +2  -4  -4  -4  -4  ...
                              │   │
                              │   └── Direction change detected HERE
                              │       Enter backlash mode
                              │
                              └────── But +3 and +2 were ALREADY COMMITTED!
                                      (they arrived before we detected the change)

  Result: Volume goes UP briefly, then DOWN. User sees: 50 → 51 → 49
          That +1 glitch is the backlash we failed to catch.
```

The problem: backlash events arrive BEFORE we can detect the direction change.
By the time we see `-4`, we've already processed `+3` and `+2`.

### The Solution: Delay Buffer

**Key insight**: We need to "look into the future" to catch backlash before
committing it. We do this by delaying all event processing by 50 events.

```
                         DELAY BUFFER (50 events)
                    ┌─────────────────────────────────┐
  events enter ───► │ +5 +5 +5 +5 +3 +2 -4 -4 -4 ... │ ───► events exit
  (most recent)     └─────────────────────────────────┘      (oldest, committed)
                              ▲
                              │
                      We can inspect the whole buffer
                      before deciding what to commit!
```

Events sit in the buffer for 50 events before being released. This gives us
a window to detect direction changes and filter out backlash BEFORE it gets
committed to the volume.

### How It Works: Normal Operation

When NOT in backlash mode, events flow through the buffer with a 50-event delay:

```
  Time ──────────────────────────────────────────────────────────────►

  Event arrives:    +5
  Buffer:           [+5]
  Released:         nothing (buffer not full yet)

  ... 49 more +5 events ...

  Event arrives:    +5 (the 51st event)
  Buffer:           [+5 +5 +5 +5 +5 ... +5 +5]  (50 events)
                     ▲                     ▲
                     │                     └── newest (just arrived)
                     └── oldest (pushed out, gets committed!)

  Accumulator:      += 5
```

The 50-event latency is imperceptible to users but crucial for backlash detection.

### How It Works: Direction Change (Real Reversal)

```
  User turns RIGHT, then LEFT.
  Events: ... +5 +5 +5 +3 +2 -4 -4 -4 -4 -4 ... (50 more -4s)
                       └────┘
                       backlash (but we don't know yet!)

  ═══════════════════════════════════════════════════════════════════

  STEP 1: Events +5, +5, +5 flow through normally

  Buffer: [+5 +5 +5 +5 +5 ... +5 +5 +5]    Mode: ACTIVE
  Committed: ... +5 +5 +5 (oldest events)

  ═══════════════════════════════════════════════════════════════════

  STEP 2: Backlash events +3, +2 arrive (still look like "right" direction)

  Buffer: [+5 +5 +5 ... +5 +5 +3 +2]       Mode: ACTIVE
  Committed: +5 +5 (more old +5s)

  Note: +3 and +2 are IN THE BUFFER, not committed yet!

  ═══════════════════════════════════════════════════════════════════

  STEP 3: First -4 arrives. Direction change detected!

  Buffer: [+5 +5 ... +5 +3 +2 -4]          Mode: BACKLASH ◄── ENTERED!
  Committed: NOTHING (we stop releasing events)

  The +3 and +2 are still in the buffer. We caught them in time!

  ═══════════════════════════════════════════════════════════════════

  STEP 4: More -4s arrive. We wait for stability (50 consecutive).

  Buffer: [+5 +3 +2 -4 -4 -4 -4 -4 ...]    Mode: BACKLASH
  Committed: NOTHING (still holding)

  consecutive_count: 1... 2... 3... ... 49... 50!

  ═══════════════════════════════════════════════════════════════════

  STEP 5: 50 consecutive -4s! Confirmed direction change. Exit backlash.

  Buffer: [+3 +2 -4 -4 -4 ... -4 -4 -4]    Mode: ACTIVE ◄── EXITED!

  drain_matching(direction = -1):
    - Keep: all the -4s ✓
    - Discard: +3, +2 ✗ (these were backlash!)

  Committed: sum of -4s only
  Haptic: BUZZ (confirms direction change to user)

  Result: Volume goes DOWN cleanly. No glitch!
```

### How It Works: False Positive (User Wobble)

Sometimes the user's finger wobbles, creating a momentary direction blip:

```
  User turns RIGHT, wobbles, continues RIGHT.
  Events: ... +5 +5 +5 -1 +5 +5 +5 +5 +5 +5 +5 +5 +5 +5 ...
                       └┘
                       accidental blip (not a real reversal)

  ═══════════════════════════════════════════════════════════════════

  STEP 1: The -1 arrives. Direction change detected!

  Buffer: [+5 +5 +5 ... +5 +5 -1]          Mode: BACKLASH ◄── ENTERED!
  pre_backlash_direction: +1 (we remember we were going right)

  ═══════════════════════════════════════════════════════════════════

  STEP 2: More +5s arrive. We're back to the ORIGINAL direction!

  Buffer: [+5 +5 ... +5 -1 +5 +5 +5 +5 +5 +5 +5 +5 +5 +5]
                                          Mode: BACKLASH

  consecutive_count in direction +1: 1... 2... 3... ... 10!

  ═══════════════════════════════════════════════════════════════════

  STEP 3: 10 consecutive +5s in ORIGINAL direction! Cancel backlash.

  Buffer: [+5 +5 ... -1 +5 +5 +5 +5 ...]   Mode: ACTIVE ◄── CANCELLED!

  drain_all():
    - Keep EVERYTHING (including the -1, it's just noise)

  Committed: sum of all events
  Haptic: (no buzz - quiet cancellation)

  Result: Volume continues UP smoothly. The tiny -1 is negligible.
```

### Why Two Thresholds?

```
  BACKLASH_THRESHOLD = 50        To CONFIRM a new direction
  BACKLASH_CANCEL_THRESHOLD = 10 To CANCEL a false positive

  Why 50 to confirm?
    - Hardware backlash can produce 20-30 spurious events
    - We need to be SURE it's a real direction change
    - 50 consecutive events = definitely intentional

  Why only 10 to cancel?
    - If we're back to the original direction quickly, it was a wobble
    - Don't make the user wait 50 events to resume normal operation
    - 10 events = "yep, still going the same way"
```

### Summary

```
  ┌─────────────────────────────────────────────────────────────────┐
  │                      THE DELAY BUFFER                           │
  │                                                                 │
  │  Problem:  Backlash events arrive BEFORE we detect the change   │
  │  Solution: Don't commit events immediately. Hold them.          │
  │            Detect the change. THEN decide what to commit.       │
  │                                                                 │
  │  ┌──────────┐    ┌──────────────────┐    ┌──────────────────┐  │
  │  │  Events  │───►│   Delay Buffer   │───►│  Accumulator     │  │
  │  │  arrive  │    │   (50 events)    │    │  (committed)     │  │
  │  └──────────┘    └──────────────────┘    └──────────────────┘  │
  │                          │                                      │
  │                          ▼                                      │
  │                  ┌───────────────┐                              │
  │                  │   Backlash    │                              │
  │                  │   Detection   │                              │
  │                  └───────────────┘                              │
  │                          │                                      │
  │              ┌───────────┴───────────┐                          │
  │              ▼                       ▼                          │
  │     ┌─────────────────┐    ┌─────────────────┐                  │
  │     │ Confirmed: keep │    │ Cancelled: keep │                  │
  │     │ only new dir    │    │ everything      │                  │
  │     └─────────────────┘    └─────────────────┘                  │
  └─────────────────────────────────────────────────────────────────┘
```

</details>
