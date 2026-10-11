# syntax=docker/dockerfile:1.7

# The web UI is built once on the build machine's architecture; its static
# files are architecture independent and are embedded into the binary below.
FROM --platform=$BUILDPLATFORM node:22-bookworm-slim@sha256:c3de60bf2f9dd0ac6370e6117950ff62d6e339527e7472301c9c78a017978392 AS web

WORKDIR /web
COPY web/package.json web/package-lock.json ./
RUN npm ci
COPY web/ ./
RUN npm run build

# The builder always runs on the build machine's own architecture and
# cross-compiles with zig as the C compiler and linker, so a multi-arch image
# never runs rustc under emulation.
FROM --platform=$BUILDPLATFORM rust:1.98-bookworm@sha256:93ce27a88655056a51dbdd8f5f2d7ddc071c7b0070fb288a37b5a285fc83971e AS builder

ARG TARGETARCH
ARG ZIG_VERSION=0.14.1
# From https://ziglang.org/download/index.json; change them with ZIG_VERSION.
ARG ZIG_SHA256_X86_64=24aeeec8af16c381934a6cd7d95c807a8cb2cf7df9fa40d359aa884195c4716c
ARG ZIG_SHA256_AARCH64=f7a654acc967864f7a050ddacfaa778c7504a0eca8d2b678839c21eea47c992b

RUN apt-get update \
    && apt-get install -y --no-install-recommends xz-utils \
    && rm -rf /var/lib/apt/lists/*

RUN set -eux; \
    host="$(uname -m)"; \
    case "$host" in \
        x86_64) sha="$ZIG_SHA256_X86_64" ;; \
        aarch64) sha="$ZIG_SHA256_AARCH64" ;; \
        *) echo "unsupported build host: $host" >&2; exit 1 ;; \
    esac; \
    curl -fsSL -o /tmp/zig.tar.xz \
        "https://ziglang.org/download/${ZIG_VERSION}/zig-${host}-linux-${ZIG_VERSION}.tar.xz"; \
    echo "${sha}  /tmp/zig.tar.xz" | sha256sum -c -; \
    tar -xJf /tmp/zig.tar.xz -C /opt; \
    rm /tmp/zig.tar.xz; \
    ln -s "/opt/zig-${host}-linux-${ZIG_VERSION}/zig" /usr/local/bin/zig

# zig cc as a drop-in cc for one target. Drops the flags it rejects: rustc's
# aarch64 musl cortex-a53 erratum workaround and the cc crate's --target.
RUN cat > /usr/local/bin/zigcc <<'SCRIPT' && chmod +x /usr/local/bin/zigcc
#!/bin/sh
zig_target=$1
shift
for arg in "$@"; do
    shift
    case "$arg" in
        -Wl,--fix-cortex-a53-843419 | --target=*) ;;
        *) set -- "$@" "$arg" ;;
    esac
done
exec zig cc -target "$zig_target" "$@"
SCRIPT

RUN set -eux; \
    case "$TARGETARCH" in \
        amd64) triple=x86_64-unknown-linux-musl; zig_target=x86_64-linux-musl ;; \
        arm64) triple=aarch64-unknown-linux-musl; zig_target=aarch64-linux-musl ;; \
        *) echo "unsupported architecture: $TARGETARCH" >&2; exit 1 ;; \
    esac; \
    printf '#!/bin/sh\nexec zigcc %s "$@"\n' "$zig_target" > "/usr/local/bin/cc-$triple"; \
    chmod +x "/usr/local/bin/cc-$triple"; \
    echo "$triple" > /triple; \
    rustup target add "$triple"

WORKDIR /src
COPY . .
COPY --from=web /web/dist /src/web/dist

# A multi-platform build runs this step once per architecture at the same time
# on the build platform; locking the shared caches keeps the two cargo runs from
# unpacking the same crate into the registry at once.
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,target=/src/target,sharing=locked \
    set -eux; \
    triple="$(cat /triple)"; \
    var="$(echo "$triple" | tr 'a-z-' 'A-Z_')"; \
    export "CARGO_TARGET_${var}_LINKER=/usr/local/bin/cc-$triple"; \
    export "CC_$(echo "$triple" | tr '-' '_')=/usr/local/bin/cc-$triple"; \
    export RUSTFLAGS="-C link-self-contained=no"; \
    cargo build --release --locked --target "$triple" -p zirv-flow; \
    install -D "target/$triple/release/zirv-flow" /out/zirv-flow

# An empty directory owned by the non-root user, so a named volume mounted
# here is writable by it (the compose quickstart keeps the master key here).
RUN install -d /out/secrets

FROM gcr.io/distroless/static-debian12:nonroot@sha256:afa5c872c891853ca7fcf1f12c3edb23f7eeef36189728842dd51042ff57f7ab

COPY --from=builder /out/zirv-flow /usr/local/bin/zirv-flow
COPY --from=builder --chown=65532:65532 /out/secrets /var/lib/zirv-flow/secrets

ENV ZIRV_FLOW_SERVER__BIND=0.0.0.0:8080
EXPOSE 8080
USER nonroot:nonroot

HEALTHCHECK --interval=15s --timeout=5s --start-period=30s --retries=3 \
    CMD ["/usr/local/bin/zirv-flow", "healthcheck"]

ENTRYPOINT ["/usr/local/bin/zirv-flow"]
CMD ["all"]
