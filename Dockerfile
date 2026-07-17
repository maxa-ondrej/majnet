# syntax=docker/dockerfile:1
# Control-plane image (ADR 0005, amended): the long-running bot + reconciler
# binaries plus the tools they shell out to (age, openssl, sops). Built in CI
# and pinned by digest so the node *pulls* instead of compiling on a 2 GB box.
#
# One image, two containers: the bot and reconciler run from the same image
# with different commands and — critically — different secret mounts, so the
# credential isolation invariant is preserved by what each container can read,
# not by separate images. `setup` is NOT here: it drives systemctl/wireguard on
# the host, so it stays a native binary installed by the bootstrap.

FROM rust:1-bookworm AS builder
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates crates
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release -p majnet-bot -p majnet-reconciler -p majnet-setup \
    && mkdir -p /out \
    && cp target/release/majnet-bot target/release/majnet-reconciler target/release/majnet-setup /out/

FROM debian:bookworm-slim
ARG SOPS_VERSION=3.11.0
# lego: ACME DNS-01 client for the per-project VPN ingress wildcard certs (ADR 0013).
ARG LEGO_VERSION=4.21.0
# curl stays in the image: it fetches the tools below AND is the bot's compose
# healthcheck command (the reconciler waits on it — see deploy/compose.yaml).
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates age openssl curl \
    && curl -fsSL -o /usr/local/bin/sops \
       "https://github.com/getsops/sops/releases/download/v${SOPS_VERSION}/sops-v${SOPS_VERSION}.linux.amd64" \
    && chmod +x /usr/local/bin/sops \
    && curl -fsSL "https://github.com/go-acme/lego/releases/download/v${LEGO_VERSION}/lego_v${LEGO_VERSION}_linux_amd64.tar.gz" \
       | tar -xz -C /usr/local/bin lego \
    && chmod +x /usr/local/bin/lego \
    && rm -rf /var/lib/apt/lists/*
# bot + reconciler run as containers; setup rides along so majnet-update can
# extract it to the host (it drives systemctl/wireguard, so it stays native).
COPY --from=builder /out/majnet-bot /out/majnet-reconciler /out/majnet-setup /usr/local/bin/
# Build metadata (CI-injected) so the bot can report what's running at /info —
# the control plane's own version signal, mirroring apps (design §16). bot and
# reconciler share this image, so one commit describes both.
ARG VERSION=dev
ARG GIT_COMMIT=unknown
ARG BUILD_TIME=unknown
ENV MAJNET_BUILD_VERSION=$VERSION \
    MAJNET_BUILD_COMMIT=$GIT_COMMIT \
    MAJNET_BUILD_TIME=$BUILD_TIME
# Overridden per service in compose; harmless default.
CMD ["majnet-bot"]
