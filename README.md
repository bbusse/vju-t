# vju-t

The versatile terminal widget

## Build

```sh
git clone https://github.com/bjoernbusse/vju-t.git
cd vju-t
cargo build --release
```

The binary will be at `target/release/vju-t`.

## Install

```sh
cargo install --path .
```

Or directly from the repository:

```sh
cargo install --git https://github.com/bjoernbusse/vju-t.git
```

## Usage

```sh
vju-t [OPTIONS] <command> [arguments...]
```

### Options

```
--big-text                    Render output as large text
--pie-chart                   Render output as pie chart
--bar-chart                   Render output as bar chart
--status-circle               Render status as circle
--status-rect                 Render status as rectangle
--status-circle-with-text     Circle with text overlay
--status-rect-with-text       Rectangle with text overlay
--status-rect-with-static-text <text>  Rectangle with static text overlay
--status-rect-with-static-icon <icon>  Rectangle with built-in static icon (e.g. KEY_ICON)
--watch [<duration>]          Re-run command periodically (default: 60s)
--select                      Enable output line selection (text mode)
--title <text>                Set pane title
--description <text>          Description shown in info overlay (v)
--border-colour <colour>      Border colour (e.g. #ff0000, cyan)
--title-colour <colour>       Title colour
--no-frame                    Disable border frame
```

Duration units: `ms`, `s`, `m`, `h`, `d`

### Keys
```
q         Quit
v         Toggle info overlay
r         Re-run command
Up/Down   Scroll output (or cycle selection with --select)
PgUp/PgDn Scroll by page
End       Resume auto-scroll
```

### Examples
```sh
# Monitor memory usage
vju-t --watch 5s --title 'Memory' free -h

# Big text CPU load
vju-t --watch 2s --big-text --title 'Load' cat /proc/loadavg

# Disk usage with periodic refresh
vju-t --watch 10s --title 'Disk usage' df -h

# Status circle based on host reachability
vju-t --watch 30s --status-circle ping -c1 -W2 8.8.8.8

# With description for the info overlay
vju-t --watch 5s --description "System load over 1/5/15 min" --big-text --title 'Load' uptime
```

Run `vju-t --help` for all options.

## Resources
[Ratatui](https://docs.rs/ratatui/latest/ratatui/index.html)
