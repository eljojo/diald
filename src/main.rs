use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Write};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use evdev::{Device, InputEventKind, Key, RelativeAxisType};
use rumqttc::{Client, Event, MqttOptions, Packet, QoS};

static LOGGING_ENABLED: AtomicBool = AtomicBool::new(true);

macro_rules! log {
    ($($arg:tt)*) => {
        if LOGGING_ENABLED.load(Ordering::Relaxed) {
            println!($($arg)*);
        }
    };
}

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
            return args.next().map(PathBuf::from);
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
                log!("diald: opened haptics {}", path);
                Some(file)
            }
            Err(err) => {
                log!("diald: failed to open haptics {} ({})", path, err);
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
            log!("diald: haptics write failed ({})", err);
            self.file = None;
        }
    }
}

#[derive(PartialEq, Clone, Copy)]
enum DialMode {
    Idle,
    Active,
    Backlash,
}

impl DialMode {
    fn as_str(&self) -> &'static str {
        match self {
            DialMode::Idle => "idle",
            DialMode::Active => "active",
            DialMode::Backlash => "backlash",
        }
    }
}

struct DialState {
    mode: DialMode,
    last_event_at: Option<Instant>,
    volume: f64,
    raw_accumulator: i32,
    last_print_at: Option<Instant>,
    last_printed_volume: i32,
    clicking: bool,
    last_raw_direction: i32,      // -1, 0, or 1
    consistent_direction_count: u32,  // consecutive events in same direction
    pre_backlash_direction: i32,  // direction before entering backlash
    backlash_accumulator: i32,    // buffered input during backlash
}

const BACKLASH_THRESHOLD: u32 = 25;  // events needed to exit backlash mode
const BACKLASH_CANCEL_THRESHOLD: u32 = BACKLASH_THRESHOLD / 5;  // events to cancel false-positive backlash

impl DialState {
    fn new() -> Self {
        Self {
            mode: DialMode::Idle,
            last_event_at: None,
            volume: 50.0,
            raw_accumulator: 0,
            last_print_at: None,
            last_printed_volume: 50,
            clicking: false,
            last_raw_direction: 0,
            consistent_direction_count: 0,
            pre_backlash_direction: 0,
            backlash_accumulator: 0,
        }
    }

    fn set_mode(&mut self, mode: DialMode) {
        if self.mode != mode {
            log!("diald: state -> {}", mode.as_str());
            self.mode = mode;
        }
    }

    fn reset_to_idle(&mut self) {
        self.set_mode(DialMode::Idle);
        self.raw_accumulator = 0;
        self.last_raw_direction = 0;
        self.consistent_direction_count = 0;
        self.pre_backlash_direction = 0;
        self.backlash_accumulator = 0;
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

fn emit_batch(events: Vec<&'static str>, mqtt: &Option<MqttHandle>) {
    // Count occurrences of each event type
    let mut counts: Vec<(&'static str, u32)> = Vec::new();
    for event in events {
        if let Some((_, count)) = counts.iter_mut().find(|(e, _)| *e == event) {
            *count += 1;
        } else {
            counts.push((event, 1));
        }
    }
    for (event, count) in &counts {
        log!("diald: {} count={}", event, count);
    }

    // Publish clicks to MQTT
    if let Some(handle) = mqtt {
        for (event, count) in counts {
            if event == "click" {
                let _ = handle.client.publish(
                    "home/diald/click",
                    QoS::AtLeastOnce,
                    false,
                    count.to_string(),
                );
            }
        }
    }
}

struct MqttHandle {
    client: Client,
    incoming_rx: Receiver<i32>,
}

fn spawn_mqtt() -> Option<MqttHandle> {
    let host = env::var("MQTT_HOST").unwrap_or_else(|_| "localhost".to_string());
    let port: u16 = env::var("MQTT_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(1883);
    let username = env::var("MQTT_USERNAME").ok();
    let password = env::var("MQTT_PASSWORD").ok();

    let mut opts = MqttOptions::new("diald", &host, port);
    opts.set_keep_alive(Duration::from_secs(30));

    if let (Some(user), Some(pass)) = (&username, &password) {
        opts.set_credentials(user, pass);
    }

    let (client, mut connection) = Client::new(opts, 10);

    if let Err(err) = client.subscribe("home/diald/volume/set", QoS::AtLeastOnce) {
        log!("diald: mqtt subscribe failed ({})", err);
        return None;
    }

    let (tx, rx): (Sender<i32>, Receiver<i32>) = mpsc::channel();

    thread::spawn(move || {
        let mut last_error_log: Option<Instant> = None;
        for event in connection.iter() {
            match event {
                Ok(Event::Incoming(Packet::Publish(publish))) => {
                    if let Ok(payload) = std::str::from_utf8(&publish.payload) {
                        if let Ok(volume) = payload.trim().parse::<i32>() {
                            let _ = tx.send(volume);
                        }
                    }
                }
                Ok(Event::Incoming(Packet::ConnAck(_))) => {
                    log!("diald: mqtt connected to {}:{}", host, port);
                }
                Err(err) => {
                    let now = Instant::now();
                    let should_log = last_error_log
                        .map(|t| now.duration_since(t) >= Duration::from_secs(10))
                        .unwrap_or(true);
                    if should_log {
                        log!("diald: mqtt error ({})", err);
                        last_error_log = Some(now);
                    }
                }
                _ => {}
            }
        }
    });

    Some(MqttHandle { client, incoming_rx: rx })
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let device_path = parse_device_arg()
        .or_else(|| env::var_os("DIALD_DEVICE").map(PathBuf::from))
        .ok_or("missing device path; pass --device or set DIALD_DEVICE")?;

    let mut haptic = HapticDevice::new(device_path.clone());
    let mut state = DialState::new();
    let mut batcher = EventBatcher::new(Duration::from_millis(250));
    let mut mqtt = spawn_mqtt();

    // Disable logging after 30 minutes to preserve SD card
    thread::spawn(|| {
        thread::sleep(Duration::from_secs(30 * 60));
        LOGGING_ENABLED.store(false, Ordering::Relaxed);
    });

    let idle_timeout = Duration::from_secs(30);

    log!("diald: state -> disconnected");

    let mut open_error_logged = false;
    loop {
        let mut device = loop {
            match Device::open(&device_path) {
                Ok(dev) => {
                    set_nonblock(&dev)?;
                    log!("diald: opened {}", device_path.display());
                    log!("diald: name={:?}", dev.name());
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
                emit_batch(events, &mqtt);
            }

            // Check for incoming MQTT volume updates (only when idle)
            if let Some(ref handle) = mqtt {
                loop {
                    match handle.incoming_rx.try_recv() {
                        Ok(volume) => {
                            if state.mode == DialMode::Idle {
                                let clamped = (volume as f64).clamp(0.0, 100.0);
                                state.volume = clamped;
                                state.last_printed_volume = clamped.round() as i32;
                                log!("diald: mqtt volume -> {}", state.last_printed_volume);
                            }
                        }
                        Err(TryRecvError::Empty) => break,
                        Err(TryRecvError::Disconnected) => {
                            log!("diald: mqtt disconnected");
                            mqtt = None;
                            break;
                        }
                    }
                }
            }

            // Transition to idle after timeout
            if state.mode == DialMode::Active || state.mode == DialMode::Backlash {
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
                    log!("diald: lost device {} ({})", device_path.display(), err);
                    log!("diald: state -> disconnected");
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

                        // Backlash state machine: track direction changes at raw level
                        let direction = event.value().signum();
                        if state.last_raw_direction != 0 && direction != state.last_raw_direction {
                            // Direction changed - enter backlash mode
                            if state.mode != DialMode::Backlash {
                                log!("diald: entering backlash (direction {} -> {})", state.last_raw_direction, direction);
                                state.pre_backlash_direction = state.last_raw_direction;
                            }
                            state.mode = DialMode::Backlash;
                            state.consistent_direction_count = 1;
                            state.raw_accumulator = 0;  // discard accumulated movement
                            state.backlash_accumulator = event.value();  // start buffering
                        } else if direction == state.last_raw_direction {
                            state.consistent_direction_count += 1;
                            if state.mode == DialMode::Backlash {
                                state.backlash_accumulator += event.value();  // keep buffering
                            }
                        }
                        state.last_raw_direction = direction;

                        // Exit backlash mode once direction stabilizes
                        if state.mode == DialMode::Backlash {
                            // Cancel backlash if we quickly return to original direction (false positive)
                            if direction == state.pre_backlash_direction
                                && state.consistent_direction_count >= BACKLASH_CANCEL_THRESHOLD
                            {
                                log!("diald: canceling backlash (returned to original direction)");
                                state.mode = DialMode::Active;
                                state.raw_accumulator = state.backlash_accumulator;  // transfer buffered input
                                // no buzz - quietly resume, don't add event again (already buffered)
                            } else if state.consistent_direction_count >= BACKLASH_THRESHOLD {
                                log!("diald: exiting backlash (stable for {} events)", state.consistent_direction_count);
                                state.mode = DialMode::Active;
                                state.raw_accumulator = state.backlash_accumulator;  // transfer buffered input
                                haptic.send_chunky();
                                // don't add event again (already buffered)
                            } else {
                                continue;  // don't process events while in backlash
                            }
                        } else {
                            // Normal mode: accumulate raw input
                            // 40 raw = 1 volume unit (400 raw = 10 volume)
                            state.raw_accumulator += event.value();
                        }

                        let volume_delta = state.raw_accumulator / 40;
                        if volume_delta != 0 {
                            state.raw_accumulator -= volume_delta * 40;

                            let old_volume = state.volume;
                            let unclamped = state.volume + volume_delta as f64;
                            state.volume = unclamped.clamp(0.0, 100.0);

                            // Buzz at boundaries (trying to go past 0 or 100)
                            if unclamped < 0.0 || unclamped > 100.0 {
                                haptic.send_chunky();
                            }

                            // Buzz when crossing multiples of 10
                            // let old_ten = (old_volume / 10.0).floor() as i32;
                            // let new_ten = (state.volume / 10.0).floor() as i32;
                            // if old_ten != new_ten {
                            //     haptic.send_chunky();
                            // }

                            // Check if we should print
                            let current_volume = state.volume.round() as i32;
                            let old_tens = state.last_printed_volume / 10;
                            let new_tens = current_volume / 10;
                            let crossed_ten = old_tens != new_tens;

                            let now = Instant::now();
                            let time_to_print = state.last_print_at
                                .map(|t| now.duration_since(t) >= Duration::from_millis(250))
                                .unwrap_or(true);

                            let volume_changed = current_volume != state.last_printed_volume;

                            if crossed_ten || (volume_changed && time_to_print) {
                                log!("diald: volume {}", current_volume);
                                state.last_print_at = Some(now);
                                state.last_printed_volume = current_volume;

                                // Publish to MQTT
                                if let Some(ref handle) = mqtt {
                                    let _ = handle.client.publish(
                                        "home/diald/volume",
                                        QoS::AtLeastOnce,
                                        false,
                                        current_volume.to_string(),
                                    );
                                }
                            }
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
