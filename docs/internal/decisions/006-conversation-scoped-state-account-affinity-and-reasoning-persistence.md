---
id: ADR-006
title: Conversation-Scoped State — Account Affinity and Reasoning Persistence
status: proposed
date: 2026-06-16
---

# ADR 006: Conversation-Scoped State — Account Affinity and Reasoning Persistence

*Date:* 2026-06-16
*Status:* proposed

## Context

codex-proxy translates the ChatGPT/Codex **Responses API** into an
OpenAI-compatible **Chat Completions** surface for the garden LiteLLM gateway
(ADR 005 §Context for the topology). Two properties of the current
implementation are entangled and the subject of this ADR.

**1. The proxy is stateless per request.** Each turn, LiteLLM resends the whole
transcript; the proxy rebuilds a fresh Responses call (`store: false`,
`src/request.rs:515`) and asks the backend to compute everything from the
visible messages. The Codex backend's *reasoning* — its chain of thought — is
requested (`include: ["reasoning.encrypted_content"]`, `src/request.rs:481`,
`:584`) but never re-supplied across turns, so the model re-derives it from
scratch every turn.

**2. The encrypted reasoning never escapes the chat path.** The Responses stream
carries reasoning as two parts: a human-readable **summary** and an opaque,
account-scoped **`encrypted_content`** blob. The SSE transform forwards summary
deltas as `delta.reasoning_content` (`src/transform/mod.rs:236`) but explicitly
drops `encrypted_content` (`src/transform/upstream.rs:133-141`). (The
non-streaming buffered path additionally drops even the summary — that is the
narrower bug tracked in #10 and is **not** this ADR.) The native `/v1/responses`
passthrough (`RelayMode::PassThrough`) does relay it verbatim, but LiteLLM does
not use that path.

**Why these are one problem, not two.** Giving the model its prior reasoning
back (with `store: false`) means echoing the `encrypted_content` blob into the
next request's `input`. That blob is **encrypted with a key scoped to the OpenAI
account/org that produced it** — it cannot be replayed against a different
account. So reasoning persistence is only valid if every turn of a conversation
hits the *same account*. Therefore **account affinity is a prerequisite for
reasoning persistence**, and both need the same two primitives:

1. a **stable conversation key** that survives the stateless Chat Completions
   boundary, and
2. a **shared, conversation-keyed state record** that all proxy replicas agree
   on.

**The topology makes affinity nonexistent today, not merely weak.** Per
`garden/deploy/services/litellm/codex-proxy.ts`, garden deploys **one
codex-proxy pod per OpenAI account**; each pod loads a single `auth.json`
(`src/credentials/fs.rs`) and `get_credentials()` returns that one account. The
proxy has no account pool. **Account selection is LiteLLM's job** — it
load-balances per-request across the N codex *deployments* (the "codex pool";
see `proxy-plan.ts`), with no conversation stickiness and no mechanism to add
it across deployments. So "move affinity into the proxy" is not a refactor of
existing affinity; it is introducing affinity for the first time, in a layer
that owns account selection.

This ADR records the **design** for conversation-scoped state. It is a planning
document: the concrete implementation lands in follow-up PRs. The gating spike
(formerly Open Questions Q1) has now run — see Spike findings below.

## Spike findings (2026-06-17)

A live spike against `chatgpt.com/backend-api/codex/responses` (read-only use of
the deployed pods' existing access tokens; **no** token refresh triggered, so
the rotating refresh tokens were untouched) resolved the central fork and
confirmed the affinity premise:

- **`store: true` is rejected outright** — `HTTP 400 {"detail":"Store must be
  set to false"}`. Because `previous_response_id` requires server-side storage
  (`store: true`), **Family 5a is impossible on this backend.** Confirmed, not
  inferred.
- **Stateless `encrypted_content` echo (5b) works end-to-end.** A `store:false`
  turn returned a `reasoning` item carrying a ~970-char `encrypted_content`
  blob; echoing that item verbatim into a follow-up request's `input` was
  accepted (HTTP 200, no `invalid_encrypted_content`) and the model recalled a
  value it had committed to **only in its hidden reasoning** on the prior turn —
  a control run without the echoed item could not. So the backend genuinely
  carries reasoning across *separate* requests.
- **Structural gotcha:** under `store:false` the `response.completed.output`
  array comes back **empty** — output items (including the reasoning item)
  arrive only via individual `response.output_item.done` events. The proxy must
  accumulate reasoning items from those streamed events to persist them; there
  is no final aggregated payload to read. (Today they are dropped at
  `src/transform/upstream.rs:133-141`.)
- **Affinity premise holds; live cross-account test was blocked.** 2 of 3 codex
  accounts were `429 usage_limit_reached` at spike time, so the cross-account
  replay could not be exercised live. Documented evidence stands: the blob is
  scoped to **account/org *and* model** — a mid-conversation model switch yields
  `invalid_encrypted_content` (openai/codex#17541) and an account switch yields
  an `organization_id` mismatch. The pin must therefore be **(account × model)**,
  and any model change must invalidate the reasoning record.

**Net:** the §5 fork collapses — 5a is out, **5b is the only mechanism and is
proven buildable**; the ZDR/retention concern (formerly Q4) is moot because 5b
is `store:false` with no server-side retention.

## Decision

### 1. One feature: a conversation-keyed state record

Introduce a single per-conversation state record holding **both** the pin —
`(account_id, model)`, not account alone (the spike confirmed the blob is
model-scoped too) — **and** the ordered reasoning blobs (§5). Affinity and
reasoning persistence are implemented together because they share identity +
storage and because reasoning replay is invalid without affinity. A change of
model on an existing conversation **invalidates** the reasoning blobs (they
cannot be decrypted by a different model) even though the account pin may hold.
Shipping order is layered (§7), but they are not independent features.

### 2. Conversation identity (the crux) — layered resolution

Chat Completions carries no conversation ID. Resolve a stable key by falling
through, in order:

1. **Explicit key** — an `x-conversation-id` header or the `user` field, if the
   client/LiteLLM supplies one. Most robust; survives history compaction.
2. **Tool-call-ID token** — embed an opaque state token inside the
   `tool_call_id`s the proxy emits (`src/transform/mod.rs` function-call path).
   Clients reliably echo `tool_call_id` back in the following `role:tool`
   message, giving **rock-solid continuity within an agentic tool loop** — which
   is exactly where Codex reasoning persistence pays off — with zero client or
   LiteLLM coordination. Spans tool-call turns only.
3. **Head-hash** — hash the stable head of the transcript (system + first user
   message), reusing the machinery already behind `derive_prompt_cache_key`
   (`src/request.rs`). Zero coordination; breaks when an agent summarizes or
   rewrites the conversation head.

A resolved key is **advisory**: a miss at any layer degrades to today's
stateless behavior (§5), never an error.

### 3. State ownership / topology

Affinity requires a layer that (a) is the single endpoint LiteLLM addresses for
codex, and (b) can reach **all** accounts and a **shared** store. Two shapes:

- **3a. Front router (recommended).** A new mode/binary of *this* codebase sits
  in front of the existing single-account pods. It owns the conversation→record
  map (§4), picks an account for *new* conversations, forwards to the matching
  account's pod, and captures reasoning on the response path. The existing pods
  stay single-account and nearly unchanged. LiteLLM sees **one** codex endpoint.
  Smallest blast radius; keeps the per-account credential isolation ADR 004
  established.
- **3b. Fat multi-account proxy.** Teach codex-proxy to load all N `auth.json`s
  and select internally, collapsing to one deployment. One fewer service, but
  pulls account-pool management, internal load-balancing, failover, and
  per-account credential-refresh fan-in into one process — a large change to the
  credentials layer (`src/credentials/`).

Either way, **LiteLLM stops load-balancing across codex accounts**, so the new
layer must reimplement weighted selection + health/failover for *new*
conversations. Recommendation: **3a**, implemented as a routing mode of this
crate so affinity lives "in the proxy" without dissolving single-account
isolation.

### 4. State store

A **shared external KV (Redis)** keyed by conversation id, values:
`{ account_id, model, reasoning_blobs[], updated_at }`, with a **short TTL** (hours —
conversations are ephemeral; TTL is the eviction story). In-proxy memory is
rejected: it fails the moment there is more than one replica or the router
restarts. The existing garden Cloud SQL is a fallback if a new Redis is
unwanted, but TTL + churn make Redis the better fit.

### 5. Persistence mechanism — stateless `encrypted_content` echo (only viable path)

The spike settled this. `store: true` / `previous_response_id` is rejected by
the backend (`400 "Store must be set to false"`), so the server-side-state
option is **removed** — it cannot be built. The only mechanism is stateless
`encrypted_content` echo, which the spike proved works end-to-end:

- Keep `store: false` and `include: ["reasoning.encrypted_content"]` (already
  set at `src/request.rs:481,584`).
- **Accumulate reasoning items from the streamed `response.output_item.done`
  events** — *not* from `response.completed`, which is empty under `store:false`
  (Spike findings) — and persist the ordered blobs per conversation. This is the
  response-side change; today they are dropped at
  `src/transform/upstream.rs:133-141`.
- On the next turn, **splice the stored reasoning items back into the
  reconstructed Responses `input`**, aligned to each assistant/tool turn and
  echoed verbatim, exactly as the official Codex CLI does.

Alignment is the brittle part: if the client trimmed or reordered history, turn
indices drift, so the proxy must detect misalignment and fall back (§5c). The
blobs are valid only for the pinned `(account, model)` — a model change
invalidates them (§1).

### 5c. Best-effort with stateless fallback (safety principle)

Persisted reasoning and affinity are an **optimization, not correctness**. On
any cache miss, unavailable/failed account, or alignment mismatch, the layer
**drops the reasoning and behaves exactly like today** (fresh stateless
Responses call, fresh account pick). A miss is a slightly more expensive turn,
never a failed request. This is what makes the feature safe to roll out
incrementally and to leave dormant when the store is down.

### 6. Affinity vs. failover tension

A pinned account that is rate-limited or down forces a choice: wait, or break
affinity. Decision: **break affinity and re-pin** (drop the conversation's
reasoning, pick a healthy account, continue) rather than fail or stall —
consistent with §5c. Reasoning loss on failover is acceptable; a stalled agent
loop is not. This is not hypothetical: the spike found **2 of 3 codex accounts
returning `429 usage_limit_reached`**, so re-pin-on-429 is a primary path the
router must handle, not an edge case. (A model change triggers the same
reasoning-drop via §1, while the account pin may survive.)

### 7. Shipping order

1. **Foundation** — identity resolution (§2, start with tool-call-ID + head-hash)
   + shared store (§4) + the router (§3a). Capture `account_id` per
   conversation. **No behavior change yet.**
2. **Affinity** — pin conversations to accounts; point LiteLLM at the single
   router endpoint; router does weighted pick + failover for new conversations.
   Useful on its own (prompt-cache locality, fewer account hops) **without any
   reasoning work**.
3. **Reasoning replay** — §5 (`encrypted_content` echo; 5a is ruled out).
   Accumulate reasoning items from `output_item.done`, persist, splice back.
   Always behind §5c.

## Non-goals (by decision)

- **Not the buffered `reasoning_content` drop (#10).** That is a self-contained
  bug on the existing stateless path and ships independently of this design.
- **No client-echoed encrypted reasoning.** Standard clients strip unknown
  fields and DeepSeek-style APIs reject echoed `reasoning_content`; relying on
  clients to round-trip an account-scoped blob is rejected in favor of
  server-side state (§4).
- **No change to the `/v1/responses` passthrough.** Native Responses clients
  already get full reasoning items; this ADR is about the chat-completions
  façade only.
- **No multi-account credential model in the single-account pods** under the
  recommended 3a — credential isolation per ADR 004 is preserved; the router
  selects *which pod*, not which key within a pod.

## Risks

- **Reasoning-item splicing/alignment is the unavoidable cost (§5).** With 5a
  ruled out (spike), there is no simpler path: the proxy must accumulate
  reasoning items from the stream, persist them, and re-inject them in the right
  positions. Misalignment must fail safe to stateless (§5c).
- **Echoing `encrypted_content` + `reasoning` can hang the backend.** Reports
  associate this combination with the backend entering a "thinking" state that
  times out internally — consistent with the known gpt-5.x pool timeouts.
  Re-injection must be guarded by the relay's keepalive/timeout discipline
  (ADR 002/005) and fail safe to stateless.
- **Loss of LiteLLM load-balancing/failover for codex (§3).** The router must
  reimplement weighted selection, health checks, and usage accounting that
  LiteLLM provides today. With 2/3 accounts usage-limited at spike time,
  re-pin-on-429 is a hot path, not an edge case. Main schedule risk.
- **Identity fragility (§2).** Head-hash breaks on history compaction;
  tool-call-ID only spans tool loops. Without an explicit key, some
  conversations will silently fall back to stateless (acceptable per §5c, but
  caps the hit rate).
- **State store as a new dependency / failure mode.** Redis outage must
  degrade to §5c, never block the request path (mirror ADR 005's
  fire-and-forget export discipline).

## Open Questions

Resolved by the 2026-06-17 spike:

- ~~Q1: Does the backend honor `store: true` + `previous_response_id`?~~
  **Resolved: no** (`400 "Store must be set to false"`). §5 is `encrypted_content`
  echo only.
- ~~Q4: Is OpenAI-side retention acceptable?~~ **Moot** — 5b is `store:false`,
  no server-side retention.

Still open (resolve before / during implementation):

1. **Can hermes / goose / pr-converge pass a stable conversation id through
   LiteLLM** (header or `user`), or do we rely on tool-call-ID + head-hash?
   (Garden already forwards `traceparent` to codex-proxy — `proxy-plan.ts:711` —
   so the header mechanism has precedent.)
2. **Router (3a) vs. fat proxy (3b)** — final call on whether single-account
   credential isolation is worth the extra hop.
3. **Live cross-account/model scoping confirmation.** The spike's documented
   evidence (openai/codex#17541) is strong but the live replay was blocked by
   `429`s on 2/3 accounts; confirm against the real blob once quota frees up,
   before relying on the (account × model) invalidation rule in anger.

## Related

- ADR 004 — server + per-account credentials / `auth.json` model this builds on
- ADR 005 — deployment topology (one pod per account, LiteLLM in front)
- #10 — buffered `reasoning_content` drop (the narrower, independent bug)
- openai/codex#17541 — model-switch → `invalid_encrypted_content` (blob is
  model-scoped; basis for the (account × model) pin)
- `src/request.rs:481,515,584` — `store: false`, `include: encrypted_content`
- `src/transform/mod.rs:236` — summary → `reasoning_content` (streaming)
- `src/transform/upstream.rs:133-141` — `encrypted_content` explicitly dropped
- `src/credentials/fs.rs` — single-account `auth.json` store
- `garden/deploy/services/litellm/codex-proxy.ts` — one Deployment per account
- `garden/deploy/services/litellm/proxy-plan.ts` — the codex pool / LiteLLM LB
