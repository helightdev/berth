# A machine as a cloud provider hands it over: systemd as PID 1, SSH, sudo and
# python3. The operator account and its keys are added when a container is
# created, so one image serves every host.
FROM ubuntu:26.04
ENV DEBIAN_FRONTEND=noninteractive LANG=C.UTF-8 container=docker
RUN apt-get update && apt-get install -y --no-install-recommends \
      ca-certificates curl dbus iproute2 openssh-server procps python3 sudo systemd systemd-sysv \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --system render \
    && mkdir -p /etc/ssh/provided-keys /run/sshd \
    && rm -f /etc/ssh/ssh_host_* \
    && printf '%s\n' 'AuthorizedKeysFile .ssh/authorized_keys /etc/ssh/provided-keys/%u' \
      'PasswordAuthentication no' 'KbdInteractiveAuthentication no' \
      > /etc/ssh/sshd_config.d/10-provided.conf \
    && systemctl mask systemd-udevd.service systemd-udev-trigger.service \
      getty.target console-getty.service \
      systemd-modules-load.service \
    && systemctl enable ssh.service
# systemd stops cleanly on this signal, not on SIGTERM.
STOPSIGNAL SIGRTMIN+3
VOLUME ["/run", "/run/lock"]
CMD ["/sbin/init"]
