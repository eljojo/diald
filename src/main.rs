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

#[derive(PartialEq)]
enum DialMode {
    Idle,
    Active,
}

struct DialState {
    mode: DialMode,
    last_event_at: Option<Instant>,
    accumulator: i32,
    clicking: bool,
}

impl DialState {
    fn new() -> Self {
        Self {
            mode: DialMode::Idle,
            last_event_at: None,
            accumulator: 0,
            clicking: false,
        }
    }

    fn reset_to_idle(&mut self) {
        self.mode = DialMode::Idle;
        self.accumulator = 0;
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let device_path = parse_device_arg()
        .or_else(|| env::var_os("DIALD_DEVICE").map(PathBuf::from))
        .ok_or("missing device path; pass --device or set DIALD_DEVICE")?;

    let mut haptic = HapticDevice::new(device_path.clone());
    let mut state = DialState::new();

    let idle_timeout = Duration::from_secs(30);
    let notch_threshold = 100;

    let mut open_error_logged = false;
    loop {
        let mut device = loop {
            match Device::open(&device_path) {
                Ok(device) => {
                    println!("diald: opened {}", device_path.display());
                    println!("diald: name={:?}", device.name());
                    open_error_logged = false;
                    state.reset_to_idle();
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

            // Transition to idle after timeout
            if state.mode == DialMode::Active {
                if let Some(last_event) = state.last_event_at {
                    if Instant::now().duration_since(last_event) >= idle_timeout {
                        state.reset_to_idle();
                    }
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
                    break;
                }
            };

            for event in events {
                // Any event transitions from Idle to Active (with vibration)
                if state.mode == DialMode::Idle {
                    state.mode = DialMode::Active;
                    haptic.send_chunky();
                }
                state.last_event_at = Some(Instant::now());

                match event.kind() {
                    InputEventKind::RelAxis(RelativeAxisType::REL_DIAL) => {
                        if state.clicking {
                            continue;
                        }

                        state.accumulator += event.value();

                        // Process notches
                        while state.accumulator >= notch_threshold {
                            state.accumulator -= notch_threshold;
                            println!("diald: volume up");
                        }
                        while state.accumulator <= -notch_threshold {
                            state.accumulator += notch_threshold;
                            println!("diald: volume down");
                        }
                    }
                    InputEventKind::Key(Key::BTN_0) => {
                        if event.value() == 1 {
                            state.clicking = true;
                        } else if state.clicking {
                            println!("diald: click");
                            state.clicking = false;
                        }
                    }
                    _ => {}
                }
            }

            thread::sleep(Duration::from_millis(10));
        }
    }
}
