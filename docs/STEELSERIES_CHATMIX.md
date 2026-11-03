# SteelSeries Arctis Nova 7 hardware ChatMix

Sink can read the physical ChatMix wheel and the real wireless headset state
from an Arctis Nova 7 Gen 1 dongle (`1038:2202`). Both features are disabled by
default. They run in Sink's existing backend process, including while the main
window is hidden in the tray; no root daemon is used.

## Protocol and attribution

The HID report framing, Nova 7 command interface 3, additional dial listener
interface 5, session/query commands, and field offsets in
`src-tauri/src/hardware/mod.rs` are adapted from
[`rdamron/rust-arctis-chatmix`](https://github.com/rdamron/rust-arctis-chatmix),
GPL-3.0-only. That project attributes the reverse-engineered device definitions
to [`elegos/Linux-Arctis-Manager`](https://github.com/elegos/Linux-Arctis-Manager),
also GPL-3.0. Sink remains GPL-3.0-only.

The implementation writes only the documented session/query sequence needed to
receive ChatMix and power-status reports. It does not write EQ, sidetone, gain,
microphone, auto-off, wireless-mode, or other stored headset preferences.

## Permissions

The Debian package includes
`/usr/lib/udev/rules.d/70-sink-arctis-nova7.rules`. The rule is deliberately
limited to hidraw nodes belonging to USB `1038:2202` and uses `TAG+="uaccess"`
for the active local seat. It does not use a global hidraw mode or grant access
to unrelated SteelSeries devices.

Installing the package needs administrator authorization. After installation,
reload udev and reconnect the dongle (or reboot) before enabling hardware
ChatMix. Do not copy a broader rule into `/etc/udev/rules.d`.

## Easy Effects

The hardware settings list the same physical outputs exposed by Sink's channel
output selectors, including Easy Effects Sink when it is running. Selecting
Easy Effects sends every configured channel into its input sink. Easy Effects'
own output must remain a physical device; routing it back to a Sink channel
would form a processing loop. Sink does not modify Easy Effects configuration.

## Auto-switch safety

Auto-switch reacts only after the first confirmed wireless state sample. Merely
starting or quitting the GUI does not change an output. If Sink starts while
the headset is already online, that first sample only prepares a guarded
restoration snapshot for the first disconnect. On a real
disconnected-to-connected transition, Sink remembers the prior system default,
activates the configured destination for every channel, and makes Game the
system default. It also snapshots the effective physical destination of each
claimed channel. On disconnect it restores that exact destination when the channel is
still on the automatically selected output and its saved setting has not
changed. This matters when "System default" would otherwise select a connected
dongle whose headset is powered off. It restores the remembered system default
only if the current default is still one of Sink's channels. Application
assignments remain on Sink's channels throughout both transitions.

The HID worker does not process reports until the virtual channels and their
routes have finished initializing. This prevents startup reports from racing
channel creation or applying a stale ChatMix value to a sink that does not yet
exist.
