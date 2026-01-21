use std::env;
use std::path::PathBuf;

use evdev::{Device, InputEventKind, Key, RelativeAxisType};

fn parse_device_arg() -> Option<PathBuf> {
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--device" {
            if let Some(path) = args.next() {
                return Some(PathBuf::from(path));
            }
        }
    }
    None
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let device_path = parse_device_arg()
        .or_else(|| env::var_os("DIALD_DEVICE").map(PathBuf::from))
        .ok_or("missing device path; pass --device or set DIALD_DEVICE")?;

    let mut device = Device::open(&device_path)?;

    println!("diald: opened {}", device_path.display());
    println!("diald: name={:?}", device.name());

    loop {
        for event in device.fetch_events()? {
            match event.kind() {
                InputEventKind::RelAxis(RelativeAxisType::REL_DIAL) => {
                    println!("diald: rotate {}", event.value());
                }
                InputEventKind::Key(Key::BTN_0) => {
                    let state = if event.value() == 1 { "down" } else { "up" };
                    println!("diald: click {}", state);
                }
                _ => {}
            }
        }
    }
}
