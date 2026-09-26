<div align="center">

<img src="crates/germal-app/assets/logo/germal.png" width="128" alt="Germal">

# Germal

**A native desktop HTTP API client with a built-in load tester and a browser traffic recorder, written in Rust with [GPUI](https://gpui.rs).**

[![License](https://img.shields.io/badge/License-Apache%202.0-007EC6?style=flat-square)](LICENSE)
[![Rust](https://img.shields.io/badge/Rust-1.97%2B-CE422B?style=flat-square&logo=rust&logoColor=white)](https://www.rust-lang.org)

<img src="assets/screenshot.png" width="900" alt="Germal main window: request builder on the left, response viewer on the right">

</div>

> Germal is a fork of [GetCat](https://github.com/finch-xu/GetCat) by [finch-xu](https://github.com/finch-xu), used under the Apache-2.0 license.

## Features

### Requests

- Methods: GET, POST, PUT, PATCH, DELETE, HEAD, OPTIONS.
- Path parameters (`{name}` in the URL), query parameters and headers.
- Bodies: form-data (text and file fields), x-www-form-urlencoded, raw JSON / Text / XML, or a binary file.
- Large responses are streamed with live progress and can be cancelled. Up to 5 MB opens in the highlighted editor, up to 64 MB in a virtualized line view, and anything larger is written to disk with a preview and a save button.
- Server-sent events render as they arrive. OpenAI Chat Completions / Responses and Anthropic Messages streams are recognized, with event list, assembled text and raw views, plus time to first token, event count, token usage and generation rate.
- Copy a request as cURL or Python, or paste a cURL command to open it as a new tab.
- Variables at global, category and environment scope (`{{var}}`, `{{$timestamp}}`), pre-request variable setting, post-response extraction and assertions, Postman environment import / export, and masked secret variables.
- Saved requests, drafts and settings are plain JSON files. Nothing is uploaded and no response history is stored.

### Load tester

- Test any request: the current tab's, or one from the recorder. The target's method and URL are editable.
- Set the number of requests and the concurrency.
- A dedicated page updates live while it runs: progress, requests sent, completed, failed and in flight, elapsed time (in seconds, minutes, hours or days), requests per second, status code distribution, failure reasons, and latency min / p50 / p90 / p99 / max.
- Target host details from DNS: IPv4 and IPv6 addresses, hosting provider (ASN), reverse hostname, network range and DNS servers.
- Copy the results as text.

### Recorder

- Opens a Chromium window and records the API (xhr / fetch) requests of every page you visit into a local SQLite database.
- For each request: method, URL, request and response headers (including Authorization and cookies), request and response bodies, status, timing (DNS, connect, TLS, send, wait), size, protocol, remote IP, TLS certificate details, initiator and redirect chain.
- Organized by project (with a project dropdown), then by domain and subdomain, with All / GET / POST / Other filters.
- A running clock shows how long the recording has been going.
- Right-click a request to copy its URL, copy it as cURL, or send it to the load tester.

### `.germal` files

`germal-rec export <file>` compresses a recording database with zstd and then encrypts it with AES-256-GCM into a single `.germal` file. The key is derived from a passphrase with Argon2id. `germal-rec unpack <file> <db>` restores it. The passphrase is prompted for, or read from `GERMAL_PASSPHRASE`.

### Command line

`germal-rec` records, lists, imports, exports and load-tests from the terminal:

```bash
germal-rec record --project "My project" --url https://example.com
germal-rec list --method POST --host api.example.com
germal-rec load <id> -n 200 -c 20
germal-rec export recordings.germal
germal-rec unpack recordings.germal restored.db
```

## Install

Packages are attached to [GitHub Releases](https://github.com/nermalcat69/germal/releases) once a release is published.

| Platform | File |
|---|---|
| macOS (Apple Silicon) | `Germal-macos-arm64.dmg` |
| macOS (Intel) | `Germal-macos-x64.dmg` |
| Linux (x64) | `Germal-linux-x64.tar.gz` |
| Linux (arm64) | `Germal-linux-arm64.tar.gz` |
| Windows (x64) | `Germal-windows-x64.exe` (portable) or `.msi` (installer) |
| Windows (arm64) | `Germal-windows-arm64.exe` (portable) or `.msi` (installer) |

The recorder needs Google Chrome or Chromium installed.

Linux needs Vulkan drivers (`vulkaninfo --summary` should list a device) and glibc 2.35 or newer, for example Ubuntu 22.04+, Debian 12+ or Fedora 36+. Windows needs Windows 10 1803 or later.

## Usage

1. Pick a method, type a URL and press **⌘ Enter** (Ctrl Enter on Windows / Linux).
2. Fill in parameters under the Params / Headers / Body tabs.
3. The response pane shows status, time and size, switches between Pretty and Raw, searches with **⌘ F** and saves to a file.
4. **⌘ S** saves the request to the sidebar. Saved requests can be grouped into categories.
5. The left rail also holds the load tester and the recorder.

| Action | macOS | Windows / Linux |
|---|---|---|
| Send | ⌘ Enter | Ctrl Enter |
| New tab / close tab | ⌘ T / ⌘ W | Ctrl T / Ctrl W |
| Collapse sidebar | ⌘ B | Ctrl B |
| Save request | ⌘ S | Ctrl S |
| Search in response | ⌘ F | Ctrl F |
| Settings | ⌘ , | Ctrl , |

### Data directory

| Platform | Directory |
|---|---|
| macOS | `~/Library/Application Support/Germal/` |
| Linux | `$XDG_DATA_HOME/germal/` (default `~/.local/share/germal/`) |
| Windows | `%APPDATA%\Germal\data\` |

```
workspace.json          # tabs, sidebar, split direction, theme
requests/<ulid>.json    # one file per saved request
drafts/<tab-id>.json    # one draft per tab
settings.json           # application settings
recordings.db           # recorder database (SQLite)
```

An existing GetCat data folder is moved to the Germal location on first launch. Headers such as `Authorization` are stored in plain text, with 0600 file permissions on Unix.

## Development

```
crates/
├─ germal-core      # request model, sending (reqwest + tokio), load tester, host/DNS lookup, JSON storage
├─ germal-recorder  # Chromium recorder (chromiumoxide), SQLite storage, .germal files, germal-rec CLI
└─ germal-app       # GPUI interface
```

Requires Rust 1.97 or newer. On Linux, install Vulkan and the Wayland / X11 / fontconfig development headers (the full list is in `.github/workflows/ci.yml`). On Windows, install the MSVC toolchain.

```bash
cargo run -p germal-app                         # run the app
cargo run -p germal-recorder -- --help          # recorder CLI
cargo test --workspace                          # tests
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
```

`tools/testserver/server.py` is a local test server (slow and huge responses, chunked bodies, LLM event streams, arbitrary status codes, mid-transfer disconnects): `python3 tools/testserver/server.py`.

GitHub Actions: `ci.yml` checks formatting, lints and tests on every push. `release.yml` builds all platform packages when started manually or when a `vX.Y.Z` tag is pushed, and creates a draft release for tags.

## License

[Apache-2.0](LICENSE). Third-party dependencies are listed in [THIRD-PARTY.md](THIRD-PARTY.md).

## Acknowledgements

Germal is a fork of [GetCat](https://github.com/finch-xu/GetCat) by finch-xu. The request builder, response viewer, variables and the rest of the base app come from GetCat; the load tester, recorder and `.germal` files are added here.
