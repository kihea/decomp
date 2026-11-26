# The node agent as a container image, for machines rented by the hour.
#
# Two images are built from this file:
#
#   docker build -f deploy/node/Dockerfile -t decomp-node:cpu .
#   docker build -f deploy/node/Dockerfile -t decomp-node:cuda \
#     --build-arg BUILD_IMAGE=nvidia/cuda:12.4.1-devel-ubuntu22.04 \
#     --build-arg RUN_IMAGE=nvidia/cuda:12.4.1-runtime-ubuntu22.04 \
#     --build-arg FEATURES=cuda .
#
# The CUDA image runs models on NVIDIA GPUs of compute capability 8.0 and up
# (Ampere and later), on a driver that supports CUDA 12.4 or later. See
# deploy/node/README.md for running it.

ARG BUILD_IMAGE=ubuntu:22.04
ARG RUN_IMAGE=ubuntu:22.04

FROM ${BUILD_IMAGE} AS build
ARG FEATURES=""
# The GPU generation the CUDA kernels are compiled for. Newer GPUs run them
# too, so this is the oldest one the image supports.
ARG CUDA_COMPUTE_CAP=80
ENV CUDA_COMPUTE_CAP=${CUDA_COMPUTE_CAP}
RUN apt-get update \
 && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
      build-essential ca-certificates curl pkg-config \
 && rm -rf /var/lib/apt/lists/*
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
      | sh -s -- -y --profile minimal --default-toolchain stable
ENV PATH=/root/.cargo/bin:${PATH}
WORKDIR /src
COPY . .
RUN cargo build --release --locked -p decomp-node --features "${FEATURES}" \
 && cp target/release/decomp-node /usr/local/bin/decomp-node

FROM ${RUN_IMAGE}
RUN apt-get update \
 && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/*
COPY --from=build /usr/local/bin/decomp-node /usr/local/bin/decomp-node
COPY --chmod=0755 deploy/node/entrypoint.sh /usr/local/bin/decomp-node-entrypoint
# Where the node keeps what it holds for the network: stored files and models.
ENV DECOMP_NODE_DIR=/var/lib/decomp
VOLUME /var/lib/decomp
# The port it holds and relays data and runs model stages on.
EXPOSE 9000
ENTRYPOINT ["/usr/local/bin/decomp-node-entrypoint"]
