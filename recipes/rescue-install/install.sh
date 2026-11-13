#!/usr/bin/env bash
# Install Ubuntu on a server from its provider's rescue system, for when the
# provider's own installer does not work: two disks as a RAID 1, booting with
# UEFI from either of them, and SSH with your key.
#
#   ./install.sh --disks /dev/nvme0n1,/dev/nvme1n1 --key ~/.ssh/authorized_keys --hostname big
#
# Everything on the disks is lost.
set -euo pipefail

release=resolute
disks=
key=
hostname=
mac=
ipv6=
gateway6=
yes=
usage() {
  sed -n '2,8p' "$0" | sed 's/^# \{0,1\}//'
  cat <<'USAGE'

  --disks A,B        the two disks to install on; one disk gives no RAID
  --key FILE         public keys that may log in as root
  --hostname NAME    the server's name
  --release NAME     the Ubuntu release, by code name (default: resolute, 26.04)
  --mac ADDRESS      the network card to configure (default: the one with the default route)
  --ipv6 ADDRESS/N   a fixed IPv6 address, with --gateway6 ADDRESS
  --yes              do not ask before the disks are wiped
USAGE
}
while (($#)); do
  case $1 in
    --disks) disks=$2; shift 2 ;;
    --key) key=$2; shift 2 ;;
    --hostname) hostname=$2; shift 2 ;;
    --release) release=$2; shift 2 ;;
    --mac) mac=$2; shift 2 ;;
    --ipv6) ipv6=$2; shift 2 ;;
    --gateway6) gateway6=$2; shift 2 ;;
    --yes) yes=1; shift ;;
    -h | --help) usage; exit 0 ;;
    *) echo "Not an option: $1" >&2; usage >&2; exit 2 ;;
  esac
done
fail() { echo "$*" >&2; exit 1; }
[[ $EUID -eq 0 ]] || fail 'Run it as root.'
[[ -n $disks && -n $key && -n $hostname ]] || { usage >&2; exit 2; }
[[ -s $key ]] || fail "No public keys in $key."
[[ $hostname =~ ^[a-z0-9]([a-z0-9-]*[a-z0-9])?$ ]] || fail "Not a host name: $hostname"
[[ -d /sys/firmware/efi ]] || fail 'This system did not boot with UEFI, which the installed one needs.'
[[ -z $ipv6 || -n $gateway6 ]] || fail '--ipv6 needs --gateway6.'
IFS=, read -r -a disk <<<"$disks"
((${#disk[@]} == 1 || ${#disk[@]} == 2)) || fail 'Name one or two disks.'
for d in "${disk[@]}"; do
  [[ -b $d ]] || fail "No disk $d."
  # A disk that holds the running system is not one to install on.
  if lsblk -nro MOUNTPOINT "$d" | grep -qx /; then fail "$d holds the running system."; fi
done
if [[ -z $mac ]]; then
  card=$(ip -o route show default | awk '{for (i = 1; i < NF; i++) if ($i == "dev") {print $(i + 1); exit}}')
  [[ -n $card ]] || fail 'No default route to tell the network card by: name it with --mac.'
  mac=$(cat "/sys/class/net/$card/address")
fi

echo "Ubuntu $release goes onto ${disk[*]} as $hostname, network card $mac."
lsblk -o NAME,SIZE,TYPE,FSTYPE,MOUNTPOINT "${disk[@]}"
if [[ -z $yes ]]; then
  read -r -p "Everything on these disks is lost. Type the host name to go on: " answer
  [[ $answer == "$hostname" ]] || fail 'Nothing was changed.'
fi

export DEBIAN_FRONTEND=noninteractive
need=()
for tool in sgdisk:gdisk mdadm:mdadm mkfs.vfat:dosfstools curl:curl xz:xz-utils partprobe:parted; do
  command -v "${tool%%:*}" >/dev/null || need+=("${tool##*:}")
done
if ((${#need[@]})); then
  apt-get update
  apt-get install -y "${need[@]}"
fi

# ── Disks ───────────────────────────────────────────────────────────────────

target=/mnt/target
umount -R "$target" 2>/dev/null || true
# Arrays the rescue system put together from what was on the disks keep them busy.
for array in /dev/md?*; do
  [[ -b $array ]] && mdadm --stop "$array" 2>/dev/null || true
done
part() { case $1 in *[0-9]) echo "${1}p$2" ;; *) echo "$1$2" ;; esac; }
for d in "${disk[@]}"; do
  for p in "$d"?*; do
    [[ -b $p ]] && { mdadm --zero-superblock "$p" 2>/dev/null || true; wipefs -a "$p" >/dev/null 2>&1 || true; }
  done
  sgdisk --zap-all "$d" >/dev/null
  sgdisk -n 1:1MiB:+512MiB -t 1:ef00 -c 1:ESP -n 2:0:0 -t 2:fd00 -c 2:raid "$d" >/dev/null
done
partprobe "${disk[@]}"
udevadm settle
number=1
for d in "${disk[@]}"; do
  mkfs.vfat -F 32 -n "ESP$number" "$(part "$d" 1)" >/dev/null
  number=$((number + 1))
done
if ((${#disk[@]} == 2)); then
  root=/dev/md0
  mdadm --create "$root" --run --level=1 --raid-devices=2 --metadata=1.2 --homehost="$hostname" \
    "$(part "${disk[0]}" 2)" "$(part "${disk[1]}" 2)"
else
  root=$(part "${disk[0]}" 2)
fi
# GRUB reads the root file system to find its kernel, and older ones cannot read these newer features.
mkfs.ext4 -q -F -L root -O ^orphan_file,^metadata_csum_seed "$root"
mkdir -p "$target"
mount "$root" "$target"

# ── The system ──────────────────────────────────────────────────────────────

# Ubuntu's own image of a server's root file system, checked against its list of digests.
base=https://cloud-images.ubuntu.com/$release/current
image=$release-server-cloudimg-amd64-root.tar.xz
echo "Downloading $base/$image"
curl --fail --location --retry 5 --progress-bar --output "/tmp/$image" "$base/$image"
curl --fail --location --retry 5 --silent --output /tmp/SHA256SUMS "$base/SHA256SUMS"
(cd /tmp && grep " \*\{0,1\}$image\$" SHA256SUMS | sha256sum --check --status) || fail "$image does not match Ubuntu's digest."
tar --extract --xz --numeric-owner --xattrs --xattrs-include='*' --file "/tmp/$image" --directory "$target"
rm -f "/tmp/$image"

mkdir -p "$target/boot/efi"
mount "$(part "${disk[0]}" 1)" "$target/boot/efi"
for fs in dev proc sys run; do mount --rbind "/$fs" "$target/$fs"; mount --make-rslave "$target/$fs"; done
mkdir -p "$target/sys/firmware/efi/efivars"
# The image's resolv.conf points at a resolver that is not running in here.
rm -f "$target/etc/resolv.conf"
cat /etc/resolv.conf > "$target/etc/resolv.conf"

cat > "$target/etc/fstab" <<FSTAB
UUID=$(blkid -s UUID -o value "$root")	/	ext4	discard,errors=remount-ro	0 1
LABEL=ESP1	/boot/efi	vfat	umask=0077,nofail,x-systemd.device-timeout=10s	0 1
FSTAB
echo "$hostname" > "$target/etc/hostname"
printf '127.0.0.1 localhost\n127.0.1.1 %s\n::1 localhost ip6-localhost ip6-loopback\n' "$hostname" > "$target/etc/hosts"
install -d -m 0700 "$target/root/.ssh"
install -m 0600 "$key" "$target/root/.ssh/authorized_keys"
install -d "$target/etc/ssh/sshd_config.d"
printf 'PasswordAuthentication no\nPermitRootLogin prohibit-password\n' > "$target/etc/ssh/sshd_config.d/10-keys-only.conf"

# The card is told by its address, because its name differs between kernels.
{
  cat <<NET
network:
  version: 2
  ethernets:
    uplink:
      match:
        macaddress: "$mac"
      set-name: eth0
      dhcp4: true
NET
  if [[ -n $ipv6 ]]; then
    cat <<NET
      accept-ra: false
      addresses:
        - "$ipv6"
      routes:
        - to: "::/0"
          via: "$gateway6"
          on-link: true
NET
  fi
} > "$target/etc/netplan/01-uplink.yaml"
chmod 0600 "$target/etc/netplan/01-uplink.yaml"

# The initramfs would otherwise wait for every disk it was built with, the
# first disk's boot partition among them, and not start when that disk is gone.
mkdir -p "$target/etc/dracut.conf.d"
echo 'hostonly_cmdline="no"' > "$target/etc/dracut.conf.d/10-start-from-either-disk.conf"
raid=
if ((${#disk[@]} == 2)); then
  raid=" rd.md.uuid=$(mdadm --detail --export "$root" | sed -n 's/^MD_UUID=//p')"
fi

chroot "$target" /usr/bin/env RAID="$raid" /bin/bash -euo pipefail <<'CHROOT'
export DEBIAN_FRONTEND=noninteractive
# Nothing here is a cloud instance: the network and the keys are set above.
apt-get purge -y 'cloud-init*' >/dev/null
rm -rf /etc/cloud /var/lib/cloud /etc/netplan/50-cloud-init.yaml
apt-get update
apt-get install -y --no-install-recommends linux-image-generic mdadm grub-efi-amd64 grub-efi-amd64-signed shim-signed openssh-server
# A kernel for virtual machines has no drivers for what a real server is made of.
apt-get purge -y 'linux-image-*virtual*' 'linux-*-kvm' >/dev/null 2>&1 || true
# The monitor does not start without somebody to tell.
{ echo 'MAILADDR root'; mdadm --detail --scan; } > /etc/mdadm/mdadm.conf
ssh-keygen -A
systemctl enable ssh.service systemd-networkd.service >/dev/null 2>&1 || true
sed -i 's/^GRUB_CMDLINE_LINUX_DEFAULT=.*/GRUB_CMDLINE_LINUX_DEFAULT=""/; s/^GRUB_CMDLINE_LINUX=.*/GRUB_CMDLINE_LINUX="console=tty1 console=ttyS0,115200n8"/' /etc/default/grub
sed -i "s/^GRUB_CMDLINE_LINUX=\"\(.*\)\"/GRUB_CMDLINE_LINUX=\"\1$RAID\"/" /etc/default/grub
rm -f /etc/default/grub.d/*cloudimg*
update-initramfs -u -k all
# The signed GRUB reads its configuration from EFI/ubuntu. It is also put at
# the place every firmware looks, with no boot entry of its own: the
# provider's network boot hands over to the disk, whichever it is.
grub-install --target=x86_64-efi --efi-directory=/boot/efi --bootloader-id=ubuntu --no-nvram
grub-install --target=x86_64-efi --efi-directory=/boot/efi --bootloader-id=ubuntu --removable --no-nvram
update-grub
CHROOT

# The second disk boots the same system when the first is gone.
if ((${#disk[@]} == 2)); then
  mkdir -p /mnt/esp2
  mount "$(part "${disk[1]}" 1)" /mnt/esp2
  cp -a "$target/boot/efi/." /mnt/esp2/
  umount /mnt/esp2
fi

sync
umount -R "$target"
echo
echo "Installed. Leave the rescue system: boot from disk, then ssh root@THE-SERVER."
((${#disk[@]} == 2)) && echo "The RAID finishes copying to the second disk in the background: cat /proc/mdstat"
