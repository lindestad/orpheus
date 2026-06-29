# poll-monitor

A small Windows-focused mouse polling-rate monitor and switcher.

The current app is a Rust CLI/TUI. It reads the mouse's configured polling rate through HID feature/input reports, reads battery telemetry where the vendor protocol exposes it, can set a target rate, and can watch running processes to apply app-specific rate rules.

## Status

This is an early working prototype. It has been tested against:

- G-Wolves Fenrir receiver `33e4:3517`, including rate read/write and battery level/status.
- IPI Piao wireless receiver `372e:1014`, including rate read/write and battery level.

Supported model IDs currently cover known Fenrir, Fenrir Pro, and Fenir Max wired/receiver IDs from G-Wolves WebHID protocol data, plus IPI Piao/Float-style PIX v1 mouse IDs from `https://shan.ipigame.cn/devices`.

## Usage

Launch the TUI:

```powershell
cargo run
```

List supported devices, current configured polling rates, and battery telemetry:

```powershell
cargo run -- list
```

Set the first supported device to a rate:

```powershell
cargo run -- set 1000
cargo run -- set 8k
```

When multiple supported devices are connected, use the TUI to choose a specific device before applying a rate.

Create a local config file:

```powershell
cargo run -- init-config
```

Watch running processes and apply configured app rules:

```powershell
cargo run -- watch --config poll-monitor.toml
```

Validate the watcher without writing to the mouse:

```powershell
cargo run -- watch --config poll-monitor.toml --dry-run --once
```

## TUI Controls

- `q` / `Esc`: quit
- `r`: refresh devices
- `up` / `down` or `k` / `j`: select device
- `left` / `right` or `h` / `l`: select target polling rate
- `Enter`: apply selected rate
- `Space`: sync target to current rate

## Config

`poll-monitor.toml` is ignored by git so local app rules do not get committed. Use `poll-monitor.example.toml` as the tracked template.

```toml
default_rate = 8000
restore_rate = 8000
scan_interval_ms = 1000

[[rules]]
exe = "problem-game.exe"
rate = 1000
restore = 8000
```

For a two-mouse setup where the idle mouse is charging and the active mouse should be boosted while a target process runs:

```toml
default_rate = 1000
restore_rate = 1000
scan_interval_ms = 1000
power_policy = "active-non-charging"
battery_trend_window_ms = 600000
battery_trend_min_delta = 1
assume_wired_is_charging = true
assume_wireless_is_discharging = false
allow_unknown_power_active = true

[[rules]]
exe = "problem-game.exe"
rate = 4000
restore = 1000
```

Rules are checked in file order. The first matching process wins. Process names are matched case-insensitively with an optional `.exe` suffix.

Rates can be written as numbers (`1000`, `8000`) or shorthand strings (`"1k"`, `"8k"`).

`power_policy = "first-device"` keeps the original behavior: the watcher only changes the first supported device when the active rule changes. `power_policy = "active-non-charging"` scans supported devices every watcher tick. While a rule is active it applies the rule rate to non-charging devices and keeps charging devices at the idle rate.

For devices that report battery level but not charge state, the watcher treats `100%` as plugged in, then falls back to a rolling battery trend. If the level increases by at least `battery_trend_min_delta` within `battery_trend_window_ms`, the device is treated as charging. If neither signal is available, the connection assumptions are used as a final fallback.

## Design Notes

- The TUI and watcher query the configured polling rate through device control reports, not by sampling pointer movement.
- Device support is implemented as per-vendor protocol adapters under one HID monitor path.
- The watcher scans processes at `scan_interval_ms`, with a minimum interval of 250 ms.
- In first-device mode, the watcher writes only when the desired rule target changes.
- In active-non-charging mode, the watcher polls device power state on each process scan and writes only when a device is not already at its target rate.
- Long-running TUI and watcher sessions keep the last valid rate and battery report for visible devices. If a device is still enumerated but stops answering control reads, the cached report is used for display and power policy decisions. The watcher defers writes when the current rate is only cached and does not match the target, so a sleeping device has to answer again before being changed.
- The long-term path is to keep this HID/control core and add a system tray UI around it later.
