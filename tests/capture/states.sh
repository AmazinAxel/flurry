# Sourced by run.sh: drives flurry through every window/state and grabs a screenshot of each.
req() { "$flurry" "$@"; sleep 0.3; }
toggle() { "$flurry" toggle "$1"; sleep 0.3; }
key() { wtype -s 150 "$@"; sleep 0.2; }
shot() { sleep "${2:-0.4}"; date '+%F %H:%M' > "$out/$1.time"; grim -o HEADLESS-1 "$out/$1.png"; echo "  $1"; }

shot focusMenu

# Notifications are hidden while a focus task is set, so capture them first
swaymsg "exec notify-send -a Mail 'New message' 'Lorem ipsum dolor sit amet, consectetur adipiscing elit, sed do eiusmod tempor.'" >/dev/null
swaymsg "exec notify-send -a firefox --action=open=Activate --action=no=Dismiss 'Download finished' 'file.zip'" >/dev/null
shot notifications 0.8
req hideNotif; shot notifications-one
req hideNotif

key 'Write docs'; shot focusMenu-typed
key -k Return; shot focusTask

req toggleStatus; shot status
req toggleStreamingMode; shot status-streaming
req toggleStreamingMode
req closeMenu

swaymsg workspace 2 >/dev/null; shot workspaces 0.15
sleep 1

toggle launcher; key 'fo'; shot launcher
key -M ctrl j -m ctrl; shot launcher-down
key -k Escape

toggle clipboard; shot clipboard
key -k Down; shot clipboard-down
key 'hello'; shot clipboard-search
key -k Escape

toggle pass; key 'ex'; shot pass
key -k Escape

toggle passSave; key 'example.org'; key -k Return; key 'me'; shot passSave
key -k Escape

req record; shot recordMenu
key q; key r; shot recordMenu-toggled
key -k Escape

toggle powermenu; shot powermenu
key -k Escape
toggle osd; shot osd 0.2
toggle osd

req toggleCalendar; shot calendar
key -k Right; shot calendar-next
key -M ctrl -k Left -m ctrl; shot calendar-prev-year
req toggleBluetooth; shot bluetooth
req toggleWifi; shot wifi
key j; shot wifi-down
req closeMenu

req lock; shot lockscreen 1
key 'x'; key -k Return; shot lockscreen-failed 1
