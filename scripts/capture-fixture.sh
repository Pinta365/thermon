#!/usr/bin/env bash
# Capture thermon's /sys and /proc inputs while preserving stable-ID symlinks.
set -euo pipefail

usage() {
  printf 'usage: %s <fixture-name>\n' "$0" >&2
}

if ((EUID == 0)); then
  printf 'refusing to capture a fixture as root; run this script as a normal user\n' >&2
  exit 1
fi

if (($# != 1)) || [[ ! $1 =~ ^[A-Za-z0-9][A-Za-z0-9._-]*$ ]] || [[ $1 == . || $1 == .. ]]; then
  usage
  exit 2
fi

name=$1
out="$(cd "$(dirname "$0")/.." && pwd)/fixtures/$name"
rm -rf "$out"
mkdir -p "$out"

copy_file() {
  local src=$1
  [[ -f $src && -r $src ]] || return 0
  mkdir -p "$out$(dirname "$src")"
  # Some sysfs attributes fail to read transiently.
  cat "$src" >"$out$src" 2>/dev/null || rm -f "$out$src"
}

copy_link() {
  local link=$1
  [[ -L $link ]] || return 0
  local target
  target=$(readlink "$link")
  mkdir -p "$out$(dirname "$link")"
  ln -sfn "$target" "$out$link"
  mkdir -p "$out$(readlink -f "$link")"
}

for h in /sys/class/hwmon/hwmon*; do
  real=$(readlink -f "$h")
  copy_link "$h"
  copy_link "$real/device"
  # ACPI thermal zones: trip points show whether the zone is a firmware stub.
  for f in "$(readlink -f "$real/device")"/trip_point_*_{type,temp}; do
    copy_file "$f"
  done
  for f in "$real"/*; do
    case ${f##*/} in
      name | temp*_input | temp*_label | temp*_crit | temp*_max | \
        fan*_input | fan*_label | fan*_max | \
        power*_input | power*_average | power*_label | power*_cap_max)
        copy_file "$f" ;;
    esac
  done
done

for c in /sys/class/drm/card*; do
  [[ ${c##*/} =~ ^card[0-9]+$ ]] || continue
  real=$(readlink -f "$c")
  copy_link "$c"
  copy_link "$real/device"
  dev=$(readlink -f "$real/device")
  copy_link "$dev/driver"
  for f in gpu_busy_percent mem_info_vram_used mem_info_vram_total; do
    copy_file "$dev/$f"
  done
done

for cpu in /sys/devices/system/cpu/cpu[0-9]*; do
  copy_file "$cpu/cpufreq/scaling_cur_freq"
  copy_file "$cpu/topology/physical_package_id"
  copy_file "$cpu/thermal_throttle/core_throttle_count"
  copy_file "$cpu/thermal_throttle/package_throttle_count"
done

copy_file /proc/stat
copy_file /proc/meminfo
for f in /proc/pressure/*; do copy_file "$f"; done

echo "captured into $out ($(find "$out" -type f | wc -l) files)"
