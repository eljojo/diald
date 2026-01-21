use std::env;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

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

    let mut pending_rotate: Option<(i32, Instant)> = None;
    let debounce_window = Duration::from_millis(500);

    loop {
        let mut device = loop {
            match Device::open(&device_path) {
                Ok(device) => {
                    println!("diald: opened {}", device_path.display());
                    println!("diald: name={:?}", device.name());
                    break device;
                }
                Err(err) => {
                    println!(
                        "diald: failed to open {} ({}), retrying...",
                        device_path.display(),
                        err
                    );
                    thread::sleep(Duration::from_secs(1));
                }
            }
        };

        loop {
            if let Some((value, deadline)) = pending_rotate {
                if Instant::now() >= deadline {
                    println!("diald: rotate {}", value);
                    pending_rotate = None;
                }
            }

            let events = match device.fetch_events() {
                Ok(events) => events,
                Err(err) => {
                    println!(
                        "diald: lost device {} ({}), reopening...",
                        device_path.display(),
                        err
                    );
                    pending_rotate = None;
                    break;
                }
            };

            let mut saw_event = false;
            for event in events {
                saw_event = true;
                match event.kind() {
                    InputEventKind::RelAxis(RelativeAxisType::REL_DIAL) => {
                        let now = Instant::now();
                        pending_rotate = Some((event.value(), now + debounce_window));
                    }
                    InputEventKind::Key(Key::BTN_0) => {
                        let state = if event.value() == 1 { "down" } else { "up" };
                        println!("diald: click {}", state);
                    }
                    _ => {}
                }
            }

            if !saw_event {
                thread::sleep(Duration::from_millis(25));
            }
        }
    }
}
