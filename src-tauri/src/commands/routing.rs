use std::collections::HashMap;

use serde::Serialize;
use tauri::State;

use crate::audio::types::is_virtual_sink;
use crate::state::AppState;

pub(crate) const MAX_VOLUME: u8 = 150;
const CHANNEL_GROUP_STEP: u8 = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelGroupAction {
    Increase,
    Decrease,
    ToggleMute,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ChannelGroupResult {
    pub level_percent: u8,
    pub muted: bool,
}

pub fn channel_group_action_from_args(args: &[String]) -> Option<ChannelGroupAction> {
    args.iter().find_map(|arg| match arg.as_str() {
        "--channels-volume-up" => Some(ChannelGroupAction::Increase),
        "--channels-volume-down" => Some(ChannelGroupAction::Decrease),
        "--channels-volume-mute" => Some(ChannelGroupAction::ToggleMute),
        _ => None,
    })
}

fn scaled_channel_volumes(
    current: &[(String, u8)],
    action: ChannelGroupAction,
) -> Vec<(String, u8)> {
    let current_max = current.iter().map(|(_, volume)| *volume).max().unwrap_or(0);
    let target_max = match action {
        ChannelGroupAction::Increase => current_max
            .saturating_add(CHANNEL_GROUP_STEP)
            .min(MAX_VOLUME),
        ChannelGroupAction::Decrease => current_max.saturating_sub(CHANNEL_GROUP_STEP),
        ChannelGroupAction::ToggleMute => current_max,
    };

    current
        .iter()
        .map(|(name, volume)| {
            let scaled = if current_max == 0 {
                if action == ChannelGroupAction::Increase {
                    CHANNEL_GROUP_STEP
                } else {
                    0
                }
            } else {
                let numerator =
                    u16::from(*volume) * u16::from(target_max) + u16::from(current_max) / 2;
                (numerator / u16::from(current_max)) as u8
            };
            (name.clone(), scaled.min(MAX_VOLUME))
        })
        .collect()
}

fn apply_channel_volumes(
    state: &AppState,
    current: &[(String, u8)],
    target: &[(String, u8)],
) -> Result<(), String> {
    let mut applied: Vec<(String, u8)> = Vec::new();
    for ((name, previous), (_, next)) in current.iter().zip(target) {
        if previous == next {
            continue;
        }
        if let Err(error) = state.backend.set_sink_volume(name, *next) {
            for (applied_name, applied_previous) in applied.into_iter().rev() {
                let _ = state
                    .backend
                    .set_sink_volume(&applied_name, applied_previous);
            }
            return Err(error.to_string());
        }
        applied.push((name.clone(), *previous));
    }
    Ok(())
}

fn apply_channel_mutes(
    state: &AppState,
    current: &[(String, bool)],
    target: &[(String, bool)],
) -> Result<(), String> {
    let mut applied: Vec<(String, bool)> = Vec::new();
    for ((name, previous), (_, next)) in current.iter().zip(target) {
        if previous == next {
            continue;
        }
        if let Err(error) = state.backend.set_sink_mute(name, *next) {
            for (applied_name, applied_previous) in applied.into_iter().rev() {
                let _ = state.backend.set_sink_mute(&applied_name, applied_previous);
            }
            return Err(error.to_string());
        }
        applied.push((name.clone(), *previous));
    }
    Ok(())
}

pub fn apply_channel_group_action(
    state: &AppState,
    action: ChannelGroupAction,
) -> Result<ChannelGroupResult, String> {
    let _gate = state
        .channel_group_gate
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);

    match action {
        ChannelGroupAction::Increase | ChannelGroupAction::Decrease => {
            let (initialized, current, muted) = {
                let mixer = state.lock_mixer()?;
                (
                    mixer.initialized,
                    mixer
                        .channels
                        .iter()
                        .map(|channel| (channel.name.clone(), channel.volume_percent))
                        .collect::<Vec<_>>(),
                    mixer.channels.iter().all(|channel| channel.muted),
                )
            };
            if !initialized || current.is_empty() {
                return Err("Sink channels are not ready".to_string());
            }
            let target = scaled_channel_volumes(&current, action);
            apply_channel_volumes(state, &current, &target)?;

            let defs = {
                let mut mixer = state.lock_mixer()?;
                for (name, volume) in &target {
                    if let Some(channel) = mixer.channel_mut(name) {
                        channel.volume_percent = *volume;
                    }
                    mixer.channel_defs.set_volume(name, *volume);
                }
                crate::commands::profiles::autosave_active(&mixer);
                mixer.channel_defs.clone()
            };
            defs.save().map_err(|error| error.to_string())?;
            Ok(ChannelGroupResult {
                level_percent: target.iter().map(|(_, volume)| *volume).max().unwrap_or(0),
                muted,
            })
        }
        ChannelGroupAction::ToggleMute => {
            let (initialized, current, level_percent) = {
                let mixer = state.lock_mixer()?;
                (
                    mixer.initialized,
                    mixer
                        .channels
                        .iter()
                        .map(|channel| (channel.name.clone(), channel.muted))
                        .collect::<Vec<_>>(),
                    mixer
                        .channels
                        .iter()
                        .map(|channel| channel.volume_percent)
                        .max()
                        .unwrap_or(0),
                )
            };
            if !initialized || current.is_empty() {
                return Err("Sink channels are not ready".to_string());
            }

            let current_map = current.iter().cloned().collect::<HashMap<_, _>>();
            let previous_restore = state
                .channel_group_mute_restore
                .lock()
                .map_err(|_| "channel group mute lock poisoned".to_string())?
                .clone();
            let (target, next_restore) = if let Some(restore) = previous_restore {
                (
                    current
                        .iter()
                        .map(|(name, _)| {
                            (name.clone(), restore.get(name).copied().unwrap_or(false))
                        })
                        .collect::<Vec<_>>(),
                    None,
                )
            } else if current.iter().all(|(_, muted)| *muted) {
                (
                    current
                        .iter()
                        .map(|(name, _)| (name.clone(), false))
                        .collect(),
                    None,
                )
            } else {
                (
                    current
                        .iter()
                        .map(|(name, _)| (name.clone(), true))
                        .collect(),
                    Some(current_map),
                )
            };
            apply_channel_mutes(state, &current, &target)?;

            let defs = {
                let mut mixer = state.lock_mixer()?;
                for (name, muted) in &target {
                    if let Some(channel) = mixer.channel_mut(name) {
                        channel.muted = *muted;
                    }
                    mixer.channel_defs.set_muted(name, *muted);
                }
                crate::commands::profiles::autosave_active(&mixer);
                mixer.channel_defs.clone()
            };
            *state
                .channel_group_mute_restore
                .lock()
                .map_err(|_| "channel group mute lock poisoned".to_string())? = next_restore;
            defs.save().map_err(|error| error.to_string())?;
            Ok(ChannelGroupResult {
                level_percent,
                muted: target.iter().all(|(_, muted)| *muted),
            })
        }
    }
}

/// An app's other live streams: same identity, different stream. Routing is
/// per app, so these move with the one the user clicked.
fn siblings_of<'a>(
    streams: &'a [crate::audio::types::AppStream],
    stream: &crate::audio::types::AppStream,
) -> Vec<&'a crate::audio::types::AppStream> {
    streams
        .iter()
        .filter(|s| {
            s.index != stream.index
                && s.match_prop == stream.match_prop
                && s.match_value == stream.match_value
        })
        .collect()
}

/// Move an app stream onto a channel; empty `sink_name` unassigns it.
/// Persisted so the assignment reapplies when the app restarts.
#[tauri::command]
pub fn route_app_to_channel(
    state: State<'_, AppState>,
    stream_index: u32,
    sink_name: String,
) -> Result<(), String> {
    route_app(state.inner(), stream_index, &sink_name)
}

pub fn route_app(state: &AppState, stream_index: u32, sink_name: &str) -> Result<(), String> {
    if !sink_name.is_empty() && !is_virtual_sink(sink_name) {
        return Err(format!("unknown channel: {sink_name}"));
    }

    // Assignments are per app, not per stream: siblings move too, and the
    // rule is keyed on the identity the user saw, not the raw stream.
    let streams = crate::commands::devices::live_streams(state)?;
    let Some(stream) = streams.iter().find(|s| s.index == stream_index) else {
        // Vanished between the click and now; move the raw index anyway in
        // case the listing raced, with nothing to record against it.
        return state
            .backend
            .move_stream_to_sink(stream_index, sink_name)
            .map_err(|e| e.to_string());
    };
    let siblings: Vec<&crate::audio::types::AppStream> = siblings_of(&streams, stream);

    // Record intent before moving, or a concurrent tick undoes the move.
    let assignments = {
        let mut mixer = state.lock_mixer()?;
        if sink_name.is_empty() {
            mixer
                .assignments
                .remove(&stream.match_prop, &stream.match_value);
        } else {
            mixer
                .assignments
                .set(&stream.match_prop, &stream.match_value, sink_name);
        }
        // The user explicitly placed these streams; don't auto-route them
        // again.
        mixer.auto_routed.insert(stream.serial);
        mixer.auto_routed.extend(siblings.iter().map(|s| s.serial));
        crate::commands::profiles::autosave_active(&mixer);
        mixer.assignments.clone()
    };

    state
        .backend
        .move_stream_to_sink(stream_index, sink_name)
        .map_err(|e| e.to_string())?;
    for sibling in &siblings {
        if let Err(e) = state.backend.move_stream_to_sink(sibling.index, sink_name) {
            eprintln!(
                "sink: moving {} (#{}) failed: {e}",
                stream.app_name, sibling.index
            );
        }
    }

    assignments.save().map_err(|e| e.to_string())?;
    Ok(())
}

/// Set a channel's volume (0-150%).
#[tauri::command]
pub fn set_channel_volume(
    state: State<'_, AppState>,
    sink_name: String,
    volume: u8,
) -> Result<(), String> {
    // Only our own channels, so a compromised webview can't touch arbitrary
    // session sinks.
    if !is_virtual_sink(&sink_name) {
        return Err(format!("unknown channel: {sink_name}"));
    }
    let volume = volume.min(MAX_VOLUME);
    state
        .backend
        .set_sink_volume(&sink_name, volume)
        .map_err(|e| e.to_string())?;

    let defs = {
        let mut mixer = state.lock_mixer()?;
        if let Some(channel) = mixer.channel_mut(&sink_name) {
            channel.volume_percent = volume;
        }
        // Persist the level itself, not just into an active profile - a channel
        // must come back at its last volume even when no profile is bound.
        mixer.channel_defs.set_volume(&sink_name, volume);
        crate::commands::profiles::autosave_active(&mixer);
        mixer.channel_defs.clone()
    };
    defs.save().map_err(|e| e.to_string())
}

/// Mute or unmute a channel.
#[tauri::command]
pub fn toggle_channel_mute(
    state: State<'_, AppState>,
    sink_name: String,
    muted: bool,
) -> Result<(), String> {
    if !is_virtual_sink(&sink_name) {
        return Err(format!("unknown channel: {sink_name}"));
    }
    state
        .backend
        .set_sink_mute(&sink_name, muted)
        .map_err(|e| e.to_string())?;

    let defs = {
        let mut mixer = state.lock_mixer()?;
        if let Some(channel) = mixer.channel_mut(&sink_name) {
            channel.muted = muted;
        }
        mixer.channel_defs.set_muted(&sink_name, muted);
        crate::commands::profiles::autosave_active(&mixer);
        mixer.channel_defs.clone()
    };
    *state
        .channel_group_mute_restore
        .lock()
        .map_err(|_| "channel group mute lock poisoned".to_string())? = None;
    defs.save().map_err(|e| e.to_string())
}

/// Listen to a channel/mix/mic on the default output (session scoped -
/// not persisted, cleared on restart).
#[tauri::command]
pub fn set_monitor(
    state: State<'_, AppState>,
    sink_name: String,
    enabled: bool,
) -> Result<(), String> {
    // Monitoring is scoped to our own nodes: a channel, a mix bus, or the mic -
    // not any arbitrary session sink.
    {
        let mixer = state.lock_mixer()?;
        let known = sink_name == "sink_mic"
            || mixer
                .channel_defs
                .channels
                .iter()
                .any(|c| c.name == sink_name)
            || mixer.buses.buses.iter().any(|b| b.name == sink_name);
        if !known {
            return Err(format!("unknown monitor target: {sink_name}"));
        }
    }
    state
        .backend
        .set_monitor(&sink_name, enabled)
        .map_err(|e| e.to_string())
}

/// Set or clear a persistent display name for an app, keyed by its stream
/// identity. An empty `alias` reverts to the discovered name.
#[tauri::command]
pub fn rename_app(
    state: State<'_, AppState>,
    match_prop: String,
    match_value: String,
    alias: String,
) -> Result<(), String> {
    let aliases = {
        let mut mixer = state.lock_mixer()?;
        mixer.aliases.set(&match_prop, &match_value, &alias);
        mixer.aliases.clone()
    };
    aliases.save().map_err(|e| e.to_string())
}

/// Set the volume of a single app stream (0-150%).
#[tauri::command]
pub fn set_app_volume(
    state: State<'_, AppState>,
    stream_index: u32,
    volume: u8,
) -> Result<(), String> {
    state
        .backend
        .set_app_volume(stream_index, volume.min(MAX_VOLUME))
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::mock::{stream as mock_stream, Call, MockBackend};
    use crate::audio::types::AppStream;
    use crate::persistence::testing::TempConfig;
    use std::sync::Arc;

    /// An AppState on a private config root, channels created, with one
    /// saved assignment for Firefox.
    fn app(backend: Arc<MockBackend>) -> AppState {
        let state = AppState::new(backend, true);
        {
            let mut mixer = state.lock_mixer().expect("mixer");
            mixer.init_defaults();
            mixer
                .assignments
                .set("application.name", "Firefox", "sink_game");
        }
        state
    }

    #[test]
    fn routing_records_the_assignment_before_it_moves_anything() {
        let _cfg = TempConfig::new("route-order");
        let backend = Arc::new(MockBackend::with_streams(vec![mock_stream(
            7, 100, "Firefox", None,
        )]));
        let state = Arc::new(app(backend.clone()));

        // Observe from inside the move: the ticker runs concurrently, so the
        // assignment and ledger must already reflect this stream's new home.
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (probe, probed_state) = (Arc::clone(&seen), Arc::clone(&state));
        backend.on_move(move |_, _| {
            let mixer = probed_state.lock_mixer().expect("mixer");
            probe.lock().expect("probe").push((
                mixer
                    .assignments
                    .sink_for("application.name", "Firefox")
                    .map(str::to_string),
                mixer.auto_routed.contains(&100),
            ));
        });

        route_app(&state, 7, "sink_chat").expect("routes");

        assert_eq!(
            seen.lock().expect("probe").as_slice(),
            [(Some("sink_chat".to_string()), true)],
            "assignment and ledger must be recorded before the move"
        );
    }

    #[test]
    fn routing_moves_every_stream_of_the_app() {
        let _cfg = TempConfig::new("route-siblings");
        let backend = Arc::new(MockBackend::with_streams(vec![
            mock_stream(7, 100, "Firefox", None),
            mock_stream(8, 101, "Firefox", None),
            mock_stream(9, 102, "Spotify", None),
        ]));
        let state = app(backend.clone());

        route_app(&state, 7, "sink_chat").expect("routes");

        let mut moved: Vec<u32> = backend.moves().into_iter().map(|(i, _)| i).collect();
        moved.sort_unstable();
        assert_eq!(moved, vec![7, 8], "siblings follow, other apps don't");
        assert!(backend.moves().iter().all(|(_, s)| s == "sink_chat"));

        // Every moved stream is ledgered, so the ticker leaves them alone.
        let mixer = state.lock_mixer().expect("mixer");
        assert!(mixer.auto_routed.contains(&100) && mixer.auto_routed.contains(&101));
        assert!(!mixer.auto_routed.contains(&102));
    }

    #[test]
    fn unrouting_clears_the_assignment() {
        let _cfg = TempConfig::new("route-clear");
        let backend = Arc::new(MockBackend::with_streams(vec![mock_stream(
            7,
            100,
            "Firefox",
            Some("sink_game"),
        )]));
        let state = app(backend.clone());

        route_app(&state, 7, "").expect("unroutes");

        assert_eq!(backend.moves(), vec![(7, String::new())]);
        assert!(state
            .lock_mixer()
            .expect("mixer")
            .assignments
            .sink_for("application.name", "Firefox")
            .is_none());
    }

    #[test]
    fn routing_rejects_a_sink_that_is_not_one_of_ours() {
        let _cfg = TempConfig::new("route-reject");
        let backend = Arc::new(MockBackend::with_streams(vec![mock_stream(
            7, 100, "Firefox", None,
        )]));
        let state = app(backend.clone());

        assert!(route_app(&state, 7, "alsa_output.hw_0").is_err());
        assert!(backend.moves().is_empty(), "nothing moves on a bad target");
    }

    fn stream(index: u32, prop: &str, value: &str) -> AppStream {
        AppStream {
            index,
            serial: u64::from(index) + 1000,
            app_name: value.to_string(),
            match_prop: prop.into(),
            match_value: value.into(),
            alias: None,
            icon_name: None,
            icon_path: None,
            pid: None,
            assigned_sink: None,
            volume_percent: 100,
            muted: false,
            active: true,
            props: Default::default(),
            settled: true,
        }
    }

    #[test]
    fn siblings_are_the_apps_other_streams() {
        let streams = vec![
            stream(1, "application.name", "Firefox"),
            stream(2, "application.name", "Firefox"),
            stream(3, "application.name", "Spotify"),
            // Same value under a different property is a different identity.
            stream(4, "application.process.binary", "Firefox"),
        ];
        let picked = siblings_of(&streams, &streams[0]);
        let indices: Vec<u32> = picked.iter().map(|s| s.index).collect();
        assert_eq!(indices, vec![2], "only same-identity streams, never itself");

        assert!(siblings_of(&streams, &streams[2]).is_empty());
    }

    #[test]
    fn channel_group_cli_flags_are_narrow() {
        assert_eq!(
            channel_group_action_from_args(&["sink".into(), "--channels-volume-up".into()]),
            Some(ChannelGroupAction::Increase)
        );
        assert_eq!(
            channel_group_action_from_args(&["sink".into(), "--channels-volume-down".into()]),
            Some(ChannelGroupAction::Decrease)
        );
        assert_eq!(
            channel_group_action_from_args(&["sink".into(), "--channels-volume-mute".into()]),
            Some(ChannelGroupAction::ToggleMute)
        );
        assert_eq!(
            channel_group_action_from_args(&["sink".into(), "--minimized".into()]),
            None
        );
    }

    #[test]
    fn grouped_volume_preserves_channel_ratios_across_a_step() {
        let current = vec![
            ("sink_game".to_string(), 100),
            ("sink_chat".to_string(), 60),
            ("sink_music".to_string(), 40),
            ("sink_system".to_string(), 0),
        ];
        let down = scaled_channel_volumes(&current, ChannelGroupAction::Decrease);
        assert_eq!(
            down,
            vec![
                ("sink_game".to_string(), 95),
                ("sink_chat".to_string(), 57),
                ("sink_music".to_string(), 38),
                ("sink_system".to_string(), 0),
            ]
        );
        assert_eq!(
            scaled_channel_volumes(&down, ChannelGroupAction::Increase),
            current
        );
    }

    #[test]
    fn grouped_volume_updates_backend_memory_and_persistence() {
        let cfg = TempConfig::new("channel-group-volume");
        let backend = Arc::new(MockBackend::default());
        let state = app(backend.clone());
        {
            let mut mixer = state.lock_mixer().expect("mixer");
            for (name, volume) in [
                ("sink_game", 100),
                ("sink_chat", 60),
                ("sink_music", 40),
                ("sink_system", 0),
            ] {
                mixer.channel_mut(name).expect("channel").volume_percent = volume;
                mixer.channel_defs.set_volume(name, volume);
            }
        }

        let result = apply_channel_group_action(&state, ChannelGroupAction::Decrease)
            .expect("group volume applies");
        assert_eq!(result.level_percent, 95);
        let mixer = state.lock_mixer().expect("mixer");
        let volumes = mixer
            .channels
            .iter()
            .map(|channel| channel.volume_percent)
            .collect::<Vec<_>>();
        assert_eq!(volumes, vec![95, 57, 38, 0]);
        drop(mixer);

        let calls = backend.calls();
        assert!(calls.contains(&Call::SetSinkVolume {
            name: "sink_game".into(),
            percent: 95,
        }));
        assert!(calls.contains(&Call::SetSinkVolume {
            name: "sink_chat".into(),
            percent: 57,
        }));
        assert!(calls.contains(&Call::SetSinkVolume {
            name: "sink_music".into(),
            percent: 38,
        }));
        assert_eq!(
            crate::persistence::channels::Channels::load()
                .get("sink_chat")
                .expect("persisted channel")
                .volume_percent,
            57
        );
        drop(cfg);
    }

    #[test]
    fn group_mute_restores_each_channels_previous_state() {
        let _cfg = TempConfig::new("channel-group-mute");
        let backend = Arc::new(MockBackend::default());
        let state = app(backend);
        {
            let mut mixer = state.lock_mixer().expect("mixer");
            mixer.channel_mut("sink_chat").expect("chat").muted = true;
            mixer.channel_defs.set_muted("sink_chat", true);
        }

        let muted = apply_channel_group_action(&state, ChannelGroupAction::ToggleMute)
            .expect("group mute applies");
        assert!(muted.muted);
        assert!(state
            .lock_mixer()
            .expect("mixer")
            .channels
            .iter()
            .all(|channel| channel.muted));

        let restored = apply_channel_group_action(&state, ChannelGroupAction::ToggleMute)
            .expect("group mute restores");
        assert!(!restored.muted);
        let mixer = state.lock_mixer().expect("mixer");
        let states = mixer
            .channels
            .iter()
            .map(|channel| (channel.name.as_str(), channel.muted))
            .collect::<Vec<_>>();
        assert_eq!(
            states,
            vec![
                ("sink_game", false),
                ("sink_chat", true),
                ("sink_music", false),
                ("sink_system", false),
            ]
        );
    }
}
