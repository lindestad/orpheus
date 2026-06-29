# poll-monitor

A small Windows-focused G-Wolves Fenrir-family polling-rate monitor.

The current app is a Rust CLI/TUI. It reads the mouse's configured polling rate through HID feature/input reports, can set a target rate, and can watch running processes to apply simple app-specific rate rules.

## Commands

```powershell
cargo run
cargo run -- tui
cargo run -- list
cargo run -- set 1000
cargo run -- init-config
cargo run -- watch --config poll-monitor.toml
```

Use `cargo run -- watch --config poll-monitor.toml --dry-run --once` to validate a config without writing to the mouse.

## Config

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

## Notes

- Supported model IDs are based on G-Wolves Fenrir/Fenrir Pro/Fenir Max WebHID protocol data.
- The TUI and watcher query the configured polling rate through device control reports, not by sampling pointer movement.
- The watcher writes only when the desired rule target changes.
