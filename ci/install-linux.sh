#!/bin/sh
# Run from an unpacked Linux release. Local installation, no service or sudo.
set -eu
root=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
prefix=${XDG_DATA_HOME:-"$HOME/.local/share"}
install -d "$HOME/.local/bin" "$prefix/applications" "$prefix/icons/hicolor/256x256/apps"
install -m 755 "$root/congminal" "$root/congminal-ssh-helper" "$HOME/.local/bin/"
install -m 644 "$root/congminal.desktop" "$prefix/applications/congminal.desktop"
install -m 644 "$root/congminal.png" "$prefix/icons/hicolor/256x256/apps/congminal.png"
install -d "$prefix/congminal"
install -m 644 "$root/LICENSE" "$root/NOTICE" "$root/LICENSE-Terminus.txt" "$root/LICENSE-libc.txt" "$prefix/congminal/"
printf '%s\n' 'Installed congminal and its desktop icon. Ensure $HOME/.local/bin is in PATH.'
