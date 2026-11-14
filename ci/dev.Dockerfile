# What the checks and the end-to-end tests need, so a machine with Docker or
# Podman needs nothing else. The checkout is mounted at /src; cargo's
# downloads (/cache/cargo) and the target directory (/src/target) are volumes.
#
#   ci/run.sh shell
FROM rust:1-trixie
ARG VALE_VERSION=3.25.1
ENV DEBIAN_FRONTEND=noninteractive LANG=C.UTF-8
RUN apt-get update && apt-get install -y --no-install-recommends \
      ca-certificates curl docker-cli docker-compose git jq musl-tools nodejs npm openssh-client openssh-server \
      procps python3 python3-venv tini xz-utils \
    && rm -rf /var/lib/apt/lists/* \
    && rustup component add clippy rustfmt \
    && rustup target add x86_64-unknown-linux-musl wasm32-unknown-unknown
RUN curl -fsSL "https://github.com/errata-ai/vale/releases/download/v${VALE_VERSION}/vale_${VALE_VERSION}_Linux_64-bit.tar.gz" \
      | tar -xz -C /usr/local/bin vale
COPY provision/requirements.txt /tmp/requirements.txt
RUN python3 -m venv /opt/venv && /opt/venv/bin/pip install --no-cache-dir -r /tmp/requirements.txt \
    && rm /tmp/requirements.txt
# The container runs as the user who owns the checkout, whose id is only known
# then: the cache directories are open to whoever that is, and HOME is scratch.
RUN mkdir -p /cache/cargo /src/target /home/dev \
    && chmod 1777 /cache/cargo /src/target /home/dev
ENV PATH=/opt/venv/bin:$PATH CARGO_HOME=/cache/cargo HOME=/home/dev CARGO_TERM_COLOR=always
WORKDIR /src
ENTRYPOINT ["tini", "--"]
CMD ["bash"]
