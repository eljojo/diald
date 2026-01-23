use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
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

/// Find the hidraw device that shares the same HID parent as the given event device.
fn find_hidraw_for_event_device(event_path: &Path) -> Option<String> {
    // /dev/input/event2 -> event2
    let event_name = event_path.file_name()?;
    // /sys/class/input/event2/device -> canonical path to input device
    let event_sysfs = PathBuf::from("/sys/class/input").join(event_name);
    let event_device_path = fs::canonicalize(event_sysfs.join("device")).ok()?;

    // Check each hidraw to see if it's an ancestor of our event device
    let hidraw_dir = fs::read_dir("/sys/class/hidraw").ok()?;
    for entry in hidraw_dir.flatten() {
        let hidraw_device_link = entry.path().join("device");
        if let Ok(hidraw_device_path) = fs::canonicalize(&hidraw_device_link) {
            // The hidraw's device should be an ancestor of the event's device
            if event_device_path.starts_with(&hidraw_device_path) {
                let name = entry.file_name();
                return Some(format!("/dev/{}", name.to_string_lossy()));
            }
        }
    }
    None
}

struct HapticDevice {
    file: Option<File>,
    last_retry: Option<Instant>,
    event_path: PathBuf,
}

impl HapticDevice {
    fn new(event_path: PathBuf) -> Self {
        let file = Self::try_open(&event_path);
        Self { file, last_retry: None, event_path }
    }

    fn try_open(event_path: &Path) -> Option<File> {
        let path = env::var("DIALD_HAPTIC_DEV")
            .ok()
            .or_else(|| find_hidraw_for_event_device(event_path))?;

        match OpenOptions::new().write(true).open(&path) {
            Ok(file) => {
                println!("diald: opened haptics {}", path);
                Some(file)
            }
            Err(err) => {
                println!("diald: failed to open haptics {} ({})", path, err);
                None
            }
        }
    }

    fn reconnect(&mut self) {
        self.file = Self::try_open(&self.event_path);
        self.last_retry = None;
    }

    fn try_reconnect_if_needed(&mut self) {
        if self.file.is_some() {
            return;
        }
        let now = Instant::now();
        if let Some(last) = self.last_retry {
            if now.duration_since(last) < Duration::from_secs(1) {
                return;
            }
        }
        self.last_retry = Some(now);
        self.file = Self::try_open(&self.event_path);
    }

    fn send_chunky(&mut self) {
        let Some(file) = self.file.as_mut() else {
            return;
        };
        // Report ID 1 output: repeat=2, manual=3, retrigger=70 (chunky)
        let payload = [1u8, 2u8, 3u8, 70u8, 0u8];
        if let Err(err) = file.write_all(&payload) {
            println!("diald: haptics write failed ({})", err);
            self.file = None;
        }
    }
}

struct DialState {
    pending_rotate: Option<(i32, Instant)>,
    latched: bool,
    skip_next_rotate_event: bool,
    last_event_at: Option<Instant>,
    clicking: bool,
    click_started_at: Option<Instant>,
}

impl DialState {
    fn new() -> Self {
        Self {
            pending_rotate: None,
            latched: true,
            skip_next_rotate_event: true,
            last_event_at: None,
            clicking: false,
            click_started_at: None,
        }
    }

    fn reset(&mut self) {
        self.pending_rotate = None;
        self.latched = true;
        self.skip_next_rotate_event = true;
        self.last_event_at = None;
    }
}

fn emit_volume_change(value: i32) {
    let direction = if value > 0 { "up" } else { "down" };
    println!("diald: volume {} {}", direction, value.abs());
}

fn process_pending_rotate(
    state: &mut DialState,
    haptic: &mut HapticDevice,
    latch_threshold: i32,
    min_volume_delta: i32,
) {
    let Some((value, deadline)) = state.pending_rotate else {
        return;
    };
    if Instant::now() < deadline {
        return;
    }
    if state.latched {
        if value.abs() >= latch_threshold {
            emit_volume_change(value);
            haptic.send_chunky(); // Only vibrate on unlatch
            state.latched = false;
        }
    } else if value.abs() >= min_volume_delta {
        emit_volume_change(value);
    }
    state.pending_rotate = None;
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let device_path = parse_device_arg()
        .or_else(|| env::var_os("DIALD_DEVICE").map(PathBuf::from))
        .ok_or("missing device path; pass --device or set DIALD_DEVICE")?;

    let mut haptic = HapticDevice::new(device_path.clone());
    let mut state = DialState::new();

    let debounce_window = Duration::from_millis(500);
    let idle_reset = Duration::from_secs(2);
    let click_timeout = Duration::from_secs(2);
    let latch_threshold = 100;
    let min_volume_delta = 10;

    let mut open_error_logged = false;
    loop {
        let mut device = loop {
            match Device::open(&device_path) {
                Ok(device) => {
                    println!("diald: opened {}", device_path.display());
                    println!("diald: name={:?}", device.name());
                    open_error_logged = false;
                    haptic.reconnect();
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
            // Try to reconnect haptic device if needed
            haptic.try_reconnect_if_needed();

            // Reset state after idle period
            if let Some(last_event) = state.last_event_at {
                if Instant::now().duration_since(last_event) >= idle_reset {
                    state.reset();
                }
            }

            // Handle click timeout
            if state.clicking {
                if let Some(started_at) = state.click_started_at {
                    if Instant::now().duration_since(started_at) >= click_timeout {
                        println!("diald: click aborted (timeout)");
                        state.clicking = false;
                        state.click_started_at = None;
                        state.pending_rotate = None;
                    }
                }
            }

            // Process pending rotation if deadline passed
            process_pending_rotate(&mut state, &mut haptic, latch_threshold, min_volume_delta);

            let events = match device.fetch_events() {
                Ok(events) => events,
                Err(err) => {
                    println!(
                        "diald: lost device {} ({}), reopening...",
                        device_path.display(),
                        err
                    );
                    state.reset();
                    break;
                }
            };

            let mut saw_event = false;
            for event in events {
                saw_event = true;
                state.last_event_at = Some(Instant::now());

                match event.kind() {
                    InputEventKind::RelAxis(RelativeAxisType::REL_DIAL) => {
                        if state.clicking {
                            continue;
                        }
                        if state.skip_next_rotate_event {
                            state.skip_next_rotate_event = false;
                            continue;
                        }

                        let now = Instant::now();

                        // Process any pending rotation that's past deadline before accumulating new value
                        process_pending_rotate(
                            &mut state,
                            &mut haptic,
                            latch_threshold,
                            min_volume_delta,
                        );

                        // Accumulate or start new pending rotation
                        match state.pending_rotate {
                            Some((value, deadline)) => {
                                state.pending_rotate = Some((value + event.value(), deadline));
                            }
                            None => {
                                state.pending_rotate =
                                    Some((event.value(), now + debounce_window));
                            }
                        }
                    }
                    InputEventKind::Key(Key::BTN_0) => {
                        if event.value() == 1 {
                            if !state.clicking {
                                state.clicking = true;
                                state.click_started_at = Some(Instant::now());
                                state.pending_rotate = None;
                            }
                        } else if state.clicking {
                            println!("diald: click up");
                            state.clicking = false;
                            state.click_started_at = None;
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
