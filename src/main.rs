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
    let idle_reset = Duration::from_secs(2);
    let click_timeout = Duration::from_secs(2);
    let latch_threshold = 100;
    let min_volume_delta = 10;
    let mut latched = true;
    let mut skip_next_rotate_event = true;
    let mut last_event_at: Option<Instant> = None;
    let mut clicking = false;
    let mut click_started_at: Option<Instant> = None;

    let mut open_error_logged = false;
    loop {
        let mut device = loop {
            match Device::open(&device_path) {
                Ok(device) => {
                    println!("diald: opened {}", device_path.display());
                    println!("diald: name={:?}", device.name());
                    open_error_logged = false;
                    break device;
                }
                Err(err) => {
                    if !open_error_logged {
                        println!(
                            "diald: failed to open {} ({}), retrying...",
                            device_path.display(),
                            err
                        );
                        open_error_logged = true;
                    }
                    thread::sleep(Duration::from_secs(1));
                }
            }
        };

        loop {
            if let Some(last_event) = last_event_at {
                if Instant::now().duration_since(last_event) >= idle_reset {
                    pending_rotate = None;
                    latched = true;
                    skip_next_rotate_event = true;
                    last_event_at = None;
                }
            }

            if clicking {
                if let Some(started_at) = click_started_at {
                    if Instant::now().duration_since(started_at) >= click_timeout {
                        println!("diald: click aborted (timeout)");
                        clicking = false;
                        click_started_at = None;
                        pending_rotate = None;
                    }
                }
            }

            if let Some((value, deadline)) = pending_rotate {
                if Instant::now() >= deadline {
                    if latched {
                        if value.abs() >= latch_threshold {
                            let direction = if value > 0 { "up" } else { "down" };
                            println!("diald: volume {} {}", direction, value.abs());
                            latched = false;
                        }
                    } else if value.abs() >= min_volume_delta {
                        let direction = if value > 0 { "up" } else { "down" };
                        println!("diald: volume {} {}", direction, value.abs());
                    }
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
                    latched = true;
                    skip_next_rotate_event = true;
                    last_event_at = None;
                    break;
                }
            };

            let mut saw_event = false;
            for event in events {
                saw_event = true;
                last_event_at = Some(Instant::now());
                match event.kind() {
                    InputEventKind::RelAxis(RelativeAxisType::REL_DIAL) => {
                        if clicking {
                            continue;
                        }
                        if skip_next_rotate_event {
                            skip_next_rotate_event = false;
                            continue;
                        }
                        let now = Instant::now();
                        match pending_rotate {
                            Some((value, deadline)) if now >= deadline => {
                                if latched {
                                    if value.abs() >= latch_threshold {
                                        let direction = if value > 0 { "up" } else { "down" };
                                        println!("diald: volume {} {}", direction, value.abs());
                                        latched = false;
                                    }
                                } else if value.abs() >= min_volume_delta {
                                    let direction = if value > 0 { "up" } else { "down" };
                                    println!("diald: volume {} {}", direction, value.abs());
                                }
                                pending_rotate = Some((event.value(), now + debounce_window));
                            }
                            Some((value, deadline)) => {
                                pending_rotate = Some((value + event.value(), deadline));
                            }
                            None => {
                                pending_rotate = Some((event.value(), now + debounce_window));
                            }
                        }
                    }
                    InputEventKind::Key(Key::BTN_0) => {
                        if event.value() == 1 {
                            if !clicking {
                                clicking = true;
                                click_started_at = Some(Instant::now());
                                pending_rotate = None;
                            }
                        } else if clicking {
                            println!("diald: click up");
                            clicking = false;
                            click_started_at = None;
                        }
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
