import { useEffect, useRef } from "react";
import { listen } from "@tauri-apps/api/event";
import {
  useMixerStore,
  type HardwareChatMixEvent,
  type Levels,
} from "../store/mixer";

const POLL_INTERVAL_MS = 2000;

/**
 * Boots the audio layer: creates virtual sinks, then keeps app/device state
 * and live levels polled while the window is visible.
 */
export function useAudio() {
  const initialize = useMixerStore((s) => s.initialize);
  const fetchAppStreams = useMixerStore((s) => s.fetchAppStreams);
  const fetchOutputs = useMixerStore((s) => s.fetchOutputs);
  const fetchSeenApps = useMixerStore((s) => s.fetchSeenApps);
  const fetchChannels = useMixerStore((s) => s.fetchChannels);
  const setLevels = useMixerStore((s) => s.setLevels);
  const applyHardwareChatMix = useMixerStore((s) => s.applyHardwareChatMix);
  const outputDevices = useMixerStore((s) => s.outputDevices);
  const profiles = useMixerStore((s) => s.profiles);
  const loadProfile = useMixerStore((s) => s.loadProfile);

  useEffect(() => {
    void initialize();
    let id: ReturnType<typeof setInterval> | undefined;
    const poll = () => {
      void fetchAppStreams();
      void fetchOutputs();
      void fetchSeenApps();
    };
    const start = () => {
      if (id === undefined) {
        poll(); // refresh immediately so a returning window isn't stale
        id = setInterval(poll, POLL_INTERVAL_MS);
      }
    };
    const stop = () => {
      if (id !== undefined) {
        clearInterval(id);
        id = undefined;
      }
    };
    // Pause polling while hidden in the tray - the product's dominant idle
    // state. Routing stays enforced by a ticker, so this only stops UI refresh.
    const onVisibility = () => (document.hidden ? stop() : start());
    if (!document.hidden) start();
    document.addEventListener("visibilitychange", onVisibility);
    return () => {
      stop();
      document.removeEventListener("visibilitychange", onVisibility);
    };
  }, [initialize, fetchAppStreams, fetchOutputs, fetchSeenApps]);

  useEffect(() => {
    const unlisten = listen<Levels>("levels", (event) => setLevels(event.payload));
    return () => {
      void unlisten.then((fn) => fn());
    };
  }, [setLevels]);

  // Hardware reports already changed PipeWire and Rust's in-memory model.
  // Mirror them locally without calling setChannelVolume, which would send
  // the same update back through IPC and create redundant work.
  useEffect(() => {
    const unlisten = listen<HardwareChatMixEvent>("arctis-chatmix", (event) => {
      applyHardwareChatMix(event.payload);
    });
    return () => {
      void unlisten.then((fn) => fn());
    };
  }, [applyHardwareChatMix]);

  // Keyboard-wide volume actions run in the Rust process even while the main
  // window is hidden. Refresh once per key event so every visible slider stays
  // in sync with the values already applied and persisted by the backend.
  useEffect(() => {
    const unlisten = listen("channel-group-changed", () => {
      void fetchChannels();
    });
    return () => {
      void unlisten.then((fn) => fn());
    };
  }, [fetchChannels]);

  // Profile switched from the tray menu - sync the whole UI.
  const onProfileChanged = useMixerStore((s) => s.onProfileChanged);
  useEffect(() => {
    const unlisten = listen<string>("profile-changed", (event) => {
      void onProfileChanged(event.payload);
    });
    return () => {
      void unlisten.then((fn) => fn());
    };
  }, [onProfileChanged]);

  // A hotkey moved the balance in the backend; the strips need the volumes.
  useEffect(() => {
    const unlisten = listen("channels-changed", () => void fetchChannels());
    return () => {
      void unlisten.then((fn) => fn());
    };
  }, [fetchChannels]);

  // Hardware profile auto-switch: when a device with a bound profile
  // appears, load that profile (Sonar-style).
  const seenDevices = useRef<Set<string> | null>(null);
  useEffect(() => {
    const names = new Set(outputDevices.map((d) => d.name));
    if (seenDevices.current === null) {
      // First sample: just learn the current device set.
      if (names.size > 0) seenDevices.current = names;
      return;
    }
    for (const name of names) {
      if (!seenDevices.current.has(name)) {
        const bound = profiles.find((p) => p.trigger_device === name);
        if (bound) void loadProfile(bound.name);
      }
    }
    seenDevices.current = names;
  }, [outputDevices, profiles, loadProfile]);
}
