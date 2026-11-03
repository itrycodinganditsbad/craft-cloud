# Container image of craft-cloud-server: a Debian trixie build stage and a distroless runtime that holds the binary
# and nothing else (no shell, no package manager). It runs as uid 65532.
#
# The build context is the repository root, because Cargo needs the manifests of all workspace members:
#   docker build -f packaging/docker/Dockerfile -t craft-cloud:local .
#   docker build -f packaging/docker/Dockerfile --build-arg CRAFT_CLOUD_VERSION=0.1.0 \
#       --build-arg CRAFT_CLOUD_REVISION="$(git rev-parse HEAD)" -t craft-cloud:0.1.0 .
#
# Every base image is pinned by the digest of its multi-arch index; the tag in the comment is for humans. Update
# the tag and the digest together (see "Docker" in docs/server.md).

# rust:1.94-trixie (the workspace's rust-version; glibc 2.41 like the runtime image below)
ARG RUST_IMAGE=rust:1.94-trixie@sha256:652612f07bfbbdfa3af34761c1e435094c00dde4a98036132fca28c7bb2b165c
# debian:trixie-slim (only used to create the data directory with its owner and mode)
ARG DEBIAN_IMAGE=debian:trixie-slim@sha256:f6b08b09a05725f9431502d3b5714f5c4576e82c1a84fa1debe0c24a7e0c3ec4
# gcr.io/distroless/cc-debian13:nonroot (glibc, libgcc and CA certificates; no shell, no package manager; uid 65532)
ARG RUNTIME_IMAGE=gcr.io/distroless/cc-debian13:nonroot@sha256:08476c244888bfb425bae1ae4dd7c17f093e972aab8426883d97f3c6f280f7c1

# ---- build ---------------------------------------------------------------------
FROM ${RUST_IMAGE} AS build
# Docker builds often run in a small VM: keep the compiler's parallelism and memory use low.
ENV CARGO_BUILD_JOBS=2 \
    CARGO_INCREMENTAL=0 \
    CARGO_PROFILE_RELEASE_STRIP=symbols
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
# Cargo resolves every workspace member, including the apps and xtask, even when only the server package is built.
# Nothing of them but the server enters the image.
COPY .cargo .cargo
COPY crates crates
COPY apps apps
COPY xtask xtask
RUN cargo build --release --locked -p craft-cloud-server \
    && install -D target/release/craft-cloud-server /out/craft-cloud-server

# ---- files that need a shell to create ---------------------------------------------
FROM ${DEBIAN_IMAGE} AS files
# The data directory belongs to the runtime user from the start, so a fresh named volume inherits the owner and the
# mode 0700.
RUN install -d -m 0700 -o 65532 -g 65532 /rootfs/var/lib/craft-cloud

# ---- runtime -----------------------------------------------------------------------
FROM ${RUNTIME_IMAGE}
ARG CRAFT_CLOUD_VERSION=0.0.0
ARG CRAFT_CLOUD_REVISION=unknown
LABEL org.opencontainers.image.title="craft-cloud-server" \
      org.opencontainers.image.description="Self-hosted sync and storage server for the Craft App Suite" \
      org.opencontainers.image.source="https://github.com/itrycodinganditsbad/craft-cloud" \
      org.opencontainers.image.licenses="MIT OR Apache-2.0" \
      org.opencontainers.image.version="${CRAFT_CLOUD_VERSION}" \
      org.opencontainers.image.revision="${CRAFT_CLOUD_REVISION}"
COPY --from=files --chown=65532:65532 /rootfs/var/lib/craft-cloud /var/lib/craft-cloud
COPY --from=build /out/craft-cloud-server /usr/local/bin/craft-cloud-server
USER 65532:65532
EXPOSE 8080
# The SQLite database and the blob storage (when it is on disk) live here: the only writable place besides /tmp.
VOLUME ["/var/lib/craft-cloud"]
# The image has no shell and no curl: the binary asks its own listener (GET /api/info on 127.0.0.1:8080).
HEALTHCHECK --interval=15s --timeout=5s --start-period=30s --retries=3 \
    CMD ["/usr/local/bin/craft-cloud-server", "healthcheck"]
ENTRYPOINT ["/usr/local/bin/craft-cloud-server"]
# The config file is mounted by Compose; see packaging/docker/compose.yaml.
CMD ["serve", "--config", "/etc/craft-cloud/config.toml"]
