# A test host for the end-to-end tests: Ubuntu as a cloud provider hands it
# over, with SSH, sudo and an operator account whose key arrives at start.
# Applying a profile installs the rest, as on a real host.
#
#   ci/run.sh e2e
FROM ubuntu:26.04
ENV DEBIAN_FRONTEND=noninteractive LANG=C.UTF-8
RUN apt-get update && apt-get install -y --no-install-recommends \
      ca-certificates curl fd-find iproute2 openssh-server procps python3 sudo tini \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --system render \
    && useradd --create-home --shell /bin/bash --groups sudo ops \
    && usermod --password '*' ops \
    && printf 'ops ALL=(ALL) NOPASSWD:ALL\n' > /etc/sudoers.d/90-cloud-init-users \
    && chmod 0440 /etc/sudoers.d/90-cloud-init-users \
    && mkdir -p /etc/ssh/provided-keys /var/log/berth-ci /run/sshd \
    && rm -f /etc/ssh/ssh_host_*
# The tailscale client, where the role would install it from Tailscale's
# packages, which the test network cannot reach. host.sh starts tailscaled.
ARG TAILSCALE=1.104.2 TAILSCALE_SHA256=512f613ed7d04fc7282f7dc5bd76aa105aae28168fbb9357eb749ebcbc29f0ae
RUN curl -fsSL -o /tmp/tailscale.tgz "https://pkgs.tailscale.com/stable/tailscale_${TAILSCALE}_amd64.tgz" \
    && echo "${TAILSCALE_SHA256}  /tmp/tailscale.tgz" | sha256sum -c - \
    && tar -xzf /tmp/tailscale.tgz -C /tmp \
    && install -m 0755 "/tmp/tailscale_${TAILSCALE}_amd64/tailscale" /usr/bin/tailscale \
    && install -m 0755 "/tmp/tailscale_${TAILSCALE}_amd64/tailscaled" /usr/sbin/tailscaled \
    && rm -rf /tmp/tailscale*
# The keys a provider puts in place sit beside the ones the role manages.
RUN printf '%s\n' 'AuthorizedKeysFile .ssh/authorized_keys /etc/ssh/provided-keys/%u' \
      'PasswordAuthentication no' 'KbdInteractiveAuthentication no' 'LogLevel VERBOSE' \
      > /etc/ssh/sshd_config.d/10-provided.conf
COPY --chmod=0755 ci/host.sh /usr/local/sbin/berth-ci-host
EXPOSE 22
ENTRYPOINT ["tini", "--", "/usr/local/sbin/berth-ci-host"]
