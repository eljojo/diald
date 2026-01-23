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

When a direction change is detected at the raw event level, the system enters **Backlash mode**:

1. **Enter Backlash**: When an event's direction differs from the previous event's direction
2. **Discard old accumulator**: Any partial movement in the old direction is cleared
3. **Buffer new input**: Raw events are buffered in a separate accumulator
4. **Wait for stability**: Count consecutive events in the same direction

## Exiting Backlash Mode

There are two ways to exit Backlash mode:

### Confirmed direction change (threshold: 50 events)
If 50 consecutive events occur in the new direction, it's a real direction change:
- Transfer buffered input to the main accumulator
- Buzz to confirm the direction change
- Resume Active mode

### False positive cancellation (threshold: 10 events)
If 10 consecutive events occur in the *original* direction (before backlash), it was a false trigger:
- Transfer buffered input to the main accumulator
- Resume Active mode quietly (no buzz)
- No movement is lost

This "double anti-backlash" catches both real encoder backlash AND accidental micro-movements from the user's finger.

## Input Preservation

During Backlash mode, raw input isn't discarded - it's buffered. When exiting Backlash (either way), the buffered input is transferred to the main accumulator. This ensures no intentional movement is lost, only the spurious direction-change artifacts are filtered out.

</details>
