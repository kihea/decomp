# Running nodes on rented machines

The node agent ships as a container image, so a machine rented by the hour
can run it as a node of the account that rents it, as any device of that
account would. CI publishes two images:

| Image | Runs on | Models run on |
| --- | --- | --- |
| `ghcr.io/kihea/decomp-node:cpu` | Any x86-64 Linux machine | The CPU |
| `ghcr.io/kihea/decomp-node:cuda` | Linux with an NVIDIA GPU of compute capability 8.0 or later (Ampere and newer: A100, A10G, L4, L40S, H100, RTX 30 and 40 series) and a driver supporting CUDA 12.4 or later | The GPU |

Each is also tagged with the commit it was built from, such as
`cuda-<commit>`.

A node runs models on one GPU. On a machine with several, run one node for
each, each shown only its own GPU (`CUDA_VISIBLE_DEVICES`) and given its own
device secret.

## Device keys

A rented machine is a device of the account, with keys of its own. Make them
on a device already in the account:

```sh
decomp-node add-device
```

It approves the new device and prints its code and a line
`DECOMP_DEVICE_SECRET=…`. Give that value to the machine; it stands in for
the key files a device keeps on its own disk. One secret runs one machine.

Whoever holds the secret can work as that device, and the company renting
out the machine can read it. When the machine is given back, revoke the
device:

```sh
decomp-node revoke-device <code>
```

Work preferences (slots, classes, tiers, rate floor) are set for the new
device from any device of the account, by its code:

```sh
decomp-node preferences --device <code> --slots 8 --classes inference,data
```

## What the container reads

| Variable | Meaning |
| --- | --- |
| `DECOMP_DEVICE_SECRET` | The device's keys, from `add-device`. Required |
| `DECOMP_CONTROL_URL` | The control plane. Required |
| `DECOMP_HOLD` | Parts of stored models to hold, separated by spaces: `NAME`, `NAME:ends`, or `NAME:stage-N` |
| `DECOMP_PORT` | The port it holds and relays data and serves model stages on; 9000 by default |
| `DECOMP_PUBLIC_URL` | How other nodes reach that port. On Vast.ai it is found by itself. Without it the node runs jobs but holds, relays, and serves nothing for others |

What the node holds is kept in `/var/lib/decomp`. Arguments given to the
container replace `start`, so the same image runs any other command:

```sh
docker run --rm -e DECOMP_DEVICE_SECRET -e DECOMP_CONTROL_URL ghcr.io/kihea/decomp-node:cpu account
```

## Vast.ai

`vast-template.sh` creates a private template that runs the CUDA image as it
is (Vast's "docker ENTRYPOINT" launch mode), on single-GPU machines with
compute capability 8.0 or later and CUDA 12.4 or later, with port 9000
mapped:

```sh
VAST_API_KEY=... deploy/node/vast-template.sh
```

It prints the template, whose `hash_id` instances are started from. Set
`DECOMP_CONTROL_URL` once in the account's environment variables at
cloud.vast.ai/account, which Vast gives every instance. The device secret
differs for each instance, so give it when the instance is created:

```sh
vastai search offers 'compute_cap>=800 cuda_max_good>=12.4 num_gpus=1' --order=dph_total
vastai create instance <offer> --template_hash <hash_id> --env "-e DECOMP_DEVICE_SECRET=<secret>"
```

Vast maps port 9000 to a port of its own on the machine's public address;
the container reads both from Vast's variables (`PUBLIC_IPADDR`,
`VAST_TCP_PORT_9000`) and tells the network.

## EC2

`ec2-user-data.sh` is user data that installs Docker if the AMI lacks it,
reads the device secret from an SSM SecureString parameter, mounts local
NVMe storage when the instance type has it, and starts the image for the
machine (`cuda` when the NVIDIA driver is present, `cpu` otherwise). Fill in
its four values and launch with it:

- **General purpose and compute or memory optimized** (M, C, R): CPU nodes.
  M6a, C6a, and R6a offer AMD SEV-SNP, which attested nodes need.
- **Storage optimized** (I): storage nodes, holding stored files and models
  on local NVMe.
- **Accelerated computing** (G5, G6, G6e, P4, P5): GPU nodes. Use an AMI
  with the NVIDIA driver and container toolkit, such as AWS's Deep Learning
  Base OSS Nvidia Driver GPU AMI. G4dn's T4 is older than the CUDA image
  supports.

The instance profile needs `ssm:GetParameter` on the parameter, and the
security group must allow TCP port 9000 in. The public address is read once
at first boot, so give the instance an Elastic IP if it will be stopped and
started.

## Checks between nodes of one account

The job board never gives one job to two nodes of the same account, so a
fleet under one account gets its results confirmed only by other accounts'
nodes. Until there are enough of those, its results are
accepted unconfirmed and audited by spot checks, as the receipt of each
says.
