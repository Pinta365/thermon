# Thermon

Thermon is a low-overhead hardware and system monitor for Linux, made for
Omarchy and Hyprland. `thermond` samples sensors and system state, `thermon`
is its CLI, and `thermon-gui` is a desktop window. The Omarchy bar plugin
lives in [omarchy-thermon](https://github.com/Pinta365/omarchy-thermon).

It reads what the kernel exposes, and says so when it has to infer something
(a possible thermal throttle, whether a GPU is integrated). It does not control
fans or change system settings.

## Install

Building needs Rust 1.95 or newer. From a checkout:

```bash
scripts/install-dev.sh
```

This builds release binaries and installs `thermond`, `thermon` and
`thermon-gui` in `~/.local/bin`, a desktop entry in
`~/.local/share/applications`, and the user service in
`~/.config/systemd/user/thermond.service`, which it enables and starts.
Desktop notifications need `notify-send` (libnotify).

Remove it again with:

```bash
scripts/install-dev.sh --uninstall
```

The service samples once per second by default and listens on
`$XDG_RUNTIME_DIR/thermon.sock`.

## CLI

The commands talk to the user service. `--socket PATH` uses a different
socket.

```bash
thermon status
```

```text
Health  OK
CPU     3.2% over 8 CPUs · 2.62–5.25 GHz
Memory  19.4 GiB / 60.4 GiB used · swap 0.0 GiB / 120.9 GiB
PSI10   cpu some 0.02 full 0.00 · memory some 0.00 full 0.00 · io some 0.04 full 0.02
```

```bash
thermon top --sort cpu --limit 3
```

```text
     PID   CPU%       RSS   NI S  NAME             COMMAND
    4211   38.0      1.2G    0 S  firefox          /usr/lib/firefox/firefox
    9902    6.0      210M    0 S  code             /usr/lib/code/code
    1185    2.0      234M    0 S  Hyprland         Hyprland
(3 of 438 processes)
```

`top` accepts `--sort cpu|mem|pid|name`. As in `top`, CPU% is per core: 100%
is one full core, so a busy multi-threaded process can go above it. Processes
are only scanned while a client asks for them.

```bash
thermon kill 4211 --start-ticks 24745178
```

`kill` sends SIGTERM (`--signal kill` for SIGKILL). With `--start-ticks`, the
start time `top --json` reports, it acts only if the PID still belongs to that
process. PID 1 is refused.

```bash
thermon history cpu.usage --range 15m
```

```text
90 points at 10 s intervals
cpu.usage  ▃▁▁▁▁▁▂▃▇▄▄▂▄▃▂▃▃▂▂▂▂▁▁▁▁▁▃▂▁▂▂▂▁▂█▃▃▃▃▄▄▄▂▄▂▃▃▃▅▄▇▇█▇▂▁▅▂▁▁  last 1.1%  min 0.6  avg 3.6  max 12.2
```

Ranges are `90s`, `15m`, `1h` or `24h`. With no series, `history` shows every
series.

```bash
thermon alerts
```

```text
Firing
  WARN  10:46:18  Processor Tctl is hot — 97 °C; the warning level is 95 °C.

Recent
  WARN  10:46:18  Processor Tctl is hot
```

```bash
thermon watch --processes
```

```json
{"v":1,"type":"inventory","data":{"cpu_count":8,"chips":[...]}}
{"v":1,"type":"snapshot","data":{"cpu":{"usage":3.2,...},...}}
```

`watch` writes the daemon's JSON lines as they come. `--processes` adds
process lists (`--sort`, `--limit`), `--alerts` adds alert events,
`--no-snapshots` leaves out snapshots, and `--interval-ms` spaces snapshots
out.

```bash
thermon dump
```

`dump` reads the machine directly, without `thermond`; `--root PATH` reads a
captured fixture instead (`thermon dump --root fixtures/ryzen-rx9070`).

```bash
thermon config
```

```text
~/.config/thermon/config.toml: not found, using defaults

sensor ids (use as [sensors."<id>"]; `*` wildcards allowed):
  k10temp@0000:00:18.3/tctl                label "Tctl", warn 95
  amdgpu@0000:03:00.0/junction             label "junction", warn 100, crit 110
```

`config` checks the configuration and lists the sensor ids. `--config PATH`
picks the file for `dump` and `config`; `--config none` uses the defaults.

```bash
thermon config hide acpitz/temp1
thermon config unhide acpitz/temp1
```

`config hide` writes `hide = true` for the exact sensor id. `config unhide`
removes an exact hide or adds `hide = false` to override a matching hidden
wildcard. Both preserve the rest of the TOML file and create it if needed.
They require a config file, so `--config none` cannot be used with them.

`--json` works for `status`, `top`, `history`, `alerts`, `dump`, and
`config hide|unhide`.

## Desktop window

`thermon-gui` shows the health banner, a sensor tree and charts. Tick sensors
to chart them over the last 5 minutes, 1 hour or 24 hours. The Processes tab
has a filter, sortable columns (CPU is per core, as in `top`), and End, Kill
and Lower priority, each with a confirmation. `thermon-gui --tab processes` opens straight on that tab.

Colours follow the current Omarchy theme and change with it. Set
`THERMON_THEME=path/colors.toml` to use another palette; elsewhere a neutral
dark theme is used.

Its window class is `thermon`. To float it on Omarchy, add this to
`~/.config/hypr/hyprland.lua`:

```lua
o.window("^thermon$", { float = true })
```

## Configuration

Thermon reads `$XDG_CONFIG_HOME/thermon/config.toml`, or
`~/.config/thermon/config.toml`. `thermond` reloads it when it changes; a file
that doesn't parse is reported and the previous configuration stays in use.

```toml
# Sample interval, 200..=60000; default 1000. Needs a restart.
interval_ms = 1000

[warn]
# Default temperature warnings by sensor category.
# cpu: 5 °C below the chip's critical temperature, or 95 °C if it has none.
# gpu: 10 °C below the sensor's critical temperature, or 90 °C if it has none.
# cpu = 90
# gpu = 95
storage = 65
# board and other have no default. A value <= 0 turns a default off.
board = 60

[sensors."gigabyte_wmi/temp1"]
label = "System"

[sensors."acpitz/temp1"]
hide = true

[sensors."amdgpu@0000:03:00.0/junction"]
warn = 95
crit = 105

# `*` matches any run of characters, including `/`.
[sensors."r8169*"]
hide = true

[alerts]
enabled = true
# Lowest severity that alerts: "info", "warn" or "crit".
min_severity = "warn"
# A problem must last this long to alert, and be gone this long to resolve.
hold_s = 10
# Desktop notifications through notify-send.
notify = true
```

Sensor ids are `<chip id>/<label>`. The chip id is the driver name and PCI
address; disks and memory sensors that share a controller also get their ATA
port or I2C address, so ids stay the same when hardware is added.

`label`, `hide`, `warn` and `crit` are optional. A sensor's `warn` replaces
its category default, and `warn = 0` turns its warning off. `crit` replaces
the driver's threshold. Exact keys win over wildcards; among wildcards, the
one with the most literal characters wins. Entries are not merged.

## Alerts

`thermond` checks every sample: temperatures against their thresholds,
memory, CPU and IO pressure, low memory, thermal throttling, and zombie
processes while processes are being watched (the same checks as the health
line in `thermon status`). A problem
seen in most samples over `hold_s` seconds becomes an alert: it is logged,
sent as a desktop notification, and streamed to clients. A warning that turns
critical alerts again. Once the problem has been gone for `hold_s` seconds,
the alert resolves.

## Data and hardware

| Metric | Source |
| --- | --- |
| CPU use | `/proc/stat` |
| CPU frequency | `/sys/devices/system/cpu/cpuN/cpufreq/scaling_cur_freq` |
| CPU throttling (Intel) | `/sys/devices/system/cpu/cpuN/thermal_throttle/*_throttle_count` |
| Memory and swap | `/proc/meminfo` |
| CPU, memory and IO pressure | `/proc/pressure/{cpu,memory,io}` |
| Temperatures, fans and power | `/sys/class/hwmon` |
| AMD GPU load and VRAM | `/sys/class/drm/cardN/device` |
| Processes | `/proc/<pid>/{stat,status,cmdline}` |

- AMD CPUs have no per-core temperatures; `k10temp` gives Tctl and Tccd.
- Board sensors are often unlabeled (`gigabyte_wmi` for one); label them in
  the config once you know what they are.
- Intel CPUs report throttling themselves. Elsewhere thermon infers a
  possible throttle from temperature and a clock drop, and says so.
- A device in runtime suspend, such as an idle laptop dGPU, isn't read, so
  polling never wakes it.
- Some firmware defines an ACPI thermal zone with a fixed temperature and
  impossible trip points (a "critical" below 30 °C). Thermon marks those as
  placeholders so they're easy to spot and hide.
- NVIDIA GPUs are not supported yet.

On a Ryzen 7 9800X3D desktop at the default interval, `thermond` uses about
0.2% of one core and 3–4 MB of memory. It rises to about 0.7% while a client
watches processes. `thermon-gui` uses about 0.25% of a core on the Sensors
tab and 1% on Processes; most of its ~125 MB is the GPU driver.

## Protocol

`thermond` and its clients exchange one JSON object per line over the Unix
socket. A connection can send any number of one-shot requests; `subscribe`
turns it into a stream that starts with the inventory and repeats it when
hardware changes. The reference is
[`crates/thermon-core/src/protocol.rs`](crates/thermon-core/src/protocol.rs).

```text
→ {"v":1,"cmd":"snapshot"}
← {"v":1,"type":"snapshot","data":{...}}
→ {"v":1,"cmd":"history","series":["cpu.usage"],"range":"15m"}
→ {"v":1,"cmd":"processes","sort":"cpu","limit":20}
→ {"v":1,"cmd":"alerts"}
→ {"v":1,"cmd":"subscribe","topics":["snapshot","processes","alerts"],"interval_ms":2000}
```

## Development

```bash
cargo test
scripts/capture-fixture.sh my-machine
```

Tests run against fixtures: captured subsets of `/sys` and `/proc` under
`fixtures/`. `capture-fixture.sh` writes one for the current machine; review
it before contributing it. `fixtures/intel-laptop-synthetic` is hand-written
from driver layouts and should be replaced by a real capture. Fixtures from
Intel, NVIDIA and other hardware are welcome.

## License

MIT
