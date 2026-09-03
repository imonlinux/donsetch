# DonSeTch

> Web fetch, search, and crawl for AI agents. Zero API keys. Chrome-true TLS. Built in Rust.

[![Release](https://img.shields.io/github/v/release/dondai44423/donsetch?color=00d4aa&style=flat-square)](https://github.com/dondai44423/donsetch/releases)
[![CI](https://img.shields.io/github/actions/workflow/status/dondai44423/donsetch/ci.yml?label=CI&style=flat-square)](https://github.com/dondai44423/donsetch/actions/workflows/ci.yml)
[![License: AGPL-3.0](https://img.shields.io/badge/License-AGPL--3.0-00d4aa?style=flat-square)](https://github.com/dondai44423/donsetch/blob/master/LICENSE)

DonSeTch gives AI agents web research from a single local process : fetch any URL, search across 10+ engines, and crawl multi-page docs. Chrome-true TLS fingerprinting, bot-wall bypass, PDF extraction with OCR, semantic reranking. Zero API keys.

## Install

```bash
npm install -g donsetch
```

Downloads the prebuilt binary for your platform from [GitHub Releases](https://github.com/dondai44423/donsetch/releases) with SHA256 verification.

| Platform | Binary |
|---|---|
| Linux x86_64 | `donsetch-linux-x64.tar.gz` |
| Linux arm64 | `donsetch-linux-arm64.tar.gz` |
| macOS arm64 | `donsetch-darwin-arm64.tar.gz` |
| Windows x86_64 | `donsetch-win32-x64.tar.gz` |

## Two ways to use it

### MCP Server (for AI agents)

```json
{
  "mcpServers": {
    "donsetch": { "command": "donsetch", "args": ["mcp"] }
  }
}
```

Or with `npx` (no global install):

```json
{
  "mcpServers": {
    "donsetch": { "command": "npx", "args": ["donsetch", "mcp"] }
  }
}
```

Three tools: `web_fetch`, `web_search`, `web_crawl`.

### CLI (for humans and scripts)

```bash
donsetch fetch https://example.com
donsetch search "rust async patterns"
donsetch crawl https://docs.example.com --topic "api reference"
donsetch keys add tinyfish sk-tinyfish-...
donsetch doctor
donsetch update
```

## License

AGPL-3.0 : Copyright (c) 2026 Bishesh Bhandari

Full documentation: [github.com/dondai44423/donsetch](https://github.com/dondai44423/donsetch)
