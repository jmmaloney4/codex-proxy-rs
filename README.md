# codex-proxy-rs

Rust port of [`jmmaloney4/codex-proxy`](https://github.com/jmmaloney4/codex-proxy): an OpenAI-compatible reverse proxy in front of the ChatGPT Codex backend. It rewrites Chat Completions requests into Codex Responses-API form, relays the SSE stream back as OpenAI chunks, and fixes the Go implementation's production failure modes by design (guaranteed `data: [DONE]` termination, idle keepalives, no token-size cap — garden#796/#803, ADR 002/003).

## Running

```sh
ADMIN_API_KEY=<proxy key> ANTHROPIC_API_KEY=<chatgpt token> CLAUDE_USER_ID=<account id> \
  codex-proxy --creds-store env
```

- Listens on `PORT` (default 9879).
- Clients authenticate to the proxy with `ADMIN_API_KEY` (as `Authorization: Bearer <key>` or `X-API-Key: <key>`) on the data-plane and `/admin/*` routes.
- `ENV=production` switches logs to JSON; anything else is pretty console output.
- `nix build` produces the `codex-proxy` binary; `nix develop` gives the dev toolchain.

### Routes

| Route | Auth | Behavior |
|---|---|---|
| `POST /v1/chat/completions` | admin key | OpenAI Chat Completions → Codex; streaming (`"stream": true`) or buffered |
| `POST /v1/responses` | admin key | Responses-API rewrite + SSE pass-through |
| `GET /v1/models` | open | Embedded model metadata (dumped from Go `supportedModels()`) |
| `GET /health` | open | `{"status": "ok"}` |
| `POST /admin/credentials` | admin key | Push OAuth tokens (fs store) |
| `GET /admin/credentials/status` | admin key | Token expiry status |

## Not planned

- **`gpt-5.3-codex-spark` / WebSocket upstream** — dropped by decision; all models route over HTTP and spark is filtered from `/v1/models`.
- **macOS keychain credential store, legacy-path migration, `auto` mode** — this port targets Kubernetes; credential modes are `env` (static token) and `fs` (auth.json on a writable volume with in-process OAuth refresh, Phase 5).
- **Cloudflare Workers / WASM variant.**

## Provenance and decisions

The Go implementation is the behavioral source of truth; parity divergences are deliberate and recorded in `docs/internal/decisions/` (ADR 001–004, including the full divergence register in ADR 004). Tests ported from Go (`transform_test.go`, `transform_sse_test.go`, `transform_responses_test.go`) act as the correctness oracle, extended with relay/server integration tests.
