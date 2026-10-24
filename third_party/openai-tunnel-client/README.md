# OpenAI Secure MCP Tunnel runtime (embedded in server.exe)

`server.exe` embeds OpenAI's tunnel runtime so a deployment needs no separate
tunnel-client executable. This directory holds the unmodified release archive;
`native_host/build.rs` verifies its SHA-256 at compile time and extracts
`tunnel-client-runtime.exe`, `LICENSE` and `NOTICE` into the build output, so
the build never downloads anything.

| | |
| --- | --- |
| Upstream | https://github.com/openai/tunnel-client |
| Release | v0.0.15 |
| Archive | `tunnel-client-runtime-v0.0.15-windows-amd64.zip` |
| Download URL | https://github.com/openai/tunnel-client/releases/download/v0.0.15/tunnel-client-runtime-v0.0.15-windows-amd64.zip |
| SHA-256 | `aa5ddb14dddd602fa59f3e6f4401aa8a79a218e341466226b7434127dff65dbc` |
| License | Apache-2.0 (LICENSE and NOTICE are inside the archive) |

To upgrade: replace the archive, update the version and hash here and in
`native_host/build.rs`, and update `OPENAI_TUNNEL_RUNTIME_VERSION` in
`native_host/src/bin/server.rs`.
