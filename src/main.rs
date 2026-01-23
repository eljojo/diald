use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Write};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use evdev::{Device, InputEventKind, Key, RelativeAxisType};

fn set_nonblock(device: &Device) -> std::io::Result<()> {
    let fd = device.as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let result = unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) };
    if result < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

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

#[derive(PartialEq, Clone, Copy)]
enum DialMode {
    Idle,
    Active,
}

impl DialMode {
    fn as_str(&self) -> &'static str {
        match self {
            DialMode::Idle => "idle",
            DialMode::Active => "active",
        }
    }
}

struct DialState {
    mode: DialMode,
    last_event_at: Option<Instant>,
    accumulator: i32,
    smoothed_magnitude: f64,
    clicking: bool,
}

impl DialState {
    fn new() -> Self {
        Self {
            mode: DialMode::Idle,
            last_event_at: None,
            accumulator: 0,
            smoothed_magnitude: 2.0,
            clicking: false,
        }
    }

    fn set_mode(&mut self, mode: DialMode) {
        if self.mode != mode {
            println!("diald: state -> {}", mode.as_str());
            self.mode = mode;
        }
    }

    fn reset_to_idle(&mut self) {
        self.set_mode(DialMode::Idle);
        self.accumulator = 0;
        self.smoothed_magnitude = 2.0;
    }
}

struct EventBatcher {
    events: Vec<&'static str>,
    deadline: Option<Instant>,
    window: Duration,
}

impl EventBatcher {
    fn new(window: Duration) -> Self {
        Self {
            events: Vec::new(),
            deadline: None,
            window,
        }
    }

    fn push(&mut self, event: &'static str) {
        if self.deadline.is_none() {
            self.deadline = Some(Instant::now() + self.window);
        }
        self.events.push(event);
    }

    fn try_flush(&mut self) -> Option<Vec<&'static str>> {
        let deadline = self.deadline?;
        if Instant::now() < deadline {
            return None;
        }
        self.deadline = None;
        Some(std::mem::take(&mut self.events))
    }
}

fn emit_batch(events: Vec<&'static str>) {
    // Count occurrences of each event type
    let mut counts: Vec<(&'static str, u32)> = Vec::new();
    for event in events {
        if let Some((_, count)) = counts.iter_mut().find(|(e, _)| *e == event) {
            *count += 1;
        } else {
            counts.push((event, 1));
        }
    }
    for (event, count) in counts {
        println!("diald: {} count={}", event, count);
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let device_path = parse_device_arg()
        .or_else(|| env::var_os("DIALD_DEVICE").map(PathBuf::from))
        .ok_or("missing device path; pass --device or set DIALD_DEVICE")?;

    let mut haptic = HapticDevice::new(device_path.clone());
    let mut state = DialState::new();
    let mut batcher = EventBatcher::new(Duration::from_millis(250));

    let idle_timeout = Duration::from_secs(30);

    println!("diald: state -> disconnected");

    let mut open_error_logged = false;
    loop {
        let mut device = loop {
            match Device::open(&device_path) {
                Ok(dev) => {
                    set_nonblock(&dev)?;
                    println!("diald: opened {}", device_path.display());
                    println!("diald: name={:?}", dev.name());
                    open_error_logged = false;
                    state.reset_to_idle();
                    haptic.reconnect();
                    break dev;
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
            haptic.try_reconnect_if_needed();

            // Flush batched events if deadline passed
            if let Some(events) = batcher.try_flush() {
                emit_batch(events);
            }

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
                Err(err) if err.kind() == ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(10));
                    continue;
                }
                Err(err) => {
                    println!("diald: lost device {} ({})", device_path.display(), err);
                    println!("diald: state -> disconnected");
                    break;
                }
            };

            for event in events {
                if state.mode == DialMode::Idle {
                    state.set_mode(DialMode::Active);
                }
                state.last_event_at = Some(Instant::now());

                match event.kind() {
                    InputEventKind::RelAxis(RelativeAxisType::REL_DIAL) => {
                        if state.clicking {
                            continue;
                        }

                        // Update smoothed magnitude (small = precise, large = fast)
                        let magnitude = event.value().abs() as f64;
                        let alpha = 0.3;
                        state.smoothed_magnitude = alpha * magnitude + (1.0 - alpha) * state.smoothed_magnitude;

                        // Piecewise threshold: precision range has steep slope, fast range gentle
                        let notch_threshold = if state.smoothed_magnitude < 2.0 {
                            // 1.0 → 200, 2.0 → 400
                            (200.0 + (state.smoothed_magnitude - 1.0) * 200.0).clamp(200.0, 400.0) as i32
                        } else {
                            // 2.0 → 400, 22.0 → 600
                            (400.0 + (state.smoothed_magnitude - 2.0) * 10.0).clamp(400.0, 600.0) as i32
                        };

                        state.accumulator += event.value();

                        while state.accumulator >= notch_threshold {
                            state.accumulator -= notch_threshold;
                            batcher.push("volume up");
                            haptic.send_chunky();
                            println!("diald: notch threshold={} smoothed={:.1}", notch_threshold, state.smoothed_magnitude);
                        }
                        while state.accumulator <= -notch_threshold {
                            state.accumulator += notch_threshold;
                            batcher.push("volume down");
                            haptic.send_chunky();
                            println!("diald: notch threshold={} smoothed={:.1}", notch_threshold, state.smoothed_magnitude);
                        }
                    }
                    InputEventKind::Key(Key::BTN_0) => {
                        if event.value() == 1 {
                            state.clicking = true;
                        } else if state.clicking {
                            state.clicking = false;
                            batcher.push("click");
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}
