# HTTP transport and Docker — the detailed guide

> Fork-local supplement (imonlinux/donsetch). The upstream README was
> rewritten in v3.4.2 and now covers the HTTP transport and Docker in a
> compact form. This document preserves the full interface guide from
> PRs #69 and #70 (merged upstream in v3.4.1), updated to match the
> final merged state — including the review fixes (own `build:` block
> and `restart: unless-stopped` on the compose `http` profile) and the
> v3.4.1 loud-failure guard. When upstream and this file disagree,
> upstream wins.

## HTTP transport (remote clients and debugging)

DonSeTch also speaks MCP over HTTP (the streamable-HTTP transport:
JSON-RPC via POST, plus the GET SSE stream and DELETE session end that
strict clients expect). Both transports dispatch through the same
handler, so tools behave identically. HTTP is opt-in:

```bash
# Flags
donsetch mcp --http --host 0.0.0.0 --port 8765

# Env vars (identical effect; flags win over env when both are set)
DONSETCH_TRANSPORT=http DONSETCH_HTTP_HOST=0.0.0.0 donsetch mcp
```

Run `donsetch help mcp` for the transport flags and env vars as the
binary ships them. (Not `donsetch mcp --help` — that starts the
server.)

MCP clients connect to `http://localhost:8765/mcp`:

```json
{
  "mcpServers": {
    "donsetch": { "url": "http://localhost:8765/mcp" }
  }
}
```

**Testing with curl:**

```bash
# Health check (always unauthenticated, for probes)
curl http://localhost:8765/health

# Test the MCP endpoint
curl -X POST http://localhost:8765/mcp \
  -H "Content-Type: application/json" \
  -H "Accept: application/json, text/event-stream" \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/list"}'
```

**Sessions and cancellation.** The `initialize` response carries an
`Mcp-Session-Id` header. Echo it back on subsequent requests to get a
dedicated cancellation registry: posting `notifications/cancelled`
with the session header while a tool call is in flight aborts it (same
semantics as stdio). Session-less clients share one default registry —
cancellation still works, but request ids share a namespace. Unknown
or expired session ids get a 404. Sessions idle for 30 minutes are
dropped; `DELETE /mcp` with the session header ends one immediately.

```bash
# Cancel request 42 during a long call (with a session header)
curl -X POST http://localhost:8765/mcp \
  -H "Content-Type: application/json" \
  -H "Mcp-Session-Id: <from initialize>" \
  -d '{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":42}}'
```

**Environment variables** (HTTP mode):

| Variable | Default | Effect |
|---|---|---|
| `DONSETCH_TRANSPORT` | `stdio` | `stdio` or `http` — same as the `--http` flag |
| `DONSETCH_HTTP_HOST` | `127.0.0.1` | Bind address (use `0.0.0.0` to accept remote clients) |
| `DONSETCH_HTTP_PORT` | `8765` | Listen port — same as `--port` |
| `DONSETCH_HTTP_TOKEN` | unset | When set, `/mcp` requires `Authorization: Bearer <token>`; `/health` stays open |
| `DONSETCH_HTTP_TIMEOUT_SECS` | `300` | Per-request timeout; timed-out calls return a JSON-RPC error |
| `DONSETCH_HTTP_CORS` | off | `1`/`true`/`on` allows cross-origin requests. Off by default: MCP clients are processes, not browsers, and a permissive layer would let any webpage in a local browser read responses from a localhost instance |

**Build requirement.** HTTP is an optional cargo feature. Source
builds need `cargo build --release --features ocr,rerank,http` (or
just `--features http` for the transport alone); a plain `cargo build`
produces a stdio-only binary. Of the prebuilt binaries, linux-x64,
macOS-arm64, and Windows-x64 ship with `http`; the linux-arm64 and
macOS-x64 prebuilts are core-only. Since v3.4.1, requesting HTTP on a
core-only build fails loudly instead of silently serving stdio:

```
mcp: HTTP transport requested (--http / DONSETCH_TRANSPORT=http), but this
binary was built without the `http` cargo feature. Rebuild with
--features http, or use a prebuilt binary that includes it.
```

On SIGTERM/SIGINT the server stops accepting new requests, drains
in-flight ones, and shuts the daemon down (no orphan Chrome
processes).

## Docker

For isolated deployment, a consistent runtime environment, and easy
updates:

```bash
git clone https://github.com/dondai44423/donsetch.git
cd donsetch
docker compose build
```

**Optional build choice — Chromium.** The default image excludes
Chromium; tier 2 browser escalation only activates when a browser is
configured. To bake Chromium in (~+350MB) for tier 2 bot-wall bypass:

```bash
docker build --build-arg INSTALL_CHROME=true -t donsetch-mcp .
```

Then point the server at it at runtime with
`-e DONGHOST_CHROME=/usr/bin/chromium` (the bundled compose file ships
this as a commented entry).

### stdio (default transport)

MCP clients launch the server as a subprocess:

```bash
docker run -i --rm --init donsetch-mcp donsetch mcp --supervised
```

stdio MCP client config (Docker flavor):

```json
{
  "mcpServers": {
    "donsetch": {
      "command": "docker",
      "args": ["run", "-i", "--rm", "--init", "donsetch-mcp", "donsetch", "mcp", "--supervised"]
    }
  }
}
```

OpenCode uses a stricter MCP schema — a `type` discriminator, a single
`command` array, and an explicit `enabled` — in
`~/.config/opencode/opencode.json`:

```json
{
  "mcp": {
    "donsetch": {
      "type": "local",
      "command": ["docker", "run", "-i", "--rm", "--init", "donsetch-mcp", "donsetch", "mcp", "--supervised"],
      "enabled": true,
      "timeout": 120000
    }
  }
}
```

`timeout` is optional but recommended: OpenCode defaults MCP requests
to 5 seconds, and tier 1 fetches regularly run longer.

Or via the bundled compose service (adds the persistent cache volume
and resource limits):

```bash
docker compose run --rm donsetch
```

### HTTP transport

```bash
docker compose --profile http up -d donsetch-http
curl http://localhost:8765/health
```

- Serves MCP at `http://localhost:8765/mcp`; MCP clients connect to
  that URL. The stdio service is unchanged.
- The service carries its own `build:` block, so this works on a
  machine that hasn't built the image yet — compose builds it instead
  of trying to pull `donsetch-mcp` from Docker Hub.
- The port is published on `127.0.0.1` by default so an unset
  `DONSETCH_HTTP_TOKEN` never exposes unauthenticated MCP to the LAN.
  To serve remote clients, change the publish to `8765:8765` **and**
  set `DONSETCH_HTTP_TOKEN`.
- `restart: unless-stopped` gives crash recovery: release builds are
  `panic = "abort"` and the HTTP transport has no in-process supervisor
  (that's stdio `--supervised`), so one panicking request aborts the
  process and Docker restarts the container. A manual
  `docker compose stop` stays stopped.
- Listener-based healthcheck (`bash /dev/tcp` — the slim runtime image
  has no curl/wget) flips the container to `healthy`.

Plain `docker run` HTTP mode (from the Dockerfile's CMD comment):

```bash
docker run -d -p 127.0.0.1:8765:8765 \
  -e DONSETCH_TRANSPORT=http -e DONSETCH_HTTP_HOST=0.0.0.0 \
  [-e DONSETCH_HTTP_TOKEN=<token>] donsetch-mcp
```

### Docker Compose options

The bundled `docker-compose.yml`:

- A cache volume persisting fetch/search state across restarts.
- A 2GB memory ceiling (OCR + reranking peak around 1–2GB under heavy
  crawls) and an init process to reap zombies.
- A 45-second stop grace period so in-flight tier-2 fetches finish on
  `docker compose stop`.
- An opt-in `http` profile running the same image as a long-lived
  HTTP server with `restart: unless-stopped` (see above).

## Running as a systemd quadlet (Podman)

The same image runs under Podman as a native systemd service via a
quadlet — no daemon, no compose. This is the natural fit for the
HTTP transport's long-lived deployment: systemd is the supervisor
(crash recovery, boot startup, `journalctl` logs), replacing the
compose `restart: unless-stopped` policy.

stdio clients should keep launching the container per connection
(`podman run -i --rm --init ... donsetch mcp --supervised`, same
shape as the Docker stdio config above) — a per-connection stdio
lifecycle does not fit a systemd service; the quadlet pattern is for
the HTTP transport.

Build the image first (quadlet does not build):

```bash
git clone https://github.com/dondai44423/donsetch.git
cd donsetch
podman build -t localhost/donsetch-mcp .
```

### Rootless (recommended)

`~/.config/containers/systemd/donsetch-cache.volume`:

```ini
[Volume]
```

`~/.config/containers/systemd/donsetch-http.container`:

```ini
[Unit]
Description=DonSeTch MCP server (HTTP transport)
Wants=network-online.target
After=network-online.target

[Container]
Image=localhost/donsetch-mcp:latest
ContainerName=donsetch-http
Environment=DONSETCH_TRANSPORT=http
# Listen on all interfaces inside the container; PublishPort decides
# who can actually reach it.
Environment=DONSETCH_HTTP_HOST=0.0.0.0
# Require `Authorization: Bearer <token>` on /mcp — set before
# publishing the port beyond localhost. /health stays open.
#Environment=DONSETCH_HTTP_TOKEN=change-me
PublishPort=127.0.0.1:8765:8765
Volume=donsetch-cache.volume:/home/donsetch/.cache/donsetch
# Same ceiling as the compose service (OCR + reranking peak 1–2GB).
#Memory=2G

[Service]
# Crash recovery: release builds are panic=abort and the HTTP
# transport has no in-process supervisor — systemd restarts the
# container, like compose's restart: unless-stopped. A manual
# `systemctl --user stop` stays stopped.
Restart=on-failure
RestartSec=3

[Install]
WantedBy=default.target
```

Enable and start:

```bash
systemctl --user daemon-reload
systemctl --user start donsetch-http.service
curl http://localhost:8765/health   # expect 200

# logs / status
journalctl --user -u donsetch-http -f
systemctl --user status donsetch-http
```

To serve remote clients, change the publish to
`PublishPort=8765:8765` **and** set `DONSETCH_HTTP_TOKEN`, then
`systemctl --user restart donsetch-http`.

For the service to run at boot without an open login session
(rootless), enable lingering for the user:

```bash
loginctl enable-linger $USER
```

### Rootful

Same two files under `/etc/containers/systemd/`, with two changes:
`WantedBy=multi-user.target` in the `[Install]` section, and plain
`systemctl` instead of `systemctl --user`:

```bash
sudo cp donsetch-cache.volume donsetch-http.container /etc/containers/systemd/
sudo systemctl daemon-reload
sudo systemctl start donsetch-http
```

### Quadlet notes

- Quadlet ships in Podman 4.4+; anything newer works unchanged.
- The `.container` filename becomes the service name
  (`donsetch-http.container` → `donsetch-http.service`); drop
  `ContainerName=` if you prefer the service name to be the only
  name.
- Referencing `donsetch-cache.volume` in `Volume=` makes systemd
  create the named volume through its own quadlet — the same
  persistence as the compose cache volume.
- To serve remote clients on a headless box, remember
  `enable-linger` (rootless) or the rootful variant; without it the
  service only runs while a session is open.
- The image's `HEALTHCHECK` is not consumed by systemd — probe
  `/health` yourself (or wire a watchdog) if you want
  health-gated supervision.

### Architecture notes

Multi-stage build (`rust:slim` → `debian:trixie-slim`, kept on the
same glibc generation — ort-sys's prebuilt ONNX Runtime archives
reference glibc 2.38+ symbols, and a binary built against a newer
glibc will not run on an older one), built with
`--features ocr,rerank,http` (the same feature set as the linux-x64,
macOS-arm64, and Windows-x64 release binaries), PDFium acquired at
build time by the repo's own `build.rs` (sha256-verified), Go
installed per-target-arch for BoringSSL's build system (amd64 and
arm64). Single binary in a minimal runtime image — no Python, no
Playwright.
