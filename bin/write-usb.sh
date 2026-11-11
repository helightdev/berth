#!/usr/bin/env bash
# Write an installer ISO to a USB drive, erasing it. Run as root:
#   sudo bin/write-usb.sh build/HOST-autoinstall.iso /dev/sdX
# Only whole USB disks are accepted, and never one holding a mounted system.
set -euo pipefail
[[ $# == 2 ]] || { echo 'Usage: write-usb.sh ISO DEVICE' >&2; exit 2; }
iso=$1 device=$2
[[ -f $iso ]] || { echo "ISO not found: $iso" >&2; exit 1; }
[[ -b $device ]] || { echo "Not a block device: $device" >&2; exit 1; }
read -r type transport < <(lsblk -dno TYPE,TRAN "$device")
[[ $type == disk && $transport == usb ]] || {
  echo "Refusing to write to $device: not a whole USB disk" >&2; exit 1; }
# An ejected ("safely removed") stick stays listed but has no medium behind it.
size=$(blockdev --getsize64 "$device" 2>/dev/null) && (( size > 0 )) || {
  echo "$device has no medium. It was probably ejected: unplug it and plug it in again." >&2; exit 1; }
(( size >= $(stat -c %s "$iso") )) || {
  echo "$device is smaller than the ISO" >&2; exit 1; }
while read -r part mountpoint; do
  [[ -n $mountpoint ]] || continue
  case $mountpoint in
    /|/boot|/boot/*|/home|/usr|/var|'[SWAP]')
      echo "Refusing to write to $device: $part is in use as $mountpoint" >&2; exit 1 ;;
  esac
  echo "Unmounting $part ($mountpoint)"
  umount "$part"
done < <(lsblk -nro PATH,MOUNTPOINT "$device")
echo "Writing $iso to $device"
dd if="$iso" of="$device" bs=4M oflag=direct conv=fsync status=progress
sync
echo "Verifying $device"
# Read back past the cache: a worn or fake stick can accept a write and return other data.
dd if="$device" bs=4M iflag=direct,count_bytes count="$(stat -c %s "$iso")" status=none | cmp -s - "$iso" || {
  echo "Verification failed: $device does not hold what was written. Use another USB drive." >&2; exit 1; }
echo "Wrote and verified $iso on $device"
