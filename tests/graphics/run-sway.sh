#!/bin/sh
# Starts Sway on the card, with a private seat daemon, logging everything.
mkdir -p /var/run/xdg
chmod 700 /var/run/xdg
export XDG_RUNTIME_DIR=/var/run/xdg
export WLR_RENDERER=pixman
export WLR_NO_HARDWARE_CURSORS=1
export SEATD_VTBOUND=0
export LIBSEAT_BACKEND=seatd
export WLR_DRM_DEVICES=/dev/dri/card0
exec seatd-launch sway -d -c /root/sway.conf > /var/log/sway.log 2>&1
