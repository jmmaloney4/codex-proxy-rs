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
document: the concrete implementation lands in follow-up PRs gated on the spike
in §Open Questions.

## Decision

### 1. One feature: a conversation-keyed state record

Introduce a single per-conversation state record holding **both** the pinned
`account_id` **and** the reasoning continuation handle. Affinity and reasoning
persistence are implemented together because they share identity + storage and
because reasoning replay is invalid without affinity. Shipping order is layered
(§7), but they are not independent features.

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
`{ account_id, continuation_handle, updated_at }`, with a **short TTL** (hours —
conversations are ephemeral; TTL is the eviction story). In-proxy memory is
rejected: it fails the moment there is more than one replica or the router
restarts. The existing garden Cloud SQL is a fallback if a new Redis is
unwanted, but TTL + churn make Redis the better fit.

### 5. Persistence mechanism — prefer server-side state, fall back to blob echo

Two families; the choice is gated on a backend capability spike (§Open
Questions):

- **5a. `store: true` + `previous_response_id` (preferred, pending
  verification).** Let OpenAI hold the reasoning server-side; persist only
  `(conversation → last response_id + account)`. No blob storage, no input
  splicing — dramatically simpler. Requires flipping `store` (currently forced
  `false` at `src/request.rs:515`) and accepting OpenAI-side retention
  (privacy/ZDR consideration). **Unverified that the ChatGPT/Codex backend
  honors `store: true` / `previous_response_id`** the way the public API does —
  this is the gating spike.
- **5b. `store: false` + echo `encrypted_content` (fallback).** Stop dropping
  reasoning items (`src/transform/upstream.rs:133-141`), store the ordered blobs
  per conversation, and **splice them back into the reconstructed Responses
  `input`**, aligned to each assistant/tool turn. Full control, no OpenAI
  retention — but alignment is brittle: if the client trimmed or reordered
  history, turn indices drift, so the proxy must detect misalignment and fall
  back (§5c).

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
loop is not.

### 7. Shipping order

1. **Foundation** — identity resolution (§2, start with tool-call-ID + head-hash)
   + shared store (§4) + the router (§3a). Capture `account_id` per
   conversation. **No behavior change yet.**
2. **Affinity** — pin conversations to accounts; point LiteLLM at the single
   router endpoint; router does weighted pick + failover for new conversations.
   Useful on its own (prompt-cache locality, fewer account hops) **without any
   reasoning work**.
3. **Reasoning replay** — spike §5a; ship it if supported, else §5b. Always
   behind §5c.

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

- **Backend `store: true` support is unverified (gates §5).** If unsupported,
  the simpler design (5a) is off the table and 5b's splicing/alignment cost is
  unavoidable. Spike first; do not build storage before this is known.
- **Loss of LiteLLM load-balancing/failover for codex (§3).** The router must
  reimplement weighted selection, health checks, and usage accounting that
  LiteLLM provides today. Underestimating this is the main schedule risk.
- **Identity fragility (§2).** Head-hash breaks on history compaction;
  tool-call-ID only spans tool loops. Without an explicit key, some
  conversations will silently fall back to stateless (acceptable per §5c, but
  caps the hit rate).
- **OpenAI-side retention with `store: true`.** A data-posture decision, not
  just a technical one; must be signed off before 5a.
- **State store as a new dependency / failure mode.** Redis outage must
  degrade to §5c, never block the request path (mirror ADR 005's
  fire-and-forget export discipline).

## Open Questions (resolve before implementation)

1. **Does the ChatGPT/Codex backend honor `store: true` + `previous_response_id`
   (and how does it interact with `include: reasoning.encrypted_content`)?** The
   single biggest fork (§5a vs §5b). Spike this first.
2. **Can hermes / goose / pr-converge pass a stable conversation id through
   LiteLLM** (header or `user`), or do we rely on tool-call-ID + head-hash?
3. **Router (3a) vs. fat proxy (3b)** — final call on whether single-account
   credential isolation is worth the extra hop.
4. **Is OpenAI-side retention acceptable** for the data posture if §5a wins?

## Related

- ADR 004 — server + per-account credentials / `auth.json` model this builds on
- ADR 005 — deployment topology (one pod per account, LiteLLM in front)
- #10 — buffered `reasoning_content` drop (the narrower, independent bug)
- `src/request.rs:481,515,584` — `store: false`, `include: encrypted_content`
- `src/transform/mod.rs:236` — summary → `reasoning_content` (streaming)
- `src/transform/upstream.rs:133-141` — `encrypted_content` explicitly dropped
- `src/credentials/fs.rs` — single-account `auth.json` store
- `garden/deploy/services/litellm/codex-proxy.ts` — one Deployment per account
- `garden/deploy/services/litellm/proxy-plan.ts` — the codex pool / LiteLLM LB
