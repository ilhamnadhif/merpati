# Changelog

All notable changes to Merpati. The newest version comes first.

## 0.2.0 — 2026-10-30

### Added
- **Native macOS menu bar**: Merpati, File, Edit, View, Window and Help menus with the usual
  shortcuts, **Check for Updates…**, Settings…, links to the repository, release notes and issues.
  On Linux and Windows, **Check for updates…** is in the workspace menu.
- **Linux packages**: `.deb` (Ubuntu, Debian, Mint, Pop!_OS), `.rpm` (Fedora, RHEL, Rocky, Alma,
  openSUSE), AppImage (Arch, Manjaro and others) and `.tar.gz`, for x86_64 and ARM64, built
  against glibc 2.28. The release workflow installs them on Ubuntu 20.04/24.04, Debian 11/12,
  Fedora 41, Rocky Linux 8/9 and openSUSE Leap 15.6 before publishing.
- **Windows installer** (`Merpati-Setup-<version>-x64.exe`): per-user or all-users install, Start
  menu entry, optional `merpati` on `PATH`, updates in place; the portable `.zip` stays.
- **Updates per kind of install**: an AppImage replaces itself and restarts like the macOS app; a
  `.deb`/`.rpm` install gets the new package, Windows the new installer (or `.zip`).
- **Response folding**: line numbers and fold carets in the Pretty view for JSON, XML/HTML and
  YAML (`"address": { … } 3 keys`), Expand all / Collapse all and collapse to level 1 or 2.
- **JSONPath filter** on JSON responses (`$.data[?(@.price < 10)].title`) with a match count, in
  the Pretty and Tree views; the evaluator lives in `merpati-core` and is tested against
  Goessner's examples.
- **Value actions**: right-click a value to copy it or its path, **Set as variable…** (environment,
  collection or globals; optionally also set on every run by a script line) or **Add test for this
  value**.
- **Compare with the previous response**: status, time, size and header changes and a line diff of
  the bodies (JSON compared with sorted keys), with a pin to keep a baseline.
- **Generate tests** from a response: status, response time, Content-Type and a JSON schema
  check, previewed before they are added.
- **Maximize and minimize the response panel**, in both layouts.
- **Copy as cURL (as sent)** in the response menu.
- New request tabs put the caret in the URL field; the active tab scrolls into view.
- If Merpati ever closes unexpectedly, the details are written to `crash.log` in its settings
  folder (only on this computer) and the next start says so.

### Fixed
- The app closed when an HTML or XML response contained non-ASCII text (for example the "★" on
  jsonplaceholder.typicode.com); highlighted text is never cut inside a character any more.
- Response bodies no longer shake while scrolling: trackpad gestures stick to one axis, and large
  bodies keep the width of their longest line.
- The text cursor of an empty field was short and sat below the middle; focused inputs now show a
  calmer accent outline.
- An https:// PAC (proxy auto-config) file could fail to download when nothing had set up TLS yet.
- History rows with long URLs no longer overlap the status code or widen the sidebar.

## 0.1.0 — 2026-10-25

First public release: a native API client for macOS (Apple Silicon and Intel), with builds for
Linux and Windows.

### Requests and responses
- HTTP requests with every method, params and `:path` variables synced with the URL, headers with
  autocomplete, and form-data, urlencoded, raw, binary and GraphQL bodies.
- Responses as Pretty, Raw, Preview or Tree, with headers, cookies, test results, timings and
  search; large responses are streamed to disk and shown virtually.
- `pm.visualizer` rendered inside the app (HTML and a CSS subset with grid, flexbox, floats and
  positioning).
- Cookie jar, extra CA and per-host client certificates (PEM, PKCS#12), proxies (HTTP, HTTPS,
  SOCKS, system settings, PAC, WPAD), HTTP/1.1 and HTTP/2, Unix sockets and Windows named pipes.
- Authentication: API key, Basic, Bearer, Digest, JWT, OAuth 1.0, OAuth 2.0 (with PKCE and
  refresh), Hawk, AWS SigV4, NTLM, Akamai EdgeGrid and ASAP.
- Searchable history and code snippets in 18 languages and tools.

### Collections and environments
- Workspaces as folders of pretty-printed JSON, one file per request, ready for Git.
- Import from Postman (collections v2.0/v2.1, environments, globals), cURL, OpenAPI 3, Swagger 2,
  Insomnia, HAR, Hoppscotch, Thunder Client, WSDL and Smithy; export to Postman v2.1 and OpenAPI.
- Five variable scopes, initial and current values, secret variables, 118 dynamic variables and
  `{{vault:name}}`, backed by the Keychain or by HashiCorp Vault, AWS Secrets Manager, Azure Key
  Vault and 1Password.
- Secrets and OAuth tokens are kept in the Keychain, never in workspace files, exports or reports.
- Local forks of a collection with three-way pull and merge.

### Scripts and tests
- Postman-compatible `pm` API on an embedded JavaScript engine (boa) with Chai, lodash, moment,
  crypto-js and tv4; pre-request and post-response scripts on collections, folders and requests.
- Scripts for gRPC, WebSocket, Socket.IO, MQTT, MCP and AI requests.
- npm and JSR packages through `pm.require`, cached and locked by SHA-256, a local Package Library,
  `Intl` and Web Streams.
- Scripts run in a separate worker process with time and memory limits.

### Runner and CLI
- Collection Runner with environments, iterations, delays, CSV/JSON data, `setNextRequest`,
  custom order, failure simulation and JSON/JUnit/HTML export; every protocol can run in it.
- Performance tests with virtual users, ramp-up and latency percentiles.
- `merpati` CLI: `run` with newman-compatible options, `schedule`, `mock`, `import`, `export`,
  `convert`, `validate`, `codegen`, `perf`, `docs`, `flow` and `init`; exit codes 0/1/2 for CI.
- A Dockerfile for the CLI and a GitHub Action that runs a collection in CI.

### Protocols
- GraphQL with subscriptions, introspection and schema autocomplete.
- gRPC from `.proto` files or server reflection, all four call types, TLS and mTLS.
- WebSocket, Socket.IO v2–v4 and MQTT 3.1.1/5 with saved messages and virtualized message logs.
- SOAP requests from WSDL.
- MCP clients over stdio, Streamable HTTP and SSE, with OAuth, sampling and elicitation.
- AI requests to OpenAI, Anthropic, Gemini and OpenAI-compatible servers, with streaming and MCP
  tools.

### Flows
- A visual canvas with Start, Request, Condition, Loop, Parallel, Join, Script, AI, Delay, Set
  Variable, Template, Output and Log blocks; parallel branches, streaming AI output, validation.
- Flows are saved in `flows/` and run from the app or with `merpati flow run`.

### API design
- Spec Hub for OpenAPI, Swagger, AsyncAPI, Protobuf, GraphQL SDL and Smithy with live validation
  and a live documentation preview.
- Generate or update a collection from an OpenAPI or Smithy spec; export a collection as OpenAPI.
- Documentation as Markdown or a static HTML site, from the app or `merpati docs`.

### Mock and capture
- Mock server from examples with Postman-style matching, templating, network and failure
  simulation.
- Local webhook listener with replay.
- Capture proxy for HTTP and HTTPS with a local CA and HAR export.

### App
- Design system: Inter, JetBrains Mono and Phosphor icons, light and dark themes, browser-like
  tabs, command palette and quick search; tabs are restored after a restart or crash.
- Updates: on start Merpati checks GitHub for a new release (it can be turned off in Settings ›
  General › Updates); on macOS **Install and restart** downloads, verifies and installs it;
  elsewhere the archive is downloaded and checked against the release checksums.
- One-line installer for macOS (`scripts/install.sh`), a universal DMG, an app archive, a
  Homebrew cask, a CLI tarball, and Linux and Windows archives.
- English and Indonesian interface.
- Accessibility: keyboard navigation and screen-reader names for every control (AccessKit).
