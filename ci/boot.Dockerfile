# The boot tests' container: QEMU with UEFI firmware, the tools that build a
# seed and an installer, a package cache with a resolver that sends the guest
# to it, and what berth needs to apply a profile. berth itself is built in the
# dev container and mounted with the target directory.
#
#   ci/run.sh boot cloud|iso
FROM debian:trixie-slim
ENV DEBIAN_FRONTEND=noninteractive LANG=C.UTF-8
RUN apt-get update && apt-get install -y --no-install-recommends \
      ca-certificates curl dnsmasq-base git nginx nodejs npm openssh-client openssl ovmf procps \
      python3 python3-venv qemu-system-x86 qemu-utils socat tini xorriso \
    && rm -rf /var/lib/apt/lists/* \
    && rm -f /etc/nginx/sites-enabled/default
COPY provision/requirements.txt /tmp/requirements.txt
RUN python3 -m venv /opt/venv && /opt/venv/bin/pip install --no-cache-dir -r /tmp/requirements.txt \
    && rm /tmp/requirements.txt
ENV PATH=/opt/venv/bin:$PATH PYTHONDONTWRITEBYTECODE=1
WORKDIR /src
ENTRYPOINT ["tini", "--"]
CMD ["bash"]
