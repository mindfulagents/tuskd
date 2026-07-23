# OpenTusk — Product Spec Sheet

**Zero-knowledge, portable memory for AI agents.**
*One vault. Every agent. Cryptographic access control. Memory that compounds.*

Version 0.1 · July 2026 · Internal — for team review
Supporting docs referenced at the end; this sheet is the map, those are the territory.

---

## The problem

Agent memory today is **fragmented** (every framework hoards its own store), **trapped** (switching agents loses everything), and **untrusted** (sharing it means handing plaintext to an operator). As users go from one agent to a swarm, and teams start running fleets, there is no memory layer that is simultaneously portable, shared, governed, and private.

## The product

OpenTusk is two products on one data model:

1. **OpenTusk CLI** *(open source, Apache-2.0)* — a single install giving any machine a complete agent-memory system: Markdown memory files in a local vault, local hybrid search (FTS5 + vectors), an MCP server (stdio + HTTP) every agent can speak to, a localhost web UI, and per-agent key provisioning.
2. **OpenTusk SaaS** *(commercial)* — a **zero-knowledge sync & durability service**: Seal-encrypted blobs in a hot cache (S3-class, ≤30s shared-scope freshness) and cold on **Walrus** (Quilt-batched), access control as **Seal policies on Sui**, tamper-evidence via Merkle anchors. **We pay all gas and storage.** We cannot read anything we store — structurally, not by policy.

**Positioning in one line:** *your agents' memory outlives the agents* — platform-neutral (Claude, Hermes, OpenClaw, anything MCP), local-first, leave-anytime (it's a folder of Markdown), private by construction.

---

## How it works (60-second version)

- **Memory = one Markdown file per record**, typed (`episodic / semantic / procedural / skill / profile`), scoped (`agent → project → org`, plus `user`), bitemporal (corrections supersede, history survives, `as_of` queries work).
- **Agents are principals** with ed25519 keys and explicit read/write/promote grants — enforced locally by the daemon, and cryptographically by Seal policies for anything synced. Revocation is an on-chain policy mutation: instant, network-wide, operator-proof.
- **All search is edge-side** (the price and proof of zero-knowledge). Cross-machine sharing is sync-then-search: pull entitled ciphertext, decrypt via Seal's threshold key committee, index locally.
- **Shared memory is gated.** Nothing enters project/org scopes without the promotion gate (dedup → contradiction check → auto-commit or human review queue). The gate is the poisoning defense and the quality mechanism in one.
- **Graceful lapse guarantee:** stop paying and sync stops — but the local vault remains complete and fully functional. Degradation is to local-only, never to data loss.

## The intelligence loop (why it gets smarter)

Storage isn't learning — **compression is**. The loop: experience → scaffolded reflection → distillation (facts, procedures, *negative knowledge*) → promotion gate → shared scopes → reuse → **feedback telemetry** (uses/successes per record) → refine or retire. Procedures that prove themselves across the fleet (≥5 uses, ≥80% success, ≥2 agents) **graduate into versioned skills** — SKILL.md bodies the daemon materializes into a local `skills/` folder that Claude Code, OpenClaw, and Hermes load natively. OpenTusk is thus also the swarm's private, access-controlled, self-updating skills registry. You seed a few skills and the reflection prompt; the loop manufactures the rest.

---

## Who it's for & what they pay

| Segment | Carrying value | Killer moment | Tier |
|---|---|---|---|
| **Individuals** (OpenClaw/Hermes power-users) | portability + private multi-device sync | tell one agent something, ask another | Free local / **Pro ~$12/mo** |
| **Teams / agencies** | shared scopes + review queue; per-client crypto isolation | new hire's agent knows the tribal knowledge on day one | **Team** (seats — core revenue) |
| **Startups** (agent products) | embeddable memory infra + fleet key provisioning | "don't build memory infra" | usage-scaled |
| **Enterprises** (regulated) | provable governance: ZK, on-chain audit, revocation | passes the security review that kills agent pilots | Enterprise / self-host |

**Sequencing:** beachhead on individuals (adoption + public ZK auditors) → Teams prove the business → startups multiply distribution → enterprises arrive for exactly the properties we built on principle. Free tier is *never* crippled — it's the funnel and the trust contract.

## Open source & moat

- **Apache-2.0:** everything edge-side (CLI, daemon, MCP, indexer, record format). Non-negotiable — a ZK claim is only credible with an auditable encryption client.
- **FSL-1.1:** the sync server (source-available, no competitive hosting, auto-converts to Apache after 2 yrs). **Trademark** is the real fence.
- **Moat is operational, not code:** gas sponsorship, epoch renewals, freshness SLA, committee relationships — un-forkable. vs. **MemWal** (Mysten first-party): we win on neutrality and owning the *format*, not the storage. No token.

## Roadmap

| Milestone | Ships |
|---|---|
| **M1 — Local core** | vault, schema, indexer, MCP stdio+HTTP, agent keys, CLI, minimal UI → **OSS launch** |
| **M2 — ZK sync** | Seal client encryption, hot cache, change feed, conflict handling → **Pro beta + encryption audit published** |
| **M3 — Durability** | Walrus Quilt write-behind, Sui anchors, sponsored-tx policy ops, lapse behavior |
| **M4 — Network** | shared scopes at SLA, promotion review UI, skills graduation, provenance/trust surfacing → **Team tier** |

## Success metrics (on the dashboard from day one)

Time-to-competence for a freshly provisioned agent (→ minutes as canon grows) · repeat-failure rate on known failure classes · agent-authored skills overtaking seeded · canon size flat while success rate rises (compression working) — plus the business basics: installs → Pro conversion on second device, Team seats, blob-economics margin.

## Top open decisions

1. Bundled local embedding model (the ZK default) — pick & size budget.
2. Review-queue UX: web-UI only, or also an MCP tool so an agent can be reviewer?
3. Change-feed transport at launch: polling vs. websocket push.
4. Seal committee default composition (t-of-n, which providers) + per-account override UX.
5. Encryption audit vendor + timing (must precede charging for sync).

---

## Reference docs (the territory)

1. **opentusk-spec.md** — full technical spec: vault layout, record schema, scopes/keys, sync engine, MCP surface, CLI, config, security model.
2. **opentusk-architecture.mermaid** — system diagram: three trust zones, all flows.
3. **opentusk-architecture-breakdown.md** — the diagram in prose: zones, the four flows that matter, five load-bearing decisions.
4. **opentusk-oss-commercial-strategy.md** — licensing analysis (Apache/FSL/trademark), monetization mechanics, governance hygiene, risks.
5. **opentusk-segment-use-cases.md** — sweet spots per segment, mechanics that carry each, sequencing logic.
6. **opentusk-intelligence-loop.md** — the compounding loop in depth, skills graduation path, spec deltas, failure modes, dashboard metrics.
7. **multiagent-memory-architecture.md** — the original tiered-architecture exploration (background; superseded where it conflicts with the spec).
