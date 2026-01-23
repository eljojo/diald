# diald

A daemon that connects your Microsoft Surface Dial to MQTT via Bluetooth for home automation.

## How it works

Diald reads rotation and click events from the Surface Dial's evdev input device and translates them into volume changes (0-100). It provides haptic feedback through the dial's hidraw interface and syncs state with MQTT.

### Volume control

- Rotation is converted to volume: 400 raw input units = 10 volume units
- Haptic buzz when crossing multiples of 10 (10, 20, 30, etc.)
- Haptic buzz when hitting boundaries (0 or 100)
- Volume changes are published to MQTT and printed to stdout

### MQTT integration

- **Publishes to** `home/diald/volume` when volume changes
- **Subscribes to** `home/diald/volume/set` for external volume updates (e.g., from Spotify)
- External updates are ignored while the dial is actively being used (30s timeout)

### State machine

- **Idle**: Accepts external MQTT volume updates
- **Active**: User is interacting with dial, ignores external updates
- Transitions to idle after 30 seconds of inactivity

## Usage

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

## NixOS module

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

## Building

```bash
nix build
# or
nix develop -c cargo build
```
