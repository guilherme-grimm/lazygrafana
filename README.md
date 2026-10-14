# lazygrafana

A lazygit/lazydocker-style terminal UI for **operating** Grafana: browse and
edit dashboards, zoom into incidents, silence alerts, tail logs, check
datasources. It uses Vim navigation and draws real pixel charts on terminals that
support it (kitty, Ghostty, WezTerm, …), with a Unicode fallback everywhere else.

Works with Grafana **11+** (OSS/Enterprise) and **Grafana Cloud**.

See [SPEC.md](SPEC.md) for the full design.

## Features

- **Dashboards**:
  - folder tree (nested folders), starred and recent views, fuzzy filter
  - panels rendered in the terminal:
    - time series, stat, gauge, bar gauge, table, heatmap, logs, text
    - placeholders for everything else
  - template variables (query, custom, interval, textbox, constant, datasource), chained and multi-value
  - annotations (built-in and query-based)
  - Variables, Versions (diff any two, restore) and JSON tabs
  - edit JSON in `$EDITOR`, with a diff before saving; star, move, export, delete
- **Time ranges made easy**:
  - crosshair (`h`/`l`) shared across panels, with values in the legends
  - visual-select a range (`v` … `Enter`) to zoom
  - `+`/`-` zoom, `H`/`L` pan, `Ctrl-o`/`Ctrl-i` jumplist
  - mouse drag-to-zoom and wheel zoom
  - zooming re-queries at the chart's pixel resolution; `M` wraps queries in `max_over_time` so short spikes stay visible
- **`gl` — logs for this**: jump from a chart selection to the matching Loki logs, for the same time range.
- **Alerts**: rule state (firing, pending, silenced, paused, error), instances, state history, silences.
  - Silence or unsilence (also in bulk), pause or resume rules.
  - Desktop notifications when alerts start firing.
- **Explore**:
  - PromQL/LogQL against any datasource, with query history
  - metrics as a chart or table
  - logs with follow mode (tail), filter, wrap, and line details (labels, plus fields parsed from JSON or logfmt)
- **Exact view**: `P` shows Grafana's own rendering of a panel (requires the Grafana Image Renderer).
- **Safe for production**:
  - per-context `readonly: true`
  - confirmations with diffs before changes
  - tokens never logged
- **Terminal integration**:
  - kitty graphics / sixel / iTerm2 images with automatic detection
  - OSC 52 clipboard (works over SSH)
  - desktop notifications
  - kitty keyboard protocol
  - mouse

## Install

```sh
cargo install --path .          # from a checkout
# or grab a binary from the GitHub releases page
```

Packaging templates for AUR, Homebrew and Nix live in [`packaging/`](packaging/) and [`flake.nix`](flake.nix).

## Quick start

```sh
# Quickest: environment variables create an implicit "env" context
GRAFANA_URL=https://grafana.example.com GRAFANA_TOKEN=glsa_… lazygrafana

# Or a config file (see below), then:
lazygrafana                        # default context
lazygrafana -x prod -t now-24h     # pick a context and time range
lazygrafana -d <dashboard-uid>     # open a dashboard directly
lazygrafana auth check prod        # verify a context's token
lazygrafana auth set prod          # store a token in the OS keyring
```

Use a Grafana **service account token** (Viewer is enough for browsing; Editor
for edits and silences).

## Configuration

`~/.config/lazygrafana/config.yml` (or `--config`, or `$LAZYGRAFANA_CONFIG`):

```yaml
default_context: staging
default_time: now-6h
refresh: off                 # off | 5s | 10s | 30s | 1m | 5m
graphics: auto               # auto | kitty | sixel | iterm | unicode
unicode_marker: braille      # braille | octant | block (fallback charts)
mouse: true
notifications: true

contexts:
  prod:
    url: https://grafana.example.com
    org: 1
    token_cmd: pass show grafana/prod   # or token_env / token_file / keyring / token
    readonly: true
  staging:
    url: https://grafana-stg.example.com
    token_env: GRAFANA_STG_TOKEN
    ca_file: /etc/ssl/internal-ca.pem
  cloud:
    url: https://mystack.grafana.net    # Grafana Cloud is detected from the URL
    keyring: cloud                      # see `lazygrafana auth set cloud`

keys:                                   # optional overrides: action -> key
  silence: S
  open_browser: o
```

Relative `token_file` paths are resolved against the config file's directory.
State (recent dashboards, query history, logs) is kept in
`~/.local/state/lazygrafana/`.

## Keys

Press `?` in the app for the full list.

| Keys | Action |
|---|---|
| `h` `j` `k` `l`, `gg` `G`, `Ctrl-d` `Ctrl-u` | Move; `h`/`l` switch between side panels and the main pane |
| `1`–`5`, `Tab` | Contexts, Dashboards, Alerts, Explore, Datasources |
| `[` `]` | Previous/next tab in the main pane |
| `/`, `:`, `@`, `?` | Filter, command line, context switcher, help |
| `T`, `R`, `Ctrl-o` `Ctrl-i` | Time range, auto-refresh, time jumplist |
| `h`/`l`, `w`/`b`, `0`/`$` | Crosshair: one point, 1/10 of the range, start/end |
| `v` … `Enter` | Select a range and zoom into it |
| `+` `-` `H` `L` | Zoom in/out, pan left/right |
| `gl`, `a`, `z`, `P`, `M` | Logs for selection, annotate, maximize, exact render, `max_over_time` |
| `e` `s` `m` `x` `dd` | Edit JSON, star, move, export, delete |
| `Space`, `c`, `r` | Versions: mark, diff, restore |
| `S` `U` `p` `F` `gb` | Alerts: silence, expire silences, pause, state filter, group by label |
| `i`, `D`, `Ctrl-r`, `f`, `t` | Explore: edit query, datasource, history, follow, table view |
| `yy` `yu` `yj` `yr`, `o` | Yank URL/UID/JSON/range, open in browser |

Commands:
- `:ctx <name>`
- `:time now-2d to now-1d`
- `:var region=eu,us` (or `=all`)
- `:refresh 30s`
- `:tag <tag>`
- `:open <uid>`
- `:explore <query>`
- `:q`

## Graphics

lazygrafana queries the terminal at startup.
- **Pixel charts:** kitty, Ghostty and WezTerm (kitty graphics protocol); foot, xterm and Windows Terminal (sixel); iTerm2.
- **Unicode charts (braille, octant or block):** everything else, including Alacritty.

Inside tmux, kitty images need `set -g allow-passthrough on`. Override the detection with `-g` or `graphics:` in the config.

## Development

Requirements: Rust (stable), Docker with Compose v2, `make`, `curl`, `jq`.

```sh
make dev-up          # Grafana 13, Prometheus, Loki, Alloy, node-exporter, log generators
make dev-bootstrap   # tokens, folders, versions, 2nd org, annotations, 7d metric backfill
cargo run -- --config dev/lazygrafana.yml

cargo test                                       # unit + UI render tests
LAZYGRAFANA_IT=1 cargo test --test integration   # against the dev stack
```

| Service | URL | Notes |
|---|---|---|
| Grafana | http://localhost:3000 | `admin` / `admin` |
| Grafana 11 (`make dev-up-all`) | http://localhost:3011 | minimum supported version; seed with `make dev-bootstrap-compat` |
| Prometheus | http://localhost:9090 | |
| Loki | http://localhost:3100 | |

The bootstrap script writes service account tokens to `dev/.secrets/` (gitignored).
[`dev/lazygrafana.yml`](dev/lazygrafana.yml) uses them in five contexts:

| Context | What it tests |
|---|---|
| `local` | admin token, org 1 |
| `local-viewer` | viewer token with `readonly: true` |
| `team-b` | a second organization |
| `grafana-11` | the minimum supported Grafana version |
| `broken` | an unreachable instance (error handling) |

What's in the stack:

- **Dashboards** cover every panel type the renderer supports, plus one unsupported type (state-timeline). They also exercise every variable type, a collapsed row, overrides and Prometheus annotations.
- **Synthetic metrics** (`demo_signal`, `demo_requests_per_second`) are computed from `time()`, so bootstrap backfills 7 days of history. A 45s spike every 15 minutes is the reference case for range selection, zooming and aliasing.
- **Alert rules** (*Dev Alerts* folder):
  - `AlwaysFiring`
  - `SpikeDetected`: fires every 15 min
  - `SineHigh`: goes pending → firing → normal
  - `ScrapeDown`: fires when you stop a container
  - `ManualRule`: has no provenance, so it can be paused
- **Logs**: three `flog` generators plus every container's own output, shipped by Alloy with a `service` label.

Other targets: `make dev-down`, `make dev-reset` (wipes data and tokens), `make dev-logs`, `make dev-ps`.

## License

Licensed under either of [Apache License 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
