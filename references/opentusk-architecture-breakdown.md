# OpenTusk System Architecture — Breakdown

*A component-by-component walkthrough of the architecture diagram. Three trust zones, one kernel, ciphertext everywhere past the device boundary.*

---

## The organizing principle

The system is drawn as **three trust zones**, and the boundaries between them are the architecture:

| Zone | Who runs it | What exists there | Trust assumption |
|---|---|---|---|
| **Edge device** | The user | Plaintext memory, keys, index | Fully trusted (it's their machine) |
| **OpenTusk SaaS** | The operator | Ciphertext blobs + metadata | **Untrusted with content** — zero-knowledge |
| **Sui / Walrus / Seal** | Public networks | Ciphertext, anchors, policies | Trustless — verified, not trusted |

Plaintext exists in exactly one zone. Everything crossing the first boundary is Seal-encrypted *before* it leaves. There is no server-side decrypt, no server-side search, no server-side index — by construction, not by policy.

---

## Zone 1 — Edge device (plaintext)

### Agents
Every agent — Claude, a Hermes profile, OpenClaw, a custom runtime — is a **principal** with its own ed25519 keypair. Agents never touch files directly; they speak MCP to the daemon. Provisioning an agent (`opentusk agent create`) mints the keypair, records grants, and emits a paste-ready MCP config.

### tuskd — the daemon
The heart of the edge. Five internal components, in the order a write flows through them:

1. **MCP server** — the only agent-facing contract. Two transports from one tool registry: *stdio* (spawned per client, bound to an agent identity at launch) and *streamable HTTP* (signed-challenge auth, then session token). Tools: `write, search, get, link, promote, reflect, forget, status`.
2. **Grant enforcement (ACL)** — every call is filtered against the agent's grants: which scopes it may *read*, *write*, and *promote into*. Reads return the union of entitled scopes; writes default to the agent's private scope.
3. **Promotion gate** — the checkpoint between private and shared memory: `reflect → extract candidates → dedup/contradiction-check → classify scope → commit or queue for review`. Per-scope policy (`auto` for projects, `review` for org by default). This is simultaneously the "common learning" mechanism and the memory-poisoning defense.
4. **Indexer** — watches the vault, maintains SQLite FTS5 + local vector embeddings, serves hybrid retrieval (BM25 + vectors + temporal/metadata filters, rank-fused). Strictly **derived state**: rebuildable from files at any time, never the source of truth. Embeddings run locally (bundled small model or the user's own endpoint) — never the SaaS, or zero-knowledge breaks.
5. **Sync engine** — the only component that talks to the outside. Watches files → content-hashes → batches → **Seal-encrypts each record client-side** → appends to a local Merkle log → signs → pushes ciphertext. Also the pull side: fetch → policy-gated decrypt → materialize MD files → reindex.

### Web UI
Localhost dashboard served by tuskd: overview/health, agent management (create/grant/revoke + MCP snippets), scope policies and the promotion **review queue**, sync status and cost meter, live search, config editor.

### Vault — the source of truth
```
memory/            one Markdown file per record, ULID-named
  agent:<id>/      private per-agent
  project:<id>/    shared per-project
  user/            cross-project user model
  org/             widest shared scope
.tusk/
  index.db         derived (FTS5 + vectors)
  keyring/         device key, agent grants
  sync/            merkle log, pending queue
```
Records are YAML-frontmatter Markdown: `type` (episodic/semantic/procedural/profile), `scope`, `author` (provenance), bitemporal fields (`valid_at`/`invalid_at`/`supersedes`), entities/relations, trust, tags. Corrections **supersede** rather than overwrite — history survives, `as_of` queries work. If OpenTusk vanished tomorrow, the vault is still a legible folder of Markdown.

---

## Zone 2 — OpenTusk SaaS (ciphertext-only)

Four components, deliberately boring:

- **Sync API** — accepts ciphertext pushes, serves a per-scope change feed for pulls. What it can observe: blob sizes, timestamps, counts, opaque scope IDs. What it cannot observe: content, entities, embeddings, queries (none exist server-side).
- **Hot cache** — S3-class object store holding **Seal-encrypted blobs**. Exists purely for speed: it's what makes the **≤ 30 s freshness SLA** for shared scopes possible. A breach leaks ciphertext.
- **Write-behind worker** — drains the hot tier into durability: batches small records via **Walrus Quilt** (per-blob overhead → amortized), anchors a **Merkle root per batch on Sui** (gas stays flat as volume grows), renews Walrus storage epochs, and submits **sponsored transactions** (Enoki-style gas station) so users never hold SUI.
- **Billing/metering** — subscription abstraction over blob-bytes, egress, and policy ops. The SaaS pays all gas and storage.

**Lapse behavior is a product guarantee:** churn → epochs stop renewing, hot cache evicts after grace → the Walrus copy expires → **the local vault is untouched and fully functional.** Degradation is to local-only, never to data loss.

---

## Zone 3 — Sui / Walrus / Seal (public, content-free)

- **Walrus (cold)** — the ciphertext system of record. Quilt-batched blobs, epoch-prepaid.
- **Sui** — two kinds of objects: **Merkle anchors** (tamper-evidence and audit for every sync batch) and **Seal policy objects** (Move contracts encoding the ACL: `agent` scope = owner key only, `project` = allowlist, `org` = role/subscription).
- **Seal key-server committee** — t-of-n independent key servers using threshold identity-based encryption. On a decrypt request they **evaluate the on-chain policy** and release key shares only if it passes. This is what makes the ACL hold *even against the operator*: the SaaS stores the blobs but cannot convene the keys.

---

## The four flows that matter

**1. Write → shared memory**
Agent calls `memory.write` → ACL check → promotion gate (if targeting a shared scope) → MD file lands in vault → indexer picks it up → sync engine encrypts and pushes → hot cache → (async) Quilt → Walrus + anchor. Other entitled agents see it within the freshness SLA.

**2. Search (always local)**
`memory.search` never leaves the device. The index answers from whatever scopes this device has synced and decrypted. Cross-machine sharing is **sync-then-search**: pull entitled ciphertext, decrypt via Seal, index locally, query locally.

**3. Grant / revoke**
`opentusk agent grant/revoke` updates local grants immediately **and** compiles to a Seal policy mutation on Sui (sponsored by the SaaS). From that moment the key committee stops honoring the revoked key — enforcement is cryptographic and network-wide, not a row in the operator's database. (Plaintext already delivered to a revoked agent is out of scope; nothing can retract it.)

**4. New device / another agent joins**
Same tuskd, its own keys. It authenticates, pulls the change feed for its entitled scopes, requests decryption from the committee (policy-checked), materializes the Markdown, builds its own index. No server ever searched anything on its behalf.

---

## Why it's shaped this way — the five load-bearing decisions

1. **Files as truth, index as cache** → portability, human-legibility, and a trivially correct rebuild story.
2. **Ciphertext in *both* storage tiers** → zero-knowledge is structural; there is no "trusted hot path" to quietly reintroduce operator trust.
3. **Edge-only search** → the price of ZK, paid deliberately; sync-then-search still delivers cross-platform shared memory.
4. **Promotion gate on shared scopes** → shared memory dies from pollution before it dies from bad recall; the gate + provenance is the immune system.
5. **Seal policies as the real ACL** → local enforcement is convenience; on-chain policy + threshold keys is the guarantee, and it survives operator compromise.
