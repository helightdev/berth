# berth itself, for running `berth serve` or the terminal UI without installing
# anything: the program with SSH, Python and Ansible beside it.
#
#   docker build -f container/berth.Dockerfile -t berth .
#
# BERTH_BUILD_VERSION is the version a release gives it, from its tag; without
# it the image says the version in Cargo.toml.
#
# Settings and host profiles live in /config, the store and builds in /data.
FROM rust:1-bookworm AS build
RUN apt-get update && apt-get install -y --no-install-recommends musl-tools python3 \
    && rm -rf /var/lib/apt/lists/* \
    && rustup target add x86_64-unknown-linux-musl wasm32-unknown-unknown
WORKDIR /src
COPY . .
ARG BERTH_BUILD_VERSION=
# With the page for a browser, which the default command serves.
RUN cargo build --release --locked --target x86_64-unknown-linux-musl -p berth --features web \
    && said=$(target/x86_64-unknown-linux-musl/release/berth --version) && echo "$said" \
    && { [ -z "$BERTH_BUILD_VERSION" ] || [ "$said" = "berth $BERTH_BUILD_VERSION" ]; }

FROM debian:trixie-slim
COPY requirements.txt /tmp/requirements.txt
RUN apt-get update && apt-get install -y --no-install-recommends \
      ca-certificates curl npm openssh-client openssl python3 python3-pip tar tini \
    && pip install --no-cache-dir --break-system-packages -r /tmp/requirements.txt \
    && apt-get purge -y python3-pip && apt-get autoremove -y \
    && rm -rf /var/lib/apt/lists/* /tmp/requirements.txt \
    && useradd --create-home --uid 1000 berth \
    && mkdir -p /config /data && chown berth:berth /config /data
COPY --from=build /src/target/x86_64-unknown-linux-musl/release/berth /usr/local/bin/berth
USER berth
ENV XDG_CONFIG_HOME=/config XDG_DATA_HOME=/data TERM=xterm-256color
VOLUME ["/config", "/data"]
EXPOSE 7777
ENTRYPOINT ["tini", "--", "berth"]
CMD ["serve", "--listen", "0.0.0.0:7777", "--expose", "--web"]
