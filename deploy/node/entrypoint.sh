#!/bin/sh
# Starts the node agent in its container.
#
#   DECOMP_DEVICE_SECRET  The device's keys, as `decomp-node add-device` showed
#                         them on a device of the account. Required.
#   DECOMP_CONTROL_URL    The control plane. Required.
#   DECOMP_HOLD           Parts of stored models to hold, separated by spaces,
#                         such as "qwen3-0.6b:ends qwen3-0.6b:stage-1".
#   DECOMP_PORT           The port to hold and relay data on. 9000 by default.
#   DECOMP_PUBLIC_URL     How other devices reach that port. On Vast.ai it is
#                         found from the machine's public address and the port
#                         mapped to DECOMP_PORT. Without it the node runs jobs
#                         but holds, relays, and serves nothing for others.
#
# Arguments given to the container replace `start` and its options, so the
# same image runs any other command, such as `account`.
set -eu

: "${DECOMP_DEVICE_SECRET:?give the device secret that decomp-node add-device showed}"
: "${DECOMP_CONTROL_URL:?give the address of the control plane}"

if [ "$#" -gt 0 ]; then
  exec decomp-node "$@"
fi

port="${DECOMP_PORT:-9000}"
public="${DECOMP_PUBLIC_URL:-}"
if [ -z "$public" ] && [ -n "${PUBLIC_IPADDR:-}" ]; then
  # Vast.ai maps each container port to a port of its own on the machine's
  # public address, and names it VAST_TCP_PORT_<container port>.
  mapped="$(printenv "VAST_TCP_PORT_${port}" || true)"
  if [ -n "$mapped" ]; then
    public="http://${PUBLIC_IPADDR}:${mapped}"
  fi
fi

set -- start
for part in ${DECOMP_HOLD:-}; do
  set -- "$@" --hold "$part"
done
if [ -n "$public" ]; then
  set -- "$@" --listen "0.0.0.0:${port}" --public-url "$public"
fi
exec decomp-node "$@"
