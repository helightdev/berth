# The runner of the end-to-end tests: what the tests and berth call,
# with berth from the target directory. It has no toolchain, so a CI
# job loads it quickly.
#
#   ci/run.sh e2e
FROM debian:trixie-slim
ENV DEBIAN_FRONTEND=noninteractive LANG=C.UTF-8
RUN apt-get update && apt-get install -y --no-install-recommends \
      ca-certificates curl docker-cli docker-compose git jq nodejs npm openssh-client procps python3 python3-venv tini xz-utils \
    && rm -rf /var/lib/apt/lists/*
COPY provision/requirements.txt /tmp/requirements.txt
RUN python3 -m venv /opt/venv && /opt/venv/bin/pip install --no-cache-dir -r /tmp/requirements.txt \
    && rm /tmp/requirements.txt
ENV PATH=/opt/venv/bin:$PATH HOME=/root
WORKDIR /src
ENTRYPOINT ["tini", "--"]
CMD ["bash"]
