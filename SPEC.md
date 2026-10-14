# lazygrafana — Specification

> A lazygit/lazydocker-style terminal UI for **operating** Grafana: alerts, logs,
> dashboards and datasources, with Vim navigation and real pixel charts on
> modern terminals.

Status: **v0.1–v0.5 implemented** (2026-10-14). See §17 for implementation notes and deviations.

---

## 1. Goals and non-goals

### Goals
- Make the frequent Grafana operations fast from the keyboard: find a
  dashboard, silence an alert, check what a metric did, tail the logs around
  an incident, diff and roll back a dashboard.
- **Make time-range selection easy and precise**: crosshair, visual-select a
  range, zoom, step back and forth through a zoom history.
- Render charts as real pixel graphics on terminals that support it (kitty,
  Ghostty, WezTerm, …) and degrade cleanly to Unicode elsewhere (Alacritty,
  tmux, SSH).
- Be safe to point at production (per-context read-only mode, confirmations,
  no secrets in logs).
- Work against self-hosted Grafana **11+** and **Grafana Cloud** from day one.

### Non-goals
- Pixel-perfect replication of every Grafana panel type. Unsupported panels
  show a placeholder; `o` opens them in the browser.
- Visual (WYSIWYG) dashboard editing. Editing is JSON in `$EDITOR` + diff.
- Replacing Grafana's web UI for building dashboards or configuring alerting.

### Prior art and differentiation
| Project | Focus | Gap lazygrafana fills |
|---|---|---|
| grafana-tui (Go, Bubble Tea) | Browse/view dashboards | No operations (alerts, silences, logs, versions), Unicode charts only |
| grot | btop-style dashboards from JSON | Viewer, not an operator tool |
| grafatui (Rust) | Prometheus TUI importing dashboard JSON | Talks to Prometheus, not Grafana |
| grafterm | JSON-defined terminal dashboards | Mostly inactive, fixed widgets |

---

## 2. Compatibility

| Area | Target |
|---|---|
| Grafana | OSS / Enterprise **11.0+**, Grafana Cloud. CI tests against the oldest supported minor (11.6) and the current release |
| Dashboard schema | Classic JSON schema (v1) first. Schema v2 / `dashboard.grafana.app` APIs (Grafana 12+) in a later milestone |
| Alerting | Grafana-managed (unified) alerting only. Legacy alerting was removed in Grafana 11 |
| Datasources (rich rendering) | Prometheus and Loki first-class; any datasource returning data frames renders through the generic path |
| Terminals | Any terminal with truecolor works. Rich graphics on kitty-graphics or sixel terminals (see §6) |
| OS | Linux, macOS. Windows best-effort (Windows Terminal supports sixel) |

### Grafana Cloud notes
- Same auth (service account token, `Authorization: Bearer glsa_…`).
- A stack has a single org; server-admin APIs (`/api/admin/*`, `/api/orgs`) are
  unavailable → hide those features when detected.
- Respect rate limits: back off on `429` and honour `Retry-After`.
- Detection: URL host ending in `.grafana.net`, or `buildInfo` in
  `/api/frontend/settings`. Overridable with `cloud: true|false` per context.

---

## 3. Layout

```
┌─ [1] Contexts ──────────┐┌─ Dashboard: API Latency ─ [Panels] Variables  Versions  JSON ─┐
│ ● prod   (ro)  v13.2    ││                                                                 │
│   staging      v13.2    ││   (main pane: depends on the focused side panel + active tab)   │
├─ [2] Dashboards ────────┤│                                                                 │
│ ▾ Platform              ││                                                                 │
│   > API Latency     ★   ││                                                                 │
│ ▸ Data                  ││                                                                 │
├─ [3] Alerts (2🔥) ──────┤│                                                                 │
├─ [4] Explore ───────────┤│                                                                 │
├─ [5] Datasources ───────┤│                                                                 │
└─────────────────────────┘└─────────────────────────────────────────────────────────────────┘
 o open  y yank  e edit  / search  T time  ? help                    prod · now-6h · ⟳ 30s
```

- Left: numbered, stacked side panels. The focused one expands; the others
  collapse to a few rows (as in lazygit).
- Right: main pane with tabs that depend on what the side panel has selected.
- Bottom: context-sensitive key hints on the left; context, time range and
  refresh interval on the right.
- `:` opens a command line on the bottom row; `?` a searchable help popup.
- Narrow terminals (< 100 cols): side panels and main pane stack vertically,
  with `Tab` switching between them.

---

## 4. Panels and actions

### [1] Contexts
Grafana instance + org pairs from the config file.
- Shows: name, version, health, read-only badge, Cloud badge.
- `Enter` switch, `t` re-check health, `@` (global) quick switcher popup.

### [2] Dashboards
Folder tree (including nested folders), with `Starred`, `Recent` and tag filter views.
- Main pane tabs:
  - **Panels**: rich or Unicode rendering of the dashboard's panels (§5, §6)
  - **Variables**: current values; edit inline, with multi-select and "All"
  - **Versions**: list, diff any two (`d`), restore (`r`)
  - **JSON**: syntax-highlighted, read-only
- Actions: `o` open in browser, `y`-family yank, `e` edit JSON in `$EDITOR`
  (diff, then confirm push), `s` star/unstar, `m` move to a folder, `x` export
  to a file, `dd` delete (confirm), `P` exact panel render (§6.4).

### [3] Alerts
Rule instances grouped by folder → group, or by a chosen label (`gb`).
- Filters: state (`firing`, `pending`, `normal`, `silenced`, `error`), label matchers via `/`.
- Main pane tabs:
  - **Details**: labels, annotations, query, the rule's own chart around the firing time
  - **History**: state timeline, where state history is enabled
  - **Silences**: silences matching this alert
- Actions: `S` silence (duration presets, matchers prefilled from labels,
  comment), `U` expire silence, `p` pause/resume rule, `o` open in browser.
  Bulk actions on marked items (`Space` / `v`).
- Notifications: an alert entering `firing` triggers a desktop notification
  (§7) when enabled.

### [4] Explore
- Datasource picker, query input (PromQL / LogQL), time range shared with the rest of the app.
- Metric results: chart and table views (`t` toggles).
- Logs mode:
  - `f` follow (tail)
  - `/` filter
  - level colouring
  - `w` wrap
  - `Enter` on a line shows labels and parsed fields
  - `J` pretty-prints JSON lines
- Query history persisted locally and searchable (`Ctrl-r`).

### [5] Datasources
- List with type, default flag and URL. `t` health check; details pane.

### Later
Annotations browser, users/teams/service accounts, provisioning view, library panels.

---

## 5. Time ranges and chart interaction

Every chart has a **crosshair cursor** and supports range selection. The
crosshair is **shared across all panels** of a dashboard (like Grafana's
shared tooltip), and its values are shown in a tooltip.

| Key | Action |
|---|---|
| `h` / `l` | Move crosshair by one data point |
| `w` / `b` | Move by 1/10 of the visible range |
| `0` / `$` | Start / end of range |
| `v` … `Enter` | Visual-select a range, then zoom into it (`Esc` cancels) |
| `+` / `-` | Zoom in/out around the crosshair (×2) |
| `H` / `L` | Pan by half the visible range |
| `Ctrl-o` / `Ctrl-i` | Back / forward through the time-range history (jumplist) |
| `T` | Time-range picker: presets, relative (`now-6h`), absolute (RFC 3339 / `2026-10-14 13:00`) |
| `R` | Auto-refresh interval picker (off, 5s, 10s, 30s, 1m, 5m) |
| Mouse drag | Select a range. Pixel-precise with SGR-Pixels (mode 1016), otherwise cell-precise |
| Mouse wheel | Zoom around the pointer |

Acting on a selection:
- `gl` (with a selection): open **Explore/logs** for that exact range, with
  label matchers carried over from the series under the crosshair.
- `a`: add an annotation over the selection.
- `y`: yank the range as `from=…&to=…` / a Grafana URL.

Data resolution:
- Every (re)query sets `maxDataPoints` to the chart's **pixel** width (cell
  width in Unicode mode) and `intervalMs` to the range divided by that. Zooming
  therefore fetches finer data instead of stretching.
- **Aliasing**: short events disappear at coarse steps. Example from the dev
  stack: the 45s spike every 15 min is invisible over 7 days at a ~10 min
  step. Mitigations, in order:
  1. Re-query on zoom.
  2. Show an "events may be hidden at this resolution" hint when the step is
     much larger than the scrape interval.
  3. Optional `max_over_time` rewrite toggle (`M`) for Prometheus.

---

## 6. Rendering

### 6.1 Tiers and detection
| Tier | Terminals | Technique |
|---|---|---|
| **Kitty graphics** | kitty, Ghostty, WezTerm | `plotters` → RGBA bitmap → kitty protocol via `ratatui-image`; images reused by ID |
| **Sixel** | foot, WezTerm, Windows Terminal, xterm (with sixel enabled), tmux ≥ 3.4 built with sixel | Same bitmap, quantised to sixel |
| **iTerm2 inline** | iTerm2, WezTerm | Same bitmap |
| **Unicode** | everything (Alacritty, plain tmux, SSH without passthrough) | Octants / braille / block elements + truecolor |

- Detection at startup: terminal queries (kitty graphics query, Primary Device
  Attributes for sixel, `CSI 16 t` for cell pixel size) with a timeout, plus
  `$TERM` / `$TERM_PROGRAM` heuristics. Inside tmux, use passthrough only when
  `allow-passthrough` is on. kitty images in tmux use Unicode placeholders.
- Override: `graphics: auto | kitty | sixel | iterm | unicode` in config,
  `--graphics` on the CLI.

### 6.2 Renderer abstraction
All charts go through a single `Renderer` interface from v0.1, even though only
the Unicode renderer exists at first. Each chart is described as a
backend-independent **ChartSpec**:
- series
- axes
- thresholds
- annotations
- crosshair
- selection

Each renderer turns a ChartSpec into widgets.

### 6.3 Base + overlay layers
To keep crosshair movement instant (and cheap over SSH):
- **Base layer**: the chart bitmap. Re-rendered only when data, size or theme change.
- **Overlay layer**: crosshair, selection band, hover marker. This is a small
  separate image placement with a higher z-index (kitty), or a re-blit of the
  affected region (sixel).
- **Unicode tier**: the overlay is drawn as cell styling on top of the text chart.
- Popups over images: kitty uses z-ordering. Sixel hides the images under the
  popup and redraws them afterwards.

### 6.4 Exact view (optional)
`P` on a panel fetches `/render/d-solo/:uid/:slug?panelId=…&from=…&to=…&width=…&height=…`
when the Grafana Image Renderer is available, and displays the PNG. This gives
pixel-identical panels. It is slow (seconds), so it is on-demand only and cached
per (panel, range, size).

### 6.5 Panel type coverage (rich + Unicode)
| Panel | Coverage |
|---|---|
| timeseries | lines and bars; stacking, thresholds, negative-Y overrides, units |
| stat | value, colour mode, sparkline |
| gauge / bargauge | supported |
| table | supported, including the hidden-column overrides |
| heatmap | pre-bucketed (`format: heatmap`); calculated heatmaps later |
| logs | supported |
| text | markdown rendered as styled text |
| Anything else (state-timeline, geomap, plugin panels, …) | placeholder with title, type and `o` / `P` hints |

---

## 7. Terminal integrations

| Feature | Use | Fallback |
|---|---|---|
| OSC 52 | `y` yanks to the *local* clipboard, also over SSH | system clipboard via `arboard` when local; otherwise show the text |
| OSC 8 | dashboard/alert names are clickable links | plain text |
| Kitty keyboard protocol | distinguish `Ctrl-i`/`Tab`, `Ctrl-m`/`Enter`, etc. | legacy key handling; affected default bindings have alternates |
| Synchronized output (mode 2026) | no tearing during redraws | none needed |
| Desktop notifications (OSC 99 kitty, OSC 9 / 777 others) | alert transitions to firing | disabled |
| SGR mouse, SGR-Pixels (1016) | optional mouse support | keyboard only |

**Mouse is an extra.** Every action must be reachable from the keyboard. The
mouse adds click-to-focus, wheel scroll/zoom and drag-to-select.

---

## 8. Keybindings

nvim-flavoured, every binding remappable in config. Hints shown in the bottom bar.

### Global
| Key | Action |
|---|---|
| `h` `j` `k` `l`, arrows | Move. `h`/`l` at panel edges moves focus between side and main pane |
| `gg` / `G`, `Ctrl-d` / `Ctrl-u` | Top/bottom, half page down/up |
| `1`–`5`, `Tab` / `Shift-Tab` | Focus side panel N, cycle panels |
| `[` / `]` | Previous / next main-pane tab |
| `/`, `n` / `N` | Fuzzy filter the current list, next/previous match |
| `:` | Command line (`:ctx`, `:time`, `:var`, `:refresh`, `:q`, …) |
| `Space`, `v` | Mark item, visual (range) select |
| `yy` / `yu` / `yj` / `yr` | Yank URL / UID / JSON / time range |
| `o` | Open in browser |
| `@` | Context switcher |
| `T`, `R` | Time range, refresh interval |
| `Ctrl-o` / `Ctrl-i` | Time-range jumplist |
| `?`, `Esc`, `q` | Help, back/cancel, quit |

Panel-specific keys are listed in §4 and §5.

### Destructive actions
Delete, restore version, push edited JSON, expire silence, and pause rule all
require confirmation. The confirmation prompt shows a diff or summary of the
change. Delete uses a double key (`dd`).

---

## 9. Data access

All data goes through the Grafana HTTP API, never directly to datasources. This
respects Grafana permissions and works on Grafana Cloud.

| Purpose | Endpoint |
|---|---|
| Health, version | `GET /api/health`, `GET /api/frontend/settings` |
| Identity | `GET /api/user`, `GET /api/org` |
| Search | `GET /api/search?type=&query=&tag=&starred=&folderUIDs=` |
| Folders (nested) | `GET /api/folders?parentUid=` |
| Dashboard | `GET /api/dashboards/uid/:uid`, `POST /api/dashboards/db`, `DELETE /api/dashboards/uid/:uid` |
| Versions | `GET /api/dashboards/uid/:uid/versions[/:id]`, `POST /api/dashboards/uid/:uid/restore` |
| Stars | `POST/DELETE /api/user/stars/dashboard/uid/:uid` |
| Queries | `POST /api/ds/query` (data frames) |
| Variable values | `GET /api/datasources/uid/:uid/resources/…` (e.g. Prometheus `api/v1/label/:name/values`, `api/v1/series`; Loki `label/:name/values`) |
| Datasources | `GET /api/datasources`, `GET /api/datasources/uid/:uid/health` |
| Alert rule state | `GET /api/prometheus/grafana/api/v1/rules` |
| Alert instances | `GET /api/alertmanager/grafana/api/v2/alerts` |
| Silences | `GET/POST /api/alertmanager/grafana/api/v2/silences`, `DELETE …/silence/:id` |
| Rule pause/edit | `GET/PUT /api/v1/provisioning/alert-rules/:uid` (rules provisioned from files are read-only) |
| Annotations | `GET/POST /api/annotations` |
| Exact render | `GET /render/d-solo/:uid/:slug` |

Implementation notes:
- **Hand-written client** with typed `serde` models for the endpoints above.
  Unknown fields are kept, so we round-trip dashboard JSON losslessly.
- **Data frames**: one parser for the `/api/ds/query` frame JSON (schema +
  columnar values, including `nanos` fields and labels per field). Everything
  downstream (charts, tables, logs) consumes the parsed frames.
- **Template variables** (the hardest part; client-side, as Grafana's frontend does it):
  - Types: custom, constant, textbox and interval first; then query variables
    (Prometheus `label_values`/`query_result`, Loki `label_values`).
  - Query definitions may be a string or an object.
  - Multi-value and "All" (including `allValue`).
  - Formats: `${var:regex}`, `${var:csv}`, `${var:pipe}`, `${var:json}`, `${var:raw}`, …
  - Chained variables, resolved in dependency order.
  - Built-ins: `$__interval`, `$__rate_interval`, `$__range`, `$__from`/`$__to`,
    `$__auto`. Grafana resolves most of these server-side when `intervalMs` and
    `maxDataPoints` are passed.
- **Log tailing**: v0.3 polls `/api/ds/query` with a moving start (deduplicated by
  timestamp + line hash). Grafana Live streaming is a later option.
- **Concurrency**:
  - Per-context connection pool.
  - Queries for visible panels first.
  - In-flight queries are cancelled when the time range or variables change.
  - Debounce rapid zoom/pan steps (~150 ms).

---

## 10. Configuration

Location: `$XDG_CONFIG_HOME/lazygrafana/config.yml` (`~/.config/…`), or `--config`.
Relative paths inside the file resolve against the file's directory.

```yaml
default_context: staging
default_time: now-6h
refresh: off                 # off | 5s | 10s | 30s | 1m | 5m
graphics: auto               # auto | kitty | sixel | iterm | unicode
mouse: true
notifications: true
theme: auto                  # auto | dark | light | <custom name>

contexts:
  prod:
    url: https://grafana.example.com
    org: 1
    token_cmd: pass show grafana/prod   # one of: token_cmd | token_env | token_file | keyring
    readonly: true
  staging:
    url: https://grafana-stg.example.com
    token_env: GRAFANA_STG_TOKEN
    ca_file: /etc/ssl/internal-ca.pem   # optional
    insecure_skip_verify: false
  cloud:
    url: https://mystack.grafana.net
    keyring: lazygrafana/cloud           # token stored in the OS keyring

keys:                         # optional overrides, action -> key
  open_browser: o
  silence: S
```

- Credentials are never written to the config by lazygrafana. `keyring` is
  populated with `lazygrafana auth set <context>`.
- State (query history, last context, recent dashboards) lives in
  `$XDG_STATE_HOME/lazygrafana/`.

---

## 11. Safety

- `readonly: true` hides and blocks every mutating action for that context,
  regardless of token permissions. The context shows a `ro` badge.
- Destructive actions confirm with a diff/summary (§8).
- Tokens are redacted in logs, errors and `--debug` output. The log file is
  created with `0600`.
- No telemetry.

---

## 12. Architecture

- **Rust**, edition 2024, MSRV = latest stable at first release.
- Libraries:

| Purpose | Crates |
|---|---|
| TUI | `ratatui` + `crossterm` |
| Images | `ratatui-image` |
| Chart bitmaps | `plotters` (bitmap backend) |
| Async | `tokio` |
| HTTP | `reqwest` (rustls, no OpenSSL) |
| Serialization, config | `serde`, `serde_json`, `serde_yaml`-compatible YAML crate |
| Errors | `color-eyre` |
| Logging | `tracing` + `tracing-appender` (to file) |
| Secrets | `keyring` |
| Clipboard | `arboard` |
| Diffs | `similar` |
| Fuzzy filter | `nucleo` |

- Pattern: Ratatui **component** architecture.
  - Components handle keys and send `Action`s.
  - A central loop applies Actions to app state and renders.
  - Background tokio tasks (polling, tailing, health checks) report back over channels.
- Module layout (single crate to start; `grafana` can become its own crate later):
  ```
  src/
    main.rs, app.rs, action.rs, config.rs, keymap.rs
    term/        capability detection, OSC 52/8/notifications
    grafana/     client, models, dataframe, variables, alerting
    render/      ChartSpec, Renderer trait, unicode, image (kitty/sixel/iterm)
    components/  contexts, dashboards, alerts, explore, datasources, main_pane, popups
  ```

---

## 13. Development and testing

- **Dev stack**: `dev/` (see README).
  - Grafana 13 + Prometheus + Loki + Alloy + log generators, plus optional
    Grafana 11 and the image renderer.
  - Provisioned dashboards cover every panel type in §6.5, plus variables,
    collapsed rows, annotations and overrides.
  - Alert rules have predictable behaviour.
  - The bootstrap script adds:
    - tokens
    - nested folders
    - a dashboard with 3 versions
    - a second org
    - 7 days of backfilled metric history
- **Tests**:
  - Unit tests: variable interpolation, data-frame parsing, time-range math,
    keymap parsing. Use real response fixtures recorded from the dev stack.
  - UI snapshots: `insta` + Ratatui `TestBackend`, Unicode tier.
  - API: `wiremock` mocks for error paths (401, 403, 429, timeouts).
  - Integration (CI): run the dev stack via docker compose against Grafana 11.6
    and the current release.
- **CI**:
  - `cargo fmt --check`, `clippy -D warnings`, tests
  - `cargo deny` for licenses and advisories
  - builds for Linux and macOS

## 14. Distribution
- `cargo-dist`: GitHub releases with Linux (gnu + musl) and macOS (x86_64 + aarch64) binaries, plus an installer script.
- `cargo install lazygrafana` (crates.io).
- AUR (`lazygrafana-bin`), Homebrew tap, Nix flake.
- License: MIT OR Apache-2.0.

---

## 15. Roadmap

| Milestone | Scope |
|---|---|
| **Spike** | One PromQL query via `/api/ds/query` → pixel chart + crosshair + range selection + zoom, with Unicode fallback. Test in kitty, Ghostty, Alacritty and tmux. Validates §5/§6 before the architecture depends on them |
| **v0.1** | Config + contexts, dashboard browser (folders, search, starred), `o`/`y`, Vim navigation, help, command line, terminal detection |
| **v0.2** | Alerts + silences, notifications |
| **v0.3** | Explore (metrics + logs, tailing), query history |
| **v0.4** | JSON edit / diff / versions / restore, star/move/delete |
| **v0.5** | Panels tab: rich rendering of §6.5 panel types, variables, shared crosshair, exact view |
| **later** | Schema v2 dashboards, annotations browser, admin views, Grafana Live streaming |

## 16. Open questions
- Theme: follow the terminal background (OSC 11 query) or Grafana's light/dark setting?
- Should the Panels tab follow the dashboard's grid layout, or a simplified
  vertical list? Maybe a toggle: grid looks right, a list fits small terminals.
- Loki live tail: Grafana Live websocket vs polling — measure the polling cost
  first.
- Windows support level.
- Exact view sizing: Grafana 13.2 + renderer v5.12 returned a 1000×500 PNG
  for a `width=900&height=320` request. Check how sizing/`scale` parameters
  behave before relying on them for cell-accurate placement.

---

## 17. Implementation status (2026-10-14)

Everything in the roadmap (spike, v0.1–v0.5) is implemented. Verification:
- 78 unit and UI tests: every view and popup is rendered at many terminal sizes
  through ratatui's `TestBackend`.
- Integration tests against Grafana 13.2 and 11.6 (`LAZYGRAFANA_IT=1`).
- Manual runs in tmux (Unicode), and in kitty and Ghostty (pixel charts,
  screenshots taken on a virtual X display).

Deviations from the original plan:

| Spec | Implemented | Why |
|---|---|---|
| `plotters` for chart bitmaps | `tiny-skia` for the plot area; axis labels stay in terminal text | Crisp labels in the terminal's own font, no font stack, anti-aliased paths/dashes/fills |
| `keyring` crate | `keyring:` via `secret-tool` (Secret Service) / `security` (macOS) + `lazygrafana auth set` | Avoids a heavy dependency stack; same behaviour |
| `cargo-dist` | Hand-written GitHub Actions release workflow + AUR/Homebrew/Nix templates | No generator needed; easy to adjust |
| Overlay as separate kitty placement (§6.3) | Base bitmap cached, overlay composited onto a copy, one stable image ID per chart (re-transmission replaces the image in place) | Simpler, still cheap (no re-rasterisation on crosshair moves); revisit if SSH bandwidth becomes an issue |
| Pixel-precise mouse (SGR-Pixels) | Cell-precise mouse | crossterm does not parse mode 1016 |
| Exact view sizing | `width`/`height`/`tz`/`var-*` passed; the image is fitted to the pane | Confirms the open question in §16: the renderer may return a different size, so we scale |

Not done yet:
- **Theme**: the palette is dark-only and the `theme` setting is ignored. Still
  open, see §16.
- **OSC 8** links cover the URL lines in Contexts, Datasources and the dashboard
  preview, not every name in every list.
- **Schema v2 dashboards** and the other "later" items in §15.

Notable behaviours found while building:
- Prometheus omits samples where an annotation expression is false. Annotation
  regions therefore break on gaps longer than the step, as well as on nulls.
- Alert state history timestamps arrive in microseconds. The frame parser
  normalises µs/ns timestamps to milliseconds.
- File-provisioned alert rules cannot be paused through the API. lazygrafana
  says so up front. Rules with no provenance (created in the UI) keep it.
- Grafana's alert scheduler only accepts group intervals that are multiples
  of 10s, which affects the dev provisioning.
