# AI client connectivity

The native FalkorDB host exposes one secured remote MCP endpoint:

```text
https://<host>/mcp
```

It uses stateless Streamable HTTP, MCP tool metadata/annotations, protected
resource discovery, OAuth authorization-server discovery, PKCE S256,
refresh tokens, and scoped Bearer tokens.

## Supported authentication patterns

### Interactive OAuth

The server currently recognizes these published MCP client identities:

- ChatGPT: `https://chatgpt.com/oauth/client.json` and ChatGPT connector
  client-document variants.
- Claude Code: `https://claude.ai/oauth/claude-code-client-metadata`.

Claude Code uses a loopback callback on an ephemeral localhost port. The server
accepts the RFC 8252 native-app pattern for `localhost`, `127.0.0.1`, and
`::1`, but only for the known Claude Code client identity.

Unknown client metadata URLs are intentionally rejected rather than trusted
without verification.

### Scoped Bearer tokens

Any MCP client that can send an Authorization header can connect without the
interactive OAuth flow:

```text
Authorization: Bearer <token>
```

Use:

- `FALKORDB_API_READ_TOKEN` for read-only access.
- `FALKORDB_API_TOKEN` for read/write/admin access.

Prefer the read-only token for analysis agents that do not need to mutate the
database.

## ChatGPT

ChatGPT custom MCP apps can point at:

```text
https://<host>/mcp
```

and complete the built-in OAuth flow. The server publishes the discovery
metadata ChatGPT needs and returns standard MCP tool annotations including
`readOnlyHint`, `destructiveHint`, and `idempotentHint`.

For OpenAI API integrations, use the remote MCP tool with this server URL and
either an OAuth access token or another authorized Bearer token.

If the database is not publicly reachable, keep it private and use the
supported secure MCP tunneling/private-network path rather than exposing the
database listener.

## Claude Code

Interactive OAuth:

```text
claude mcp add --transport http falkordb https://<host>/mcp
```

Then run:

```text
/mcp
```

and complete the browser authorization flow.

Direct Bearer-token configuration is also supported:

```text
claude mcp add --transport http falkordb https://<host>/mcp \
  --header "Authorization: Bearer <token>"
```

## Claude API

Anthropic's remote MCP connector can use the same URL and an authorization
token. The server exposes tools over standard remote MCP, so no FalkorDB-
specific Claude adapter is required.

## Other MCP clients

Clients that support remote Streamable HTTP can connect to `/mcp` using a
scoped Bearer token even if they do not have a client identity explicitly
allowlisted for interactive OAuth.

This is the compatibility baseline for tools such as IDE agents, command-line
agents, orchestration frameworks, and custom model runtimes.

## Tool scopes

The MCP server separates access into:

- `graph:read`
- `graph:write`
- `graph:admin`

Examples:

- read: list graphs, database statistics, read-only Cypher, graph export.
- write: create graphs, write Cypher, batch graph queries.
- admin: graph deletion/copy/checkpoint/flush, whole-database restore, native
  RDB import, and server-local bulk file import.

The tool catalog also marks read-only/destructive/idempotent behavior so clients
can apply their own approval and policy controls.

## Security guidance

- Keep the raw RESP/admin listener private when remote AI clients only need MCP.
- Use HTTPS for remote MCP.
- Give analysis-only clients `FALKORDB_API_READ_TOKEN`.
- Do not reuse the admin database password as a long-lived client token.
- Prefer interactive OAuth when the client identity is explicitly supported.
- Prefer separate credentials for human Browser viewing and AI-agent MCP access.
