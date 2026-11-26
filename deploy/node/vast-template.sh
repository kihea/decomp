#!/bin/sh
# Creates a Vast.ai template that runs the node agent's CUDA image as it is
# (Vast's "docker ENTRYPOINT" launch mode), on machines whose GPUs and driver
# the image supports. The template holds no secret: each instance is given
# its own device secret when it is created, and the control plane's address
# can be set once in the account's environment variables
# (cloud.vast.ai/account), which Vast gives every instance.
#
#   VAST_API_KEY=... deploy/node/vast-template.sh [image]
#
# The image defaults to the one CI publishes. Prints Vast's answer, which
# holds the template's hash_id, the ID instances are started from.
set -eu

: "${VAST_API_KEY:?give a Vast.ai API key in VAST_API_KEY}"
image="${1:-ghcr.io/kihea/decomp-node}"

# The image's CUDA kernels need a GPU of compute capability 8.0 or later
# (Vast counts it times 100) and a driver that supports CUDA 12.4.
MIN_COMPUTE_CAP=800
MIN_CUDA=12.4
# A node runs one GPU, and one instance runs one node.
GPUS=1
# Disk for the models and files the node holds, in GB.
DISK_GB=64

curl --fail-with-body -sS https://console.vast.ai/api/v0/template/ \
  -H "Authorization: Bearer ${VAST_API_KEY}" \
  -H "Content-Type: application/json" \
  -d @- <<EOF
{
  "name": "DeComp GPU node",
  "image": "${image}",
  "tag": "cuda",
  "desc": "Runs the DeComp node agent on the GPU. Give each instance its own DECOMP_DEVICE_SECRET, and DECOMP_CONTROL_URL.",
  "readme": "For each instance, make a device secret on a device of your account with decomp-node add-device, and give it to the instance as DECOMP_DEVICE_SECRET. One secret runs one machine. Set DECOMP_CONTROL_URL in your Vast account's environment variables or on the instance, and optionally DECOMP_HOLD, the parts of stored models to hold. Revoke the device with decomp-node revoke-device when the instance is destroyed.",
  "env": "-p 9000:9000",
  "onstart": "",
  "runtype": "args",
  "args_str": "",
  "use_ssh": false,
  "ssh_direct": false,
  "use_jupyter_lab": false,
  "jup_direct": false,
  "jupyter_dir": "",
  "docker_login_repo": "",
  "docker_login_user": "",
  "docker_login_pass": "",
  "href": "https://github.com/kihea/decomp",
  "repo": "",
  "extra_filters": {
    "compute_cap": { "gte": ${MIN_COMPUTE_CAP} },
    "cuda_max_good": { "gte": ${MIN_CUDA} },
    "num_gpus": { "eq": ${GPUS} }
  },
  "recommended_disk_space": ${DISK_GB},
  "private": true
}
EOF
echo
