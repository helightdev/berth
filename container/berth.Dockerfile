# berth itself, for running `berth serve` or the terminal UI without installing
# anything: the program with SSH, Python and Ansible beside it, and Docker's
# client for the Docker plugin. The engine is another machine's, or this
# machine's through its socket.
#
# The program is not built here. The context holds a static Linux build of
# it as `berth`, with `requirements.txt` from the provisioning source:
#
#   berth server image               builds it with this machine's engine
#   berth server deploy HOST         builds it on HOST and runs the server from it
#
# CI builds the published image from the same file, with the release's own
# program in the context and BERTH_SOURCE=published. berth carries this file
# and a test checks that the two are the same.
#
# Settings and host profiles live in /config, the store and builds in /data.
FROM debian:trixie-slim
COPY requirements.txt /tmp/requirements.txt
RUN apt-get update && apt-get install -y --no-install-recommends \
      ca-certificates curl docker-buildx docker-cli docker-compose npm openssh-client openssl \
      python3 python3-pip tar tini \
    && pip install --no-cache-dir --break-system-packages -r /tmp/requirements.txt \
    && apt-get purge -y python3-pip && apt-get autoremove -y \
    && rm -rf /var/lib/apt/lists/* /tmp/requirements.txt \
    && useradd --create-home --uid 1000 berth \
    && mkdir -p /config /data && chown berth:berth /config /data
# The context gives it mode 0755, which COPY keeps.
COPY berth /usr/local/bin/berth
# The version the program has to say, and the SHA-256 it has to have. The
# build stops when either is not so, which also stops a program for another
# CPU, as it does not run here.
ARG BERTH_VERSION=
ARG BERTH_SHA256=
RUN said=$(berth --version) && echo "$said" \
    && case "$said" in "berth $BERTH_VERSION" | "berth $BERTH_VERSION "*) ;; *) [ -z "$BERTH_VERSION" ] ;; esac \
    && { [ -z "$BERTH_SHA256" ] || echo "$BERTH_SHA256  /usr/local/bin/berth" | sha256sum -c -; }
# `self-built` for an image built from a berth, `published` for CI's. The
# baseline is the repository a self-built server goes back to for a release.
ARG BERTH_SOURCE=self-built
ARG BERTH_BASELINE=ghcr.io/helightdev/berth
LABEL org.opencontainers.image.version=$BERTH_VERSION \
      dev.berth.source=$BERTH_SOURCE \
      dev.berth.baseline=$BERTH_BASELINE \
      dev.berth.binary-sha256=$BERTH_SHA256
USER berth
ENV XDG_CONFIG_HOME=/config XDG_DATA_HOME=/data TERM=xterm-256color
VOLUME ["/config", "/data"]
EXPOSE 7777
ENTRYPOINT ["tini", "--", "berth"]
CMD ["serve", "--listen", "0.0.0.0:7777", "--expose"]
