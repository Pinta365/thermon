#!/usr/bin/env bash
set -euo pipefail

if ((EUID == 0)); then
  printf 'refusing to run as root: systemctl --user requires the target user session\n' >&2
  exit 1
fi

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
unit_source="$repo_root/contrib/systemd/thermond.service"
unit_destination="$HOME/.config/systemd/user/thermond.service"
bin_destination="$HOME/.local/bin"
desktop_source="$repo_root/contrib/thermon.desktop"
desktop_destination="$HOME/.local/share/applications/thermon.desktop"

update_desktop_database() {
  if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database "$HOME/.local/share/applications"
  fi
}

desktop_exec_path() {
  local path=$1
  if [[ $path =~ [[:space:]\&\|\;\<\>\*\?\#\~\(\)] ]]; then
    printf '"%s"' "$path"
  else
    printf '%s' "$path"
  fi
}

rewrite_desktop_entry() {
  local source=$1
  local destination=$2
  local executable=$3
  local quoted_executable
  quoted_executable=$(desktop_exec_path "$executable")

  awk -v executable="$quoted_executable" '
    $0 == "Exec=thermon-gui" {
      print "Exec=" executable
      next
    }
    $0 == "Exec=thermon-gui --tab processes" {
      print "Exec=" executable " --tab processes"
      next
    }
    { print }
  ' "$source" >"$destination"
}

case "${1:-}" in
  "")
    ;;
  --uninstall)
    systemctl --user disable --now thermond.service 2>/dev/null || true
    rm -f \
      "$unit_destination" \
      "$bin_destination/thermond" \
      "$bin_destination/thermon" \
      "$bin_destination/thermon-gui" \
      "$desktop_destination"
    systemctl --user daemon-reload
    update_desktop_database
    exit 0
    ;;
  *)
    printf 'usage: %s [--uninstall]\n' "$0" >&2
    exit 2
    ;;
esac

if ! command -v cargo >/dev/null 2>&1; then
  export PATH="$HOME/.cargo/bin:$PATH"
fi
if ! command -v cargo >/dev/null 2>&1; then
  printf 'cargo was not found; install it or add it to PATH\n' >&2
  exit 1
fi

cd "$repo_root"
cargo build --release -p thermond -p thermon -p thermon-gui
install -Dm755 target/release/thermond "$bin_destination/thermond"
install -Dm755 target/release/thermon "$bin_destination/thermon"
install -Dm755 target/release/thermon-gui "$bin_destination/thermon-gui"
install -Dm644 "$unit_source" "$unit_destination"
sed -i 's|^ExecStart=/usr/bin/thermond$|ExecStart=%h/.local/bin/thermond|' "$unit_destination"
mkdir -p "$(dirname "$desktop_destination")"
rewrite_desktop_entry "$desktop_source" "$desktop_destination" "$bin_destination/thermon-gui"
update_desktop_database

systemctl --user daemon-reload
systemctl --user enable --now thermond.service
systemctl --user restart thermond.service
