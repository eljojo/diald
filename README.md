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

The dial operates in three modes:

- **Idle**: Waiting for user input. Accepts external MQTT volume updates. Transitions to Active on any input event.
- **Active**: User is interacting with the dial. Ignores external MQTT updates to prevent conflicts. Transitions to Idle after 30 seconds of inactivity.
- **Backlash**: Temporary state during direction changes (see below). Transitions to Active once movement stabilizes.

## Volume Accumulation

Raw encoder events are accumulated and converted to volume units:
- 40 raw units = 1 volume unit
- 400 raw units = 10 volume units (one "notch")

This provides a smooth, continuous feel rather than discrete steps.

## Backlash Compensation

Rotary encoders suffer from mechanical backlash - when you reverse direction, the first few signals can be spurious or in the wrong direction. This is especially noticeable on the Surface Dial's sensitive capacitive encoder.

### The Problem

When you reverse direction, the hardware sends a burst of wrong-direction events *before* we can detect the change. By the time software sees a direction change, it has already committed spurious events.

### The Solution: Delay Buffer

Events are held in a **delay buffer** (50 events) before being committed. This acts like a TV broadcast delay - giving us time to detect direction changes and filter backlash before events are processed.

When a direction change is detected at the raw event level, the system enters **Backlash mode**:

1. **Enter Backlash**: When an event's direction differs from the previous event's direction
2. **Hold all events**: Events stay in the buffer and are not committed
3. **Wait for stability**: Count consecutive events in the same direction

## Exiting Backlash Mode

There are two ways to exit Backlash mode:

### Confirmed direction change (threshold: 50 events)
If 50 consecutive events occur in the new direction, it's a real direction change:
- Drain buffer, keeping only events matching the new direction (discard backlash)
- Buzz to confirm the direction change
- Resume Active mode

### False positive cancellation (threshold: 10 events)
If 10 consecutive events occur in the *original* direction (before backlash), it was a false trigger:
- Drain buffer, keeping all events (none were backlash)
- Resume Active mode quietly (no buzz)

This "double anti-backlash" catches both real encoder backlash AND accidental micro-movements from the user's finger.

## Input Preservation

During normal operation, events flow through the delay buffer with a 50-event latency. This latency is imperceptible to users but provides a critical window for detecting and filtering backlash before events are committed. When exiting Backlash mode, legitimate movement is preserved while spurious direction-change artifacts are filtered out.

</details>
