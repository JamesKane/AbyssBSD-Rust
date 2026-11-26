#!/bin/sh
# SPDX-License-Identifier: BSD-2-Clause
# Starts Sway on the Sky1 with hardware rendering: the display card is komeda's, and the
# compositor and its clients draw with Mesa's panfrost through panthor's render node. Mesa finds
# the GPU only through the harness libdrm (libdrm-platform/build.sh): stock 15.1 libdrm describes
# only PCI devices (docs/GRAPHICS.md §17.5).
# Usage: run-sway-panthor.sh [card] [render node]
CARD=${1:-/dev/dri/card1}
RENDER=${2:-/dev/dri/renderD128}
mkdir -p /var/run/xdg
chmod 700 /var/run/xdg
export XDG_RUNTIME_DIR=/var/run/xdg
export LD_LIBRARY_PATH=/root/harness/libdrm
export WLR_RENDERER=gles2
export WLR_NO_HARDWARE_CURSORS=1
export SEATD_VTBOUND=0
export LIBSEAT_BACKEND=seatd
export WLR_DRM_DEVICES=$CARD
export WLR_RENDER_DRM_DEVICE=$RENDER
exec seatd-launch sway -d -c /root/sway.conf > /var/log/sway.log 2>&1
