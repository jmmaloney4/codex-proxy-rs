---
id: ADR-007
title: Router Mode — Conversation→Account Affinity
status: accepted
date: 2026-06-17
---

# ADR 007: Router Mode — Conversation→Account Affinity

*Date:* 2026-06-17
*Status:* accepted

## Context

ADR 006 designed conversation-scoped state. Its Phase-1 foundation (conversation
identity + reasoning capture) shipped and was deployed; prod tracing then
**confirmed the affinity gap empirically**: a single agent conversation (stable
head-hash key, `message_count` climbing 90→102) is load-balanced by LiteLLM
across all three codex accounts within seconds. Because the Codex
`encrypted_content` reasoning blob is account×model-scoped, reasoning can never
be replayed while a conversation bounces across accounts.

This ADR implements **PR2 — account affinity** (ADR 006 §3a, the front-router
topology). Reasoning replay itself is deferred to a later PR; this ADR only
guarantees that all turns of a conversation reach the *same* account.

## Decision

### 1. A `router` mode of this crate, not a separate service

Add `CODEX_PROXY_MODE = backend | router` (`src/config.rs`, mirroring the
`CredsStore` enum), threaded into `AppState` and selecting the route table in
`server::router`. `backend` (default) is byte-for-byte unchanged. `router` fronts
the existing single-account pods; LiteLLM points at one router endpoint instead
of the N codex deployments. One binary/image, two modes — reuses all of the
crate's HTTP/relay machinery and keeps per-account credential isolation (ADR 004)
on the backend pods.

### 2. Plain byte reverse-proxy (no transform)

In router mode the backend pods already emit **final OpenAI-format** responses
(they did the codex→OpenAI translation), so the router does *not* re-transform or
re-run the SSE rewrite. It resolves the account, forwards the request body
verbatim to the chosen pod (`router::proxy_to_pod`), and streams the response
straight back (`server::stream::proxy_response`: copy status + sanitized headers,
pipe the body). The pod's own keepalives/`[DONE]` flow through.

### 3. Account pool + selection

`CODEX_PROXY_ACCOUNTS` is a `slug=url` comma list (`src/router.rs:AccountPool`).
New conversations get a round-robin pick across **healthy** accounts; an account
that returns `429`/`5xx`/connection-error is put in a short in-process cooldown
(60s) and skipped. Selection always returns *something* (falls back through
cooldown, then to any account) so a request is never dropped for lack of a pick.

### 4. Affinity store: dedicated single-pod Redis, best-effort

`src/affinity.rs` defines an `AffinityStore` trait (`get`/`put`/`clear`) with a
`RedisAffinityStore` (key `conv:{key}` → `{slug, model}`, `SET … EX <ttl>`,
default 1-day TTL) and an `InMemoryAffinityStore` (tests / single-replica
fallback). Production uses a **dedicated single-pod Redis** (garden side; modeled
on `cavinsresearch/zeus deploy/platform/redis` — redis:8-alpine, `--requirepass`,
RDB `--save 60 1`, RWO ceph PVC). Single-pod is intentional: affinity is
best-effort, so HA Redis is not warranted yet (revisit if PR3 reasoning
persistence raises the cost of state loss).

Every store op is **best-effort and infallible by contract** — it logs and
swallows backend errors, and a miss is indistinguishable from "unpinned". A Redis
outage (or no `CODEX_PROXY_REDIS_URL`) degrades to stateless round-robin, never a
failed request (ADR 006 §5c).

### 5. Flow + re-pin

`resolve_conversation_key` (ADR 006 §2) → look up the pin → use it if the account
is known and healthy, else pick fresh and pin. Proxy; on `429`/`5xx`/send-error
from the chosen account, cool it down, clear the pin, pick a *different* healthy
account, and retry once. `model` is recorded on the pin for PR3 (the blob is
invalid across a model change) but does not affect routing here. Tracing emits
`conversation_key_fp` (fingerprint, not the raw key), `account_slug`, and
`account_source` (`pinned`/`new`/`repinned`/`fallback`/…).

### 6. Auth

LiteLLM → router → pod all share the one `ADMIN_API_KEY`. The router validates
inbound with the existing `admin_auth` gate and forwards `Bearer <ADMIN_API_KEY>`
to the pod. The router has no credential store; `/admin/credentials*` routes are
omitted in router mode.

## Non-goals

- **Reasoning replay** (persist `ReasoningItem`s, splice into the Responses
  `input`, account×model invalidation) — a later PR. This ADR only pins accounts.
- **Redis HA** — single-pod is deliberate (see §4).
- **Per-tenant inbound auth** — the shared admin key is sufficient; the router is
  ClusterIP-internal, reached only by LiteLLM.

## Risks

- **Loss of LiteLLM's cross-account load-balancing.** The router now owns
  selection + failover for codex; the round-robin + cooldown must be sound.
  Re-pin-on-429 is a hot path (2/3 accounts were usage-limited at spike time).
- **Single-pod Redis is a soft dependency.** Its loss drops affinity (degrades to
  stateless), not availability — but reasoning persistence (PR3) will care more,
  hence the revisit note.
- **Head-hash identity imperfection** (ADR 006 §2): no `x-conversation-id` →
  head-hash, which can't distinguish two conversations with an identical head.
  Acceptable for affinity (worst case: two conversations share an account).

## Related

- ADR 006 — conversation-scoped state design (§2 identity, §3a router, §4 store, §5c fallback)
- ADR 004 — per-account credentials / the backend pods this fronts
- `cavinsresearch/zeus deploy/platform/redis/index.ts` — the single-pod Redis template
- garden `deploy/services/litellm/{proxy-plan,index,codex-proxy}.ts` — PR2b deploys the router + Redis and collapses the codex pool to one endpoint
