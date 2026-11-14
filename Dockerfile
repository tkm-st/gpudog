# syntax=docker/dockerfile:1

# The NVIDIA container runtime injects CUDA/NVML at startup; neither a CUDA
# toolkit nor an NVIDIA driver is bundled in the production images.
# Build: docker build -t gpudogd .
# Run: docker run -d --name gpudogd --pid=host --userns=host --gpus all \
#        -v /run/gpudog:/run/gpudog -v /etc/gpudog:/etc/gpudog:ro gpudogd
# Logs go to stderr; use docker logs gpudogd.
# The config mount is optional. --userns=host opts out of user namespace
# remapping, which would break client identity.

FROM rust:1-bookworm AS builder
WORKDIR /src
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates crates
RUN cargo build --release --locked --workspace

# Optional test-only target. BuildKit skips it for production builds; the
# standalone mock ABI is never copied into the client or daemon image.
FROM builder AS test-drivers
RUN mkdir -p /test-drivers \
    && rustc --edition=2021 --crate-type=cdylib -O \
        crates/gpudog/tests/support/mock_driver.rs -o /test-drivers/libmock-driver.so \
    && ln -s libmock-driver.so /test-drivers/libcuda.so \
    && ln -s libmock-driver.so /test-drivers/libcuda.so.1 \
    && ln -s libmock-driver.so /test-drivers/libnvidia-ml.so.1

# This base matches the builder. It is not a glibc 2.28 compatibility build.
FROM debian:bookworm-slim AS runtime-base
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates \
    && rm -rf /var/lib/apt/lists/*
ENV NVIDIA_VISIBLE_DEVICES=all \
    NVIDIA_DRIVER_CAPABILITIES=compute,utility

# Build with --target client. Pass the invoking host UID/GID via --user;
# the non-root default is for help only and is not a host-user identity map.
FROM runtime-base AS client
COPY --from=builder /src/target/release/gpudog /usr/bin/gpudog
USER 65532:65532
ENTRYPOINT ["/usr/bin/gpudog"]
CMD ["--help"]

# The last stage remains the default daemon image.
FROM runtime-base AS runtime
COPY --from=builder /src/target/release/gpudogd /usr/bin/gpudogd
RUN mkdir -p /run/gpudog /etc/gpudog && chmod 0755 /run/gpudog /etc/gpudog

# No anonymous volumes: the socket must be shared explicitly with clients.
# The bind-mounted socket directory must be owned by root and not writable
# by group or others. The observer needs host PID visibility (--pid=host)
# for procfs identities; client containers keep private PID namespaces.
# No host network, --privileged, or process signaling is needed.
# compute exposes libcuda.so.1 for context-free CUDA visibility enumeration;
# utility exposes NVML for names, indices and MIG checks.
USER root
ENTRYPOINT ["/usr/bin/gpudogd"]
CMD ["--socket", "/run/gpudog/gpudogd.sock"]
