#!/bin/sh
# User data for an EC2 instance that runs the node agent from boot. Fill in
# the four values below and give the script as the instance's user data.
#
# The instance needs:
#   - an instance profile allowed ssm:GetParameter on SECRET_PARAMETER (and
#     kms:Decrypt on its key), where the device secret from
#     `decomp-node add-device` is kept as a SecureString, one per instance;
#   - a security group open to TCP port 9000 from anywhere, so that other
#     nodes can reach it;
#   - for a GPU instance, an AMI with the NVIDIA driver and container
#     toolkit, such as AWS's Deep Learning Base OSS Nvidia Driver GPU AMI.
#
# On instance types with local NVMe storage (storage optimized), the first
# local disk is formatted and holds what the node stores. Its contents are
# lost when the instance stops, which the network repairs by copying.
set -eu

CONTROL_URL="https://control.example"         # the control plane
SECRET_PARAMETER="/decomp/devices/node-1"      # SSM parameter with the device secret
HOLD=""                                        # parts of stored models, e.g. "qwen3-0.6b:ends"
IMAGE="ghcr.io/kihea/decomp-node"

DATA=/var/lib/decomp
PORT=9000
METADATA=http://169.254.169.254/latest

if ! command -v docker >/dev/null 2>&1; then
  if command -v dnf >/dev/null 2>&1; then
    dnf install -y docker
  else
    apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y docker.io
  fi
fi
systemctl enable --now docker

# The first local NVMe disk, if the instance type has one and it is unused.
mkdir -p "$DATA"
for disk in /dev/nvme*n1; do
  [ -b "$disk" ] || continue
  model="$(cat "/sys/block/$(basename "$disk")/device/model" 2>/dev/null || true)"
  case "$model" in
    *"Instance Storage"*)
      if ! blkid "$disk" >/dev/null 2>&1; then
        mkfs.ext4 -q "$disk"
      fi
      mount "$disk" "$DATA"
      break
      ;;
  esac
done

token="$(curl -sS -X PUT "$METADATA/api/token" -H "X-aws-ec2-metadata-token-ttl-seconds: 300")"
region="$(curl -sS -H "X-aws-ec2-metadata-token: $token" "$METADATA/meta-data/placement/region")"
address="$(curl -sS -H "X-aws-ec2-metadata-token: $token" "$METADATA/meta-data/public-ipv4")"
secret="$(aws ssm get-parameter --region "$region" --name "$SECRET_PARAMETER" \
  --with-decryption --query Parameter.Value --output text)"

if command -v nvidia-smi >/dev/null 2>&1 && nvidia-smi >/dev/null 2>&1; then
  tag=cuda
  gpus="--gpus all"
else
  tag=cpu
  gpus=""
fi

docker rm -f decomp-node >/dev/null 2>&1 || true
# shellcheck disable=SC2086 # $gpus is empty or two words
docker run -d --name decomp-node --restart unless-stopped $gpus \
  -p "$PORT:$PORT" -v "$DATA:/var/lib/decomp" \
  -e DECOMP_DEVICE_SECRET="$secret" \
  -e DECOMP_CONTROL_URL="$CONTROL_URL" \
  -e DECOMP_PUBLIC_URL="http://$address:$PORT" \
  -e DECOMP_HOLD="$HOLD" \
  "$IMAGE:$tag"
