# poll-monitor

A small Windows-focused mouse polling-rate monitor and switcher.

The current app is a Rust CLI/TUI. It reads the mouse's configured polling rate through HID feature/input reports, can set a target rate, and can watch running processes to apply simple app-specific rate rules.

## Status

This is an early working prototype. It has been tested against:

- G-Wolves Fenrir receiver `33e4:3517`, including read and no-op write.
- IPI Piao wireless receiver `372e:1014`, including read and no-op write.

Supported model IDs currently cover known Fenrir, Fenrir Pro, and Fenir Max wired/receiver IDs from G-Wolves WebHID protocol data, plus IPI Piao/Float-style PIX v1 mouse IDs from `https://shan.ipigame.cn/devices`.

## Usage

Launch the TUI:

```powershell
cargo run
```

List supported devices and current configured polling rates:

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

Rules are checked in file order. The first matching process wins. Process names are matched case-insensitively with an optional `.exe` suffix.

Rates can be written as numbers (`1000`, `8000`) or shorthand strings (`"1k"`, `"8k"`).

## Design Notes

- The TUI and watcher query the configured polling rate through device control reports, not by sampling pointer movement.
- Device support is implemented as per-vendor protocol adapters under one HID monitor path.
- The watcher scans processes at `scan_interval_ms`, with a minimum interval of 250 ms.
- The watcher writes only when the desired rule target changes.
- The long-term path is to keep this HID/control core and add a system tray UI around it later.
