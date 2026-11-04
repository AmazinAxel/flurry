#!/usr/bin/env bash
# Captures golden screenshots of flurry inside an isolated headless sway.
# Isolation: fake HOME, own runtime dir, own session bus, mocked system bus (iwd/bluez/upower/logind),
# fake backlight, no pipewire, no PAM configs, fake gpu-screen-recorder. Never touches the live session.
# usage: tests/capture/run.sh [flurry-binary] [out-dir]
# flurry-binary: the `flurry` client (result/bin/flurry, target/release/flurry); it starts the sibling flurry-shell
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)

if [[ ${1:-} != __inner ]]; then
  flurry=$(realpath "$(command -v "${1:-flurry}")")
  out=$(realpath -m "${2:-$here/../golden}")
  mkdir -p "$out"
  tools='with import <nixpkgs> {}; buildEnv { name = "flurry-capture"; paths = [
    grim wtype imagemagick dbus glib.bin bubblewrap libnotify (python3.withPackages (p: [ p.python-dbusmock ])) ]; }'
  exec nix shell --impure --expr "$tools" -c dbus-run-session -- "$0" __inner "$flurry" "$out"
fi

flurry=$2 out=$3
tmp=$(mktemp -d /tmp/flc.XXXX)
trap 'mkdir -p "$out/.logs"; cp "$tmp"/*.log "$out/.logs/" 2>/dev/null; kill $(jobs -p) 2>/dev/null; rm -rf "$tmp"' EXIT

# Fake home
H=$tmp/home
mkdir -p "$H/.local/share" "$H/.config" "$H/Wallpapers" "$H/Downloads" "$H/.local/state/flurry" "$H/.password-store/web" "$tmp/run" "$tmp/pw"
chmod 700 "$tmp/run"
magick -size 2560x1600 gradient:'#2e3440-#5e81ac' -swirl 90 "$H/Wallpapers/dark.jpg"
date +%s000 > "$H/.local/state/flurry/lastSync" # just synced: no reminder in the shots
for e in github.com mail.example.org web/forum.example.net web/shop.example.com; do : > "$H/.password-store/$e.gpg"; done

# Fake backlight
B=$tmp/backlight/intel_backlight
mkdir -p "$B"
echo 1000 > "$B/max_brightness"
echo 600 > "$B/brightness"

export HOME=$H XDG_RUNTIME_DIR=$tmp/run TZ=UTC LANG=C.UTF-8 LC_ALL=C.UTF-8
export MPD_HOST=127.0.0.1 MPD_PORT=1 # refused: never the live mpd
# Safety: everything below must talk to the private session bus from dbus-run-session, never the live one
[[ ${DBUS_SESSION_BUS_ADDRESS:-} == *"/run/user/"* || -z ${DBUS_SESSION_BUS_ADDRESS:-} ]] && { echo "refusing: not on a private session bus" >&2; exit 1; }
unset XDG_CONFIG_HOME XDG_DATA_HOME XDG_STATE_HOME XDG_CACHE_HOME WAYLAND_DISPLAY SWAYSOCK DISPLAY I3SOCK
# clipboard history: entry files plus the index (newest first)
C=$H/.cache/flurry/clipboard url=https://example.org/a/very/long/link/that/should/wrap/inside/the/clipboard/list/item
mkdir -p "$C"
printf 'hello from the clipboard' > "$C/1"
printf '%s' "$url" > "$C/2"
printf 'second line\nthird line' > "$C/3"
magick -size 320x200 gradient:"#bf616a-#5e81ac" "png:$C/4.png"
printf '4\timage/png\t%s\t320x200\n3\ttext/plain\t%s\tsecond line third line\n2\ttext/plain\t%s\t%s\n1\ttext/plain\t%s\thello from the clipboard\n' \
  "$(stat -c %s "$C/4.png")" "$(stat -c %s "$C/3")" "$(stat -c %s "$C/2")" "$url" "$(stat -c %s "$C/1")" > "$C/index"

mkdir -p "$tmp/bin"
printf '#!/bin/sh\nexit 0\n' > "$tmp/bin/gpu-screen-recorder"
chmod +x "$tmp/bin/gpu-screen-recorder"
# sway runs it once its IPC is up: it hands back the socket and display (the waits below read FIFOs)
# shellcheck disable=SC2016 # expanded by the script, not here
printf '#!/bin/sh\necho "$SWAYSOCK $WAYLAND_DISPLAY" > %q\n' "$tmp/sway.ready" > "$tmp/bin/sway-ready"
chmod +x "$tmp/bin/sway-ready"
mkfifo "$tmp/sys.ready" "$tmp/sway.ready"

cat > "$tmp/sway.conf" <<EOF
output HEADLESS-1 resolution 2560x1600 scale 2
default_border none
seat * hide_cursor 1
exec $tmp/bin/sway-ready
EOF

# Mocked system bus
cat > "$tmp/sysbus.conf" <<EOF
<busconfig><type>system</type><listen>unix:path=$tmp/sys</listen><auth>EXTERNAL</auth>
<policy context="default"><allow send_destination="*" eavesdrop="true"/><allow eavesdrop="true"/><allow own="*"/></policy></busconfig>
EOF
# the address is printed once it listens (EOF instead if it fails)
dbus-daemon --config-file="$tmp/sysbus.conf" --nofork --print-address > "$tmp/sys.ready" &
read -r _ < "$tmp/sys.ready"
export DBUS_SYSTEM_BUS_ADDRESS=unix:path=$tmp/sys
sysmock() { dbus-send --system --print-reply --dest="$1" "$2" "${@:3}" >/dev/null; }

# each mock is ready once it owns its name (calls before its main loop runs wait in the queue)
python3 -m dbusmock --system --template upower > "$tmp/mock.log" 2>&1 &
python3 -m dbusmock --system --template logind >> "$tmp/mock.log" 2>&1 &
python3 -m dbusmock --system --template bluez5 >> "$tmp/mock.log" 2>&1 &
PYTHONPATH=$here python3 -m dbusmock --system --template "$here/iwd.py" >> "$tmp/mock.log" 2>&1 &
for name in org.freedesktop.UPower org.freedesktop.login1 org.bluez net.connman.iwd; do gdbus wait --timeout 30 --system "$name"; done

# upower display device: type=battery(2) state=discharging(2) 45% present, icon
sysmock org.freedesktop.UPower /org/freedesktop/UPower org.freedesktop.DBus.Mock.SetupDisplayDevice \
  uint32:2 uint32:2 double:45 double:20 double:50 double:10 int64:3600 int64:0 boolean:true string:battery-level-40-symbolic uint32:1
sysmock org.bluez / org.bluez.Mock.AddAdapter string:hci0 string:flurry
sysmock org.bluez / org.bluez.Mock.AddDevice string:hci0 string:11:22:33:44:55:66 string:S80A
sysmock org.bluez / org.bluez.Mock.AddDevice string:hci0 string:AA:BB:CC:DD:EE:FF string:MOU-302
sysmock org.bluez / org.bluez.Mock.PairDevice string:hci0 string:11:22:33:44:55:66
sysmock org.bluez / org.bluez.Mock.PairDevice string:hci0 string:AA:BB:CC:DD:EE:FF
sysmock org.bluez / org.bluez.Mock.ConnectDevice string:hci0 string:11:22:33:44:55:66

# Sway + flurry, with sysfs/PAM faked (shot() records the real clock)
# a sway that dies before its ready line closes the FIFO instead, so the read ends with EOF
{
  bwrap --die-with-parent --dev-bind / / --bind "$tmp/backlight" /sys/class/backlight --tmpfs /etc/pam.d \
    env PIPEWIRE_RUNTIME_DIR="$tmp/pw" PULSE_RUNTIME_PATH="$tmp/pw" WLR_BACKENDS=headless WLR_LIBINPUT_NO_DEVICES=1 \
    PATH="$tmp/bin:$PATH" sway -c "$tmp/sway.conf" > "$tmp/sway.log" 2>&1
  : > "$tmp/sway.ready"
} &
read -r SWAYSOCK WAYLAND_DISPLAY < "$tmp/sway.ready"
export SWAYSOCK WAYLAND_DISPLAY
printf '#!/usr/bin/env bash\nexec %q > %q 2>&1\n' "$flurry" "$tmp/flurry.log" > "$tmp/bin/start-flurry"
chmod +x "$tmp/bin/start-flurry"
swaymsg "exec $tmp/bin/start-flurry" >/dev/null
# flurry takes the notification name once its first frame is out and its services start
gdbus wait --timeout 30 --session org.freedesktop.Notifications

# shellcheck source=states.sh
. "$here/states.sh"
echo "captured into $out"
