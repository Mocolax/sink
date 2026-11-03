//! SteelSeries Arctis Nova 7 Gen 1 hardware ChatMix support.
//!
//! Protocol framing and byte offsets are adapted from the GPL-3.0-only
//! `rust-arctis-chatmix` project by rdamron, which in turn documents its
//! source as Linux-Arctis-Manager. See `docs/STEELSERIES_CHATMIX.md`.
//! This module intentionally sends only the session/query commands required
//! to receive dial and wireless-status reports. It never writes EQ, sidetone,
//! gain, auto-off, microphone, or other persistent headset settings.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;
use tauri::{Emitter, Manager};

use crate::state::AppState;

const HID_ID: &str = "0003:00001038:00002202";
const COMMAND_INTERFACE_SUFFIX: &str = "/input3";
const DIAL_INTERFACE_SUFFIX: &str = "/input5";
const REPORT_LEN: usize = 64;
const STATUS_INTERVAL: Duration = Duration::from_secs(2);
const PERSIST_DEBOUNCE: Duration = Duration::from_millis(350);
const SESSION_POLL_MS: i32 = 250;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ArctisConnectionState {
    Disabled,
    Connected,
    Disconnected,
    PermissionDenied,
    Unsupported,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ArctisStatus {
    pub state: ArctisConnectionState,
    pub detail: Option<String>,
}

impl Default for ArctisStatus {
    fn default() -> Self {
        Self {
            state: ArctisConnectionState::Disabled,
            detail: None,
        }
    }
}

/// Shared status plus a condition variable so settings changes wake a worker
/// that may currently be in reconnect backoff.
pub struct HardwareRuntime {
    status: Mutex<ArctisStatus>,
    audio_ready: AtomicBool,
    wake_generation: Mutex<u64>,
    wake: Condvar,
}

impl Default for HardwareRuntime {
    fn default() -> Self {
        Self {
            status: Mutex::new(ArctisStatus::default()),
            audio_ready: AtomicBool::new(false),
            wake_generation: Mutex::new(0),
            wake: Condvar::new(),
        }
    }
}

impl HardwareRuntime {
    pub fn status(&self) -> ArctisStatus {
        self.status.lock().map(|s| s.clone()).unwrap_or_default()
    }

    pub fn notify_settings_changed(&self) {
        if let Ok(mut generation) = self.wake_generation.lock() {
            *generation = generation.wrapping_add(1);
            self.wake.notify_all();
        }
    }

    pub fn mark_audio_ready(&self) {
        self.audio_ready.store(true, Ordering::Release);
        self.notify_settings_changed();
    }

    pub fn mark_audio_unready(&self) {
        self.audio_ready.store(false, Ordering::Release);
        self.notify_settings_changed();
    }

    fn audio_ready(&self) -> bool {
        self.audio_ready.load(Ordering::Acquire)
    }

    fn wait(&self, duration: Duration) {
        let Ok(generation) = self.wake_generation.lock() else {
            std::thread::sleep(duration);
            return;
        };
        let observed = *generation;
        let _ = self
            .wake
            .wait_timeout_while(generation, duration, |current| *current == observed);
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct ChatMixEvent {
    a: String,
    b: String,
    a_volume: u8,
    b_volume: u8,
}

#[derive(Debug, Clone)]
struct WorkerPrefs {
    enabled: bool,
    auto_switch: bool,
    output: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum AutoSwitchAction {
    None,
    Prime,
    Claim,
    Release(Option<String>),
}

/// Pure transition state: no first-sample action and no shutdown action.
/// Those two rules prevent launching or quitting the GUI from changing the
/// user's default output.
#[derive(Debug, Default)]
struct AutoSwitchState {
    observed_online: Option<bool>,
    previous_default: Option<String>,
    output_claims: HashMap<String, OutputClaim>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct OutputClaim {
    claimed: String,
    restore: Option<String>,
    saved: Option<String>,
}

impl AutoSwitchState {
    fn reset_observation(&mut self) {
        self.observed_online = None;
        self.previous_default = None;
        self.output_claims.clear();
    }

    fn observe(
        &mut self,
        online: bool,
        enabled: bool,
        current_default: Option<&str>,
        // Defaults Sink may have set itself: the headset and the channels.
        owned: &[String],
    ) -> AutoSwitchAction {
        let Some(previous_online) = self.observed_online.replace(online) else {
            return if online && enabled {
                AutoSwitchAction::Prime
            } else {
                AutoSwitchAction::None
            };
        };
        if previous_online == online {
            return AutoSwitchAction::None;
        }
        if !enabled {
            self.previous_default = None;
            return AutoSwitchAction::None;
        }
        if online {
            self.previous_default = current_default
                .filter(|name| !owned.iter().any(|candidate| candidate == name))
                .map(str::to_string);
            AutoSwitchAction::Claim
        } else {
            let restore = current_default
                .filter(|name| owned.iter().any(|candidate| candidate == name))
                .and_then(|_| self.previous_default.take());
            // Always release the temporary per-channel output claims. The
            // previous system default is optional because a manual default
            // change must still be preserved.
            self.previous_default = None;
            AutoSwitchAction::Release(restore)
        }
    }
}

fn restore_output_for_claim(
    saved: Option<&str>,
    resolved: Option<&str>,
    claimed: &str,
    alternate: Option<&str>,
) -> Option<String> {
    saved.map(str::to_string).or_else(|| match resolved {
        Some(current) if current != claimed => Some(current.to_string()),
        _ => alternate.map(str::to_string),
    })
}

fn auto_switch_output_plan(
    controlled: &[String],
    saved_outputs: &HashMap<String, Option<String>>,
    target: Option<&str>,
) -> Vec<(String, Option<String>, Option<String>)> {
    controlled
        .iter()
        .map(|name| {
            let saved = saved_outputs.get(name).cloned().flatten();
            let claimed = target.map(str::to_string).or_else(|| saved.clone());
            (name.clone(), saved, claimed)
        })
        .collect()
}

fn should_restore_claim(current: Option<Option<&str>>, claimed: &str) -> bool {
    match current {
        // Backends without resolved-output reporting cannot provide the
        // second guard; the persisted-setting guard still applies.
        None => true,
        Some(Some(name)) => name == claimed,
        Some(None) => false,
    }
}

#[derive(Debug)]
enum Discovery {
    Found(DevicePaths),
    Missing,
    Unsupported(String),
}

#[derive(Debug)]
struct DevicePaths {
    command: PathBuf,
    listeners: Vec<PathBuf>,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct ParsedReport {
    mix: Option<(u8, u8)>,
    online: Option<bool>,
}

/// Nova 7 Gen 1 reports are unnumbered. Dial reports are
/// `45 <game> <chat>`; status reports are
/// `b0 <power> <battery> <charging> <game> <chat> ...`.
fn parse_report(report: &[u8]) -> ParsedReport {
    match report.first().copied() {
        Some(0x45) if report.len() >= 3 => ParsedReport {
            mix: Some((report[1].min(100), report[2].min(100))),
            online: None,
        },
        Some(0xb0) if report.len() >= 6 => ParsedReport {
            mix: Some((report[4].min(100), report[5].min(100))),
            online: match report[1] {
                0x02 => Some(false),
                0x03 => Some(true),
                _ => None,
            },
        },
        _ => ParsedReport::default(),
    }
}

fn uevent_matches(content: &str) -> bool {
    let id = content
        .lines()
        .find_map(|line| line.strip_prefix("HID_ID="));
    let phys = content
        .lines()
        .find_map(|line| line.strip_prefix("HID_PHYS="));
    id == Some(HID_ID) && phys.is_some_and(|path| path.ends_with(COMMAND_INTERFACE_SUFFIX))
}

fn hid_phys(content: &str) -> Option<&str> {
    content
        .lines()
        .find_map(|line| line.strip_prefix("HID_PHYS="))
}

fn listener_matches(content: &str, expected_phys: &str) -> bool {
    content
        .lines()
        .any(|line| line.strip_prefix("HID_ID=") == Some(HID_ID))
        && hid_phys(content) == Some(expected_phys)
}

fn discover() -> Discovery {
    let entries = match fs::read_dir("/sys/class/hidraw") {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Discovery::Unsupported("hidraw is not available on this system".into())
        }
        Err(e) => return Discovery::Unsupported(format!("cannot inspect hidraw: {e}")),
    };

    let entries: Vec<(String, String)> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let content = fs::read_to_string(entry.path().join("device/uevent")).ok()?;
            Some((name, content))
        })
        .collect();

    for (name, content) in &entries {
        if uevent_matches(content) {
            let phys_base = hid_phys(content)
                .and_then(|path| path.rsplit_once("/input"))
                .map(|(base, _)| base);
            let listeners = phys_base
                .into_iter()
                .flat_map(|base| {
                    let expected = format!("{base}{DIAL_INTERFACE_SUFFIX}");
                    entries
                        .iter()
                        .filter(move |&(candidate, uevent)| {
                            candidate != name && listener_matches(uevent, &expected)
                        })
                        .map(|(candidate, _)| PathBuf::from("/dev").join(candidate))
                })
                .collect();
            return Discovery::Found(DevicePaths {
                command: PathBuf::from("/dev").join(name),
                listeners,
            });
        }
    }
    Discovery::Missing
}

fn frame_command(command: &[u8]) -> Vec<u8> {
    let mut report = vec![0; REPORT_LEN + 1];
    let copy_len = command.len().min(REPORT_LEN);
    report[1..1 + copy_len].copy_from_slice(&command[..copy_len]);
    report
}

fn write_command(device: &mut File, command: &[u8]) -> io::Result<()> {
    let report = frame_command(command);
    let written = device.write(&report)?;
    if written == report.len() {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::WriteZero,
            format!("short hidraw write: {written}/{} bytes", report.len()),
        ))
    }
}

fn poll_readable(fd: i32, timeout_ms: i32) -> io::Result<bool> {
    let mut pollfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    let result = unsafe { libc::poll(&mut pollfd, 1, timeout_ms) };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    if result == 0 {
        return Ok(false);
    }
    if pollfd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
        return Err(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "hidraw device disconnected",
        ));
    }
    Ok(pollfd.revents & libc::POLLIN != 0)
}

fn poll_any(fds: &[i32], timeout_ms: i32) -> io::Result<bool> {
    let mut pollfds: Vec<libc::pollfd> = fds
        .iter()
        .map(|fd| libc::pollfd {
            fd: *fd,
            events: libc::POLLIN,
            revents: 0,
        })
        .collect();
    let result = unsafe {
        libc::poll(
            pollfds.as_mut_ptr(),
            pollfds.len() as libc::nfds_t,
            timeout_ms,
        )
    };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    if result == 0 {
        return Ok(false);
    }
    if pollfds
        .iter()
        .any(|fd| fd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0)
    {
        return Err(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "hidraw device disconnected",
        ));
    }
    Ok(pollfds.iter().any(|fd| fd.revents & libc::POLLIN != 0))
}

fn worker_prefs(app: &tauri::AppHandle) -> Option<WorkerPrefs> {
    let state = app.state::<AppState>();
    let mixer = state.lock_mixer().ok()?;
    Some(WorkerPrefs {
        enabled: mixer.prefs.hardware_chatmix_enabled,
        auto_switch: mixer.prefs.headset_auto_switch,
        output: mixer.prefs.arctis_output.clone(),
    })
}

fn balance_pair(app: &tauri::AppHandle) -> Option<(String, String)> {
    let state = app.state::<AppState>();
    let mixer = state.lock_mixer().ok()?;
    let names: Vec<&str> = mixer
        .channel_defs
        .channels
        .iter()
        .map(|channel| channel.name.as_str())
        .collect();
    if names.len() < 2 {
        return None;
    }

    let valid = |candidate: Option<&String>| {
        candidate
            .filter(|name| names.contains(&name.as_str()))
            .cloned()
    };
    let mut a = valid(mixer.prefs.balance_a.as_ref());
    let mut b = valid(mixer.prefs.balance_b.as_ref());
    if a.is_none() {
        a = names
            .contains(&"sink_game")
            .then(|| "sink_game".to_string());
    }
    if b.is_none() {
        b = names
            .contains(&"sink_chat")
            .then(|| "sink_chat".to_string());
    }
    let a = a.unwrap_or_else(|| names[0].to_string());
    let b = match b {
        Some(candidate) if candidate != a => candidate,
        _ => names
            .iter()
            .copied()
            .find(|candidate| *candidate != a.as_str())?
            .to_string(),
    };
    Some((a, b))
}

fn set_status(
    app: &tauri::AppHandle,
    runtime: &HardwareRuntime,
    state: ArctisConnectionState,
    detail: Option<String>,
) {
    let next = ArctisStatus { state, detail };
    let changed = runtime
        .status
        .lock()
        .map(|mut status| {
            if *status == next {
                false
            } else {
                *status = next.clone();
                true
            }
        })
        .unwrap_or(false);
    if changed {
        let _ = app.emit("arctis-status", next);
    }
}

fn apply_mix(app: &tauri::AppHandle, game: u8, chat: u8) -> Result<ChatMixEvent, String> {
    let (a, b) = balance_pair(app).ok_or_else(|| "ChatMix requires two channels".to_string())?;
    let state = app.state::<AppState>();
    state
        .backend
        .set_sink_volume(&a, game)
        .map_err(|e| e.to_string())?;
    state
        .backend
        .set_sink_volume(&b, chat)
        .map_err(|e| e.to_string())?;

    {
        let mut mixer = state.lock_mixer()?;
        if let Some(channel) = mixer.channel_mut(&a) {
            channel.volume_percent = game;
        }
        if let Some(channel) = mixer.channel_mut(&b) {
            channel.volume_percent = chat;
        }
        mixer.channel_defs.set_volume(&a, game);
        mixer.channel_defs.set_volume(&b, chat);
    }

    let event = ChatMixEvent {
        a,
        b,
        a_volume: game,
        b_volume: chat,
    };
    let _ = app.emit("arctis-chatmix", &event);
    Ok(event)
}

fn persist_mix(app: &tauri::AppHandle) {
    let state = app.state::<AppState>();
    let Ok(mixer) = state.lock_mixer() else {
        return;
    };
    let channels = mixer.channel_defs.clone();
    crate::commands::profiles::autosave_active(&mixer);
    drop(mixer);
    if let Err(e) = channels.save() {
        eprintln!("sink: persisting hardware ChatMix volumes failed: {e}");
    }
}

struct DebouncedPersistence<'a> {
    app: &'a tauri::AppHandle,
    due: Option<Instant>,
}

impl<'a> DebouncedPersistence<'a> {
    fn new(app: &'a tauri::AppHandle) -> Self {
        Self { app, due: None }
    }

    fn mark_dirty(&mut self) {
        self.due = Some(Instant::now());
    }

    fn flush_if_due(&mut self) {
        if self
            .due
            .is_some_and(|due| due.elapsed() >= PERSIST_DEBOUNCE)
        {
            persist_mix(self.app);
            self.due = None;
        }
    }
}

impl Drop for DebouncedPersistence<'_> {
    fn drop(&mut self) {
        if self.due.is_some() {
            persist_mix(self.app);
        }
    }
}

fn controlled_channels(app: &tauri::AppHandle) -> Vec<String> {
    app.state::<AppState>()
        .lock_mixer()
        .map(|mixer| {
            mixer
                .channel_defs
                .channels
                .iter()
                .map(|channel| channel.name.clone())
                .collect()
        })
        .unwrap_or_default()
}

fn apply_auto_switch(app: &tauri::AppHandle, machine: &mut AutoSwitchState, online: bool) {
    let Some(prefs) = worker_prefs(app) else {
        return;
    };
    let state = app.state::<AppState>();
    let controlled = controlled_channels(app);
    let current = state
        .backend
        .get_default_devices()
        .ok()
        .and_then(|(output, _)| output);
    let owned: Vec<String> = controlled
        .iter()
        .cloned()
        .chain(prefs.output.clone())
        .collect();
    match machine.observe(online, prefs.auto_switch, current.as_deref(), &owned) {
        AutoSwitchAction::None => {}
        AutoSwitchAction::Prime => {
            let Some(target) = prefs.output.as_deref() else {
                return;
            };
            if controlled.is_empty() {
                return;
            }
            let saved_outputs = {
                let Ok(mixer) = state.lock_mixer() else {
                    return;
                };
                auto_switch_output_plan(
                    &controlled,
                    &mixer.outputs.outputs,
                    prefs.output.as_deref(),
                )
            };
            let resolved = state.backend.resolved_channel_outputs().unwrap_or_default();
            machine.output_claims.clear();
            for (name, saved, _) in saved_outputs {
                let Some(current) = resolved.get(&name).and_then(|value| value.as_deref()) else {
                    continue;
                };
                if current != target {
                    continue;
                }
                let alternate = if saved.is_none() {
                    state
                        .backend
                        .preferred_output_excluding(target)
                        .ok()
                        .flatten()
                } else {
                    None
                };
                let restore = restore_output_for_claim(
                    saved.as_deref(),
                    Some(current),
                    target,
                    alternate.as_deref(),
                );
                machine.output_claims.insert(
                    name,
                    OutputClaim {
                        claimed: target.to_string(),
                        restore,
                        saved,
                    },
                );
            }
        }
        AutoSwitchAction::Release(previous) => {
            let current_outputs = state.backend.resolved_channel_outputs().unwrap_or_default();
            let current_saved = state.lock_mixer().map(|mixer| {
                machine
                    .output_claims
                    .keys()
                    .map(|name| (name.clone(), mixer.outputs.get(name).map(str::to_string)))
                    .collect::<HashMap<_, _>>()
            });
            let claims = std::mem::take(&mut machine.output_claims);
            if let Ok(current_saved) = current_saved {
                for (name, claim) in claims {
                    if current_saved.get(&name) != Some(&claim.saved) {
                        continue;
                    }
                    let current = current_outputs.get(&name).map(|value| value.as_deref());
                    if !should_restore_claim(current, &claim.claimed) {
                        continue;
                    }
                    if let Err(e) = state
                        .backend
                        .set_channel_output(&name, claim.restore.as_deref())
                    {
                        eprintln!("sink: releasing ChatMix output for {name} failed: {e}");
                    }
                }
            }
            // Nothing to restore (the headset was already the default when it
            // came online): leave the headset for the best other device.
            let previous = previous.or_else(|| {
                let target = prefs.output.as_deref()?;
                current
                    .as_deref()
                    .filter(|name| owned.iter().any(|candidate| candidate == name))?;
                state
                    .backend
                    .preferred_output_excluding(target)
                    .ok()
                    .flatten()
            });
            if let Some(previous) = previous {
                if let Err(e) = state.backend.set_default_output(&previous) {
                    eprintln!("sink: restoring the pre-headset output failed: {e}");
                }
            }
        }
        AutoSwitchAction::Claim => {
            if controlled.is_empty() {
                return;
            }
            let outputs = {
                let Ok(mixer) = state.lock_mixer() else {
                    return;
                };
                auto_switch_output_plan(
                    &controlled,
                    &mixer.outputs.outputs,
                    prefs.output.as_deref(),
                )
            };
            let resolved = state.backend.resolved_channel_outputs().unwrap_or_default();
            machine.output_claims.clear();
            for (name, saved, claimed) in outputs {
                let alternate = claimed.as_deref().and_then(|target| {
                    (saved.is_none()
                        && resolved
                            .get(&name)
                            .and_then(|value| value.as_deref())
                            .is_none_or(|current| current == target))
                    .then(|| {
                        state
                            .backend
                            .preferred_output_excluding(target)
                            .ok()
                            .flatten()
                    })
                    .flatten()
                });
                let restore = claimed.as_deref().and_then(|target| {
                    restore_output_for_claim(
                        saved.as_deref(),
                        resolved.get(&name).and_then(|value| value.as_deref()),
                        target,
                        alternate.as_deref(),
                    )
                });
                if let Err(e) = state.backend.set_channel_output(&name, claimed.as_deref()) {
                    eprintln!("sink: activating ChatMix output for {name} failed: {e}");
                } else if let Some(claimed) = claimed {
                    machine.output_claims.insert(
                        name,
                        OutputClaim {
                            claimed,
                            restore,
                            saved,
                        },
                    );
                }
            }
            if let Some(target) = prefs.output.as_deref() {
                if let Err(e) = state.backend.set_default_output(target) {
                    eprintln!("sink: setting the headset as default failed: {e}");
                }
            }
        }
    }
}

fn run_device_session(
    app: &tauri::AppHandle,
    runtime: &HardwareRuntime,
    machine: &mut AutoSwitchState,
    paths: &DevicePaths,
) -> io::Result<()> {
    let mut device = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&paths.command)?;
    let mut listeners: Vec<File> = paths
        .listeners
        .iter()
        .map(File::open)
        .collect::<io::Result<_>>()?;

    // Session/query sequence from rust-arctis-chatmix's Nova 7 definition.
    for command in [&[0x10][..], &[0x09][..], &[0xb0][..]] {
        write_command(&mut device, command)?;
        std::thread::sleep(Duration::from_millis(10));
    }

    set_status(
        app,
        runtime,
        ArctisConnectionState::Disconnected,
        Some("Waiting for the wireless headset status".into()),
    );

    let mut last_status = Instant::now();
    let mut last_applied: Option<ChatMixEvent> = None;
    let mut persistence = DebouncedPersistence::new(app);

    loop {
        let prefs = worker_prefs(app).unwrap_or(WorkerPrefs {
            enabled: false,
            auto_switch: false,
            output: None,
        });
        if !prefs.enabled {
            return Ok(());
        }
        if !runtime.audio_ready() {
            return Ok(());
        }

        if last_status.elapsed() >= STATUS_INTERVAL {
            write_command(&mut device, &[0xb0])?;
            last_status = Instant::now();
        }
        persistence.flush_if_due();
        let fds: Vec<i32> = std::iter::once(device.as_raw_fd())
            .chain(listeners.iter().map(AsRawFd::as_raw_fd))
            .collect();
        if !poll_any(&fds, SESSION_POLL_MS)? {
            continue;
        }

        let mut newest = ParsedReport::default();
        for reader in std::iter::once(&mut device).chain(listeners.iter_mut()) {
            while poll_readable(reader.as_raw_fd(), 0)? {
                let mut report = [0u8; REPORT_LEN + 1];
                let count = reader.read(&mut report)?;
                if count == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "hidraw device returned EOF",
                    ));
                }
                let parsed = parse_report(&report[..count]);
                if parsed.mix.is_some() {
                    newest.mix = parsed.mix;
                }
                if parsed.online.is_some() {
                    newest.online = parsed.online;
                }
            }
        }

        if let Some(online) = newest.online {
            set_status(
                app,
                runtime,
                if online {
                    ArctisConnectionState::Connected
                } else {
                    ArctisConnectionState::Disconnected
                },
                Some(if online {
                    "Arctis Nova 7 wireless link is active".into()
                } else {
                    "Dongle detected; headset is powered off or out of range".into()
                }),
            );
            apply_auto_switch(app, machine, online);
        }

        if let Some((game, chat)) = newest.mix {
            let pair_changed = balance_pair(app).is_some_and(|(a, b)| {
                last_applied
                    .as_ref()
                    .is_none_or(|event| event.a != a || event.b != b)
            });
            let values_changed = last_applied
                .as_ref()
                .is_none_or(|event| event.a_volume != game || event.b_volume != chat);
            if pair_changed || values_changed {
                match apply_mix(app, game, chat) {
                    Ok(event) => {
                        last_applied = Some(event);
                        persistence.mark_dirty();
                    }
                    Err(e) => eprintln!("sink: applying hardware ChatMix failed: {e}"),
                }
            }
        }
    }
}

fn worker_loop(app: tauri::AppHandle, runtime: Arc<HardwareRuntime>) {
    let mut backoff = Duration::from_secs(1);
    let mut auto_switch = AutoSwitchState::default();
    loop {
        let prefs = worker_prefs(&app).unwrap_or(WorkerPrefs {
            enabled: false,
            auto_switch: false,
            output: None,
        });
        if !prefs.enabled {
            auto_switch.reset_observation();
            set_status(&app, &runtime, ArctisConnectionState::Disabled, None);
            runtime.wait(Duration::from_secs(30));
            backoff = Duration::from_secs(1);
            continue;
        }

        if !runtime.audio_ready() {
            auto_switch.reset_observation();
            set_status(
                &app,
                &runtime,
                ArctisConnectionState::Disconnected,
                Some("Waiting for Sink's virtual audio devices".into()),
            );
            runtime.wait(Duration::from_secs(1));
            backoff = Duration::from_secs(1);
            continue;
        }

        let paths = match discover() {
            Discovery::Found(paths) => paths,
            Discovery::Missing => {
                apply_auto_switch(&app, &mut auto_switch, false);
                set_status(
                    &app,
                    &runtime,
                    ArctisConnectionState::Disconnected,
                    Some("Arctis Nova 7 dongle not found".into()),
                );
                runtime.wait(backoff);
                backoff = (backoff * 2).min(Duration::from_secs(30));
                continue;
            }
            Discovery::Unsupported(detail) => {
                set_status(
                    &app,
                    &runtime,
                    ArctisConnectionState::Unsupported,
                    Some(detail),
                );
                runtime.wait(Duration::from_secs(30));
                continue;
            }
        };

        match run_device_session(&app, &runtime, &mut auto_switch, &paths) {
            Ok(()) => {
                backoff = Duration::from_secs(1);
            }
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
                set_status(
                    &app,
                    &runtime,
                    ArctisConnectionState::PermissionDenied,
                    Some(format!("Cannot open {}: {e}", paths.command.display())),
                );
                runtime.wait(backoff);
                backoff = (backoff * 2).min(Duration::from_secs(30));
            }
            Err(e) => {
                apply_auto_switch(&app, &mut auto_switch, false);
                set_status(
                    &app,
                    &runtime,
                    ArctisConnectionState::Disconnected,
                    Some(format!("{}: {e}", paths.command.display())),
                );
                runtime.wait(backoff);
                backoff = (backoff * 2).min(Duration::from_secs(30));
            }
        }
    }
}

pub fn spawn(app: tauri::AppHandle) {
    let runtime = app.state::<AppState>().hardware.clone();
    if let Err(e) = std::thread::Builder::new()
        .name("arctis-chatmix".into())
        .spawn(move || worker_loop(app, runtime))
    {
        eprintln!("sink: failed to start the Arctis ChatMix worker: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dial_report_maps_center_to_full_volume() {
        assert_eq!(parse_report(&[0x45, 100, 100]).mix, Some((100, 100)));
    }

    #[test]
    fn dial_report_maps_both_extremes() {
        assert_eq!(parse_report(&[0x45, 100, 0]).mix, Some((100, 0)));
        assert_eq!(parse_report(&[0x45, 0, 100]).mix, Some((0, 100)));
    }

    #[test]
    fn status_report_carries_wireless_state_and_mix() {
        let online = parse_report(&[0xb0, 0x03, 4, 0, 72, 100]);
        assert_eq!(online.online, Some(true));
        assert_eq!(online.mix, Some((72, 100)));
        assert_eq!(
            parse_report(&[0xb0, 0x02, 4, 0, 100, 100]).online,
            Some(false)
        );
    }

    #[test]
    fn values_are_bounded_to_sink_fader_range() {
        assert_eq!(parse_report(&[0x45, 255, 101]).mix, Some((100, 100)));
    }

    #[test]
    fn discovery_match_is_limited_to_product_and_interface() {
        let event = "HID_ID=0003:00001038:00002202\nHID_PHYS=usb-x/input3\n";
        assert!(uevent_matches(event));
        assert!(!uevent_matches(&event.replace("2202", "2206")));
        assert!(!uevent_matches(&event.replace("input3", "input4")));

        let dial = event.replace("input3", "input5");
        assert!(listener_matches(&dial, "usb-x/input5"));
        assert!(!listener_matches(&dial, "usb-other/input5"));
    }

    #[test]
    fn autoswitch_plan_claims_every_configured_channel() {
        let controlled =
            ["sink_game", "sink_chat", "sink_music", "sink_system"].map(str::to_string);
        let saved_outputs = HashMap::from([
            ("sink_game".to_string(), None),
            ("sink_chat".to_string(), Some("speakers".to_string())),
            ("sink_music".to_string(), None),
            ("sink_system".to_string(), Some("hdmi".to_string())),
        ]);

        let plan = auto_switch_output_plan(&controlled, &saved_outputs, Some("alsa_output.arctis"));

        assert_eq!(plan.len(), controlled.len());
        assert_eq!(
            plan.iter().map(|(name, _, _)| name).collect::<Vec<_>>(),
            controlled.iter().collect::<Vec<_>>()
        );
        assert!(plan
            .iter()
            .all(|(_, _, claimed)| claimed.as_deref() == Some("alsa_output.arctis")));
        assert_eq!(plan[1].1.as_deref(), Some("speakers"));
        assert_eq!(plan[3].1.as_deref(), Some("hdmi"));
    }

    #[test]
    fn first_online_observation_only_primes_restore_state() {
        let mut state = AutoSwitchState::default();
        let controlled = vec!["sink_game".to_string(), "sink_chat".to_string()];
        assert_eq!(
            state.observe(true, true, Some("speakers"), &controlled),
            AutoSwitchAction::Prime
        );
        assert_eq!(state.previous_default, None);
    }

    #[test]
    fn first_offline_observation_does_nothing() {
        let mut state = AutoSwitchState::default();
        let controlled = vec!["sink_game".to_string(), "sink_chat".to_string()];
        assert_eq!(
            state.observe(false, true, Some("speakers"), &controlled),
            AutoSwitchAction::None
        );
    }

    #[test]
    fn disconnect_and_reconnect_claim_and_release() {
        let mut state = AutoSwitchState::default();
        let controlled = vec!["sink_game".to_string(), "arctis".to_string()];
        assert_eq!(
            state.observe(false, true, Some("speakers"), &controlled),
            AutoSwitchAction::None
        );
        assert_eq!(
            state.observe(true, true, Some("speakers"), &controlled),
            AutoSwitchAction::Claim
        );
        assert_eq!(state.previous_default.as_deref(), Some("speakers"));
        assert_eq!(
            state.observe(false, true, Some("arctis"), &controlled),
            AutoSwitchAction::Release(Some("speakers".into()))
        );
        assert_eq!(
            state.observe(true, true, Some("speakers"), &controlled),
            AutoSwitchAction::Claim
        );
    }

    #[test]
    fn disconnect_releases_channels_without_undoing_a_manual_default_change() {
        let mut state = AutoSwitchState::default();
        let controlled = vec!["sink_game".to_string(), "sink_chat".to_string()];
        state.observe(false, true, Some("speakers"), &controlled);
        state.observe(true, true, Some("speakers"), &controlled);
        assert_eq!(
            state.observe(false, true, Some("usb_dac"), &controlled),
            AutoSwitchAction::Release(None)
        );
    }

    #[test]
    fn follow_default_restores_an_alternate_when_target_won_fallback() {
        assert_eq!(
            restore_output_for_claim(
                None,
                Some("alsa_output.arctis"),
                "alsa_output.arctis",
                Some("alsa_output.speakers"),
            ),
            Some("alsa_output.speakers".into())
        );
    }

    #[test]
    fn explicit_output_is_restored_exactly() {
        assert_eq!(
            restore_output_for_claim(
                Some("easyeffects_sink"),
                Some("alsa_output.speakers"),
                "alsa_output.arctis",
                Some("alsa_output.speakers"),
            ),
            Some("easyeffects_sink".into())
        );
    }

    #[test]
    fn release_only_restores_a_route_still_owned_by_hardware() {
        assert!(should_restore_claim(
            Some(Some("alsa_output.arctis")),
            "alsa_output.arctis"
        ));
        assert!(!should_restore_claim(
            Some(Some("alsa_output.speakers")),
            "alsa_output.arctis"
        ));
        assert!(should_restore_claim(None, "alsa_output.arctis"));
    }
}
