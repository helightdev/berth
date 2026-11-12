# A test host for the end-to-end tests: Ubuntu as a cloud provider hands it
# over, with SSH, sudo and an operator account whose key arrives at start.
# Applying a profile installs the rest, as on a real host.
#
#   container/ci.sh e2e
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
# The keys a provider puts in place sit beside the ones the role manages.
RUN printf '%s\n' 'AuthorizedKeysFile .ssh/authorized_keys /etc/ssh/provided-keys/%u' \
      'PasswordAuthentication no' 'KbdInteractiveAuthentication no' 'LogLevel VERBOSE' \
      > /etc/ssh/sshd_config.d/10-provided.conf
COPY --chmod=0755 container/ci-host.sh /usr/local/sbin/berth-ci-host
EXPOSE 22
ENTRYPOINT ["tini", "--", "/usr/local/sbin/berth-ci-host"]
