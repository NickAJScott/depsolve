# Side-by-side image: CINC Server's own dependency solver and our Rust engine.
#
# Build from the repository root:
#   docker build -f docker/cinc-baseline/Dockerfile -t depsolve-cinc-baseline .
#
# Both read one JSON problem per stdin line and write one JSON outcome per line:
#   docker run --rm -i depsolve-cinc-baseline solve-cinc < problems.jsonl
#   docker run --rm -i depsolve-cinc-baseline solve-rust < problems.jsonl
#
# CINC Server is published for amd64 only, so the whole image is linux/amd64. On an
# arm64 host both solvers run under the same emulation, which keeps timings comparable.

FROM --platform=linux/amd64 rust:1-slim-bookworm AS rust-build
WORKDIR /build
COPY rust/Cargo.toml rust/Cargo.lock ./
COPY rust/src ./src
RUN cargo build --release --locked

FROM --platform=linux/amd64 ubuntu:24.04

ARG CINC_SERVER_VERSION=15.10.127
ARG CINC_SERVER_SHA256=7d0cf7854c7b176700b647d2c0478db718f8889ffa2e2785eb52b6e6b3eeb621

RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl \
 && rm -rf /var/lib/apt/lists/*

# The package is unpacked, not installed: nothing is configured or started. We only
# want the embedded Ruby, the dep_selector gem bundle and the Gecode build erchef uses.
RUN curl -fsSL -o /tmp/cinc-server.deb \
      "https://downloads.cinc.sh/files/stable/cinc-server/${CINC_SERVER_VERSION}/ubuntu/24.04/cinc-server-core_${CINC_SERVER_VERSION}-1_amd64.deb" \
 && echo "${CINC_SERVER_SHA256}  /tmp/cinc-server.deb" | sha256sum -c - \
 && dpkg-deb -x /tmp/cinc-server.deb / \
 && rm /tmp/cinc-server.deb

COPY docker/cinc-baseline/cinc_solve.rb /opt/depsolve/cinc_solve.rb
COPY --from=rust-build /build/target/release/depsolve /usr/local/bin/depsolve
# The shared library the patched erchef worker loads (see cinc-patch/).
COPY --from=rust-build /build/target/release/libdepsolve_rust.so /opt/depsolve/libdepsolve_rust.so

RUN printf '#!/bin/sh\nexec /opt/cinc-project/embedded/bin/ruby /opt/depsolve/cinc_solve.rb\n' > /usr/local/bin/solve-cinc \
 && printf '#!/bin/sh\nexec /usr/local/bin/depsolve --jsonl\n' > /usr/local/bin/solve-rust \
 && chmod +x /usr/local/bin/solve-cinc /usr/local/bin/solve-rust
