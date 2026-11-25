#!/bin/sh
# Starts Sway on the Radeon card with hardware rendering: the compositor and
# its clients draw with Mesa's radeonsi through the card's render node.
# Usage: run-sway-gles.sh [card] [render node]
CARD=${1:-/dev/dri/card1}
RENDER=${2:-/dev/dri/renderD129}
mkdir -p /var/run/xdg
chmod 700 /var/run/xdg
export XDG_RUNTIME_DIR=/var/run/xdg
export WLR_RENDERER=gles2
export WLR_NO_HARDWARE_CURSORS=1
export SEATD_VTBOUND=0
export LIBSEAT_BACKEND=seatd
export WLR_DRM_DEVICES=$CARD
export WLR_RENDER_DRM_DEVICE=$RENDER
export MESA_LOADER_DRIVER_OVERRIDE=radeonsi
exec seatd-launch sway -d -c /root/sway.conf > /var/log/sway.log 2>&1
