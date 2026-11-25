# syntax=docker/dockerfile:1.29.0@sha256:572cebbc2b15e6f84c077742952674889cb63cd5e9b80abdffcbe587b6f19183

# All images are pinned by multi-arch index digest; Renovate keeps them up
# to date.

# --- Rust: the server (static binary) and the WebAssembly core ------------
FROM docker.io/library/rust:1.100.0-alpine3.24@sha256:8d0e82abfdf9a38071e07cabc5afba4ddbe077631a80e4e742a760c38beadc33 AS rust

RUN apk add --no-cache binaryen \
 && rustup target add wasm32-unknown-unknown
WORKDIR /src
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates ./crates

# wasm-bindgen-cli must be exactly the version of the wasm-bindgen crate.
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    version="$(sed -n '/^name = "wasm-bindgen"$/{n;s/^version = "\(.*\)"$/\1/p;}' Cargo.lock)" \
 && cargo install --locked --root /opt/wasm-bindgen wasm-bindgen-cli --version "$version"

# An explicit --target keeps RUSTFLAGS away from proc-macros, which must
# stay dynamic.
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/src/target,sharing=locked \
    target="$(rustc -vV | sed -n 's/^host: //p')" \
 && RUSTFLAGS="-C target-feature=+crt-static" cargo build --release --locked --target "$target" -p texel-server \
 && cp "target/$target/release/texel" /texel \
 && cargo build --profile wasm --locked --target wasm32-unknown-unknown -p texel-web -p texel-web-ps2 \
 && for m in texel_web texel_web_ps2; do \
      /opt/wasm-bindgen/bin/wasm-bindgen --target web --out-dir /wasm "target/wasm32-unknown-unknown/wasm/$m.wasm" \
      && wasm-opt -O3 --enable-bulk-memory --enable-nontrapping-float-to-int --enable-sign-ext --enable-mutable-globals \
           -o "/wasm/${m}_bg.wasm" "/wasm/${m}_bg.wasm"; \
    done

# --- The web app -----------------------------------------------------------
FROM docker.io/library/node:26.13.1-alpine3.24@sha256:6bd9ab43c0ec0190ca7b2d193906cb8b7edf6e7f4afa43ddeccd6452710bffb5 AS web

WORKDIR /src/frontend
COPY frontend/package.json frontend/pnpm-lock.yaml ./
RUN --mount=type=cache,target=/root/.cache/pnpm,sharing=locked \
    npm install --global --no-fund --no-audit "pnpm@$(sed -n 's/.*"packageManager": "pnpm@\(.*\)".*/\1/p' package.json)" \
 && pnpm install --frozen-lockfile
COPY frontend ./
COPY --from=rust /wasm ./src/wasm
RUN pnpm run build

# --- Runtime: a static binary, CA certificates, a non-root user -------------
FROM gcr.io/distroless/static-debian13:nonroot@sha256:aafe993665bf6200db38a02c11a9351e782a84b6a45e7f33b6695638c88e3931

LABEL org.opencontainers.image.title="Texel" \
      org.opencontainers.image.description="PlayStation and PlayStation 2 emulators for the browser, with a self-hosted game library" \
      org.opencontainers.image.licenses="MIT"

COPY --from=rust --chmod=0755 /texel /usr/local/bin/texel
COPY --from=web /src/frontend/dist /usr/share/texel/web

USER 65532:65532
ENV TEXEL_STATE=/state \
    TEXEL_WEB=/usr/share/texel/web
VOLUME ["/state"]
EXPOSE 8080
HEALTHCHECK --interval=30s --timeout=5s --start-period=20s \
    CMD ["/usr/local/bin/texel", "--healthcheck"]
ENTRYPOINT ["/usr/local/bin/texel"]
