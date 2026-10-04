# Midir container image (published to ghcr.io by .github/workflows; build context: repository root).
# Two stages: a static musl build of the binary (rust:alpine targets <arch>-unknown-linux-musl natively, so each
# platform is built on its own architecture), and a `scratch` image with only the binary, CA certificates, time zone
# data and an owner-writable /app: about 12 MB, 5 MB to download. No shell and no libc: the HEALTHCHECK runs
# `midir --healthcheck`. Runtime files come from the working directory /app: .env values arrive as environment
# variables (compose env_file) and config/midir.toml as a compose config.

FROM rust:1.99.0-alpine AS build
# make: jemalloc (the allocator) is built from source
RUN apk add --no-cache musl-dev make ca-certificates tzdata
WORKDIR /src
COPY Cargo.toml Cargo.lock build.rs ./
COPY src ./src
# the workspace's other member (repository tasks): only its manifest is read, nothing of it is built
COPY xtask ./xtask
# jemalloc fixes its page size at build time: on arm64, 64 KiB covers kernels with 4, 16 or 64 KiB pages (Raspberry Pi
# 5, Asahi, ARM servers); the build machine's 4 KiB would abort at startup on those
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    if [ "$(uname -m)" = "aarch64" ]; then export JEMALLOC_SYS_WITH_LG_PAGE=16; fi \
 && cargo build --release --locked --package midir && cp target/release/midir /midir \
 && mkdir -p /rootfs/app/config /rootfs/app/data /rootfs/tmp

FROM scratch
COPY --from=build /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt
COPY --from=build /usr/share/zoneinfo /usr/share/zoneinfo
COPY --from=build --chown=1000:1000 /rootfs/ /
COPY --from=build /midir /usr/local/bin/midir
COPY LICENSE /app/LICENSE
ENV PATH=/usr/local/bin SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt
WORKDIR /app
# which build this is (midir --version, banner, /health): release (main), dev (develop), local (cargo xtask deploy).
# Last on purpose: commit and date change on every build, and nothing after them needs rebuilding.
ARG MIDIR_BUILD_CHANNEL=local
ARG MIDIR_BUILD_COMMIT=
ARG MIDIR_BUILD_DATE=
ENV MIDIR_BUILD_CHANNEL=$MIDIR_BUILD_CHANNEL MIDIR_BUILD_COMMIT=$MIDIR_BUILD_COMMIT MIDIR_BUILD_DATE=$MIDIR_BUILD_DATE
# no root at runtime; compose overrides the ids with yours (user: "${UID}:${GID}")
USER 1000:1000
EXPOSE 18880
HEALTHCHECK --interval=10s --timeout=3s --start-period=10s --retries=5 \
  CMD ["midir", "--healthcheck", "--port", "18880"]
CMD ["midir", "--host", "0.0.0.0", "--port", "18880"]
LABEL org.opencontainers.image.title="midir" \
      org.opencontainers.image.description="Midir: OpenAI- and Anthropic-compatible gateway for LLM backends (StackSpot AI agents today)" \
      org.opencontainers.image.licenses="MIT" \
      org.opencontainers.image.source="https://github.com/helton/midir" \
      org.opencontainers.image.revision="$MIDIR_BUILD_COMMIT"
