# OpenTusk — The Intelligence Loop

*How a swarm on shared memory actually gets smarter over time, why storage alone doesn't do it, and where skills fit (short answer: the loop should* write *the skills, not depend on you authoring them).*

---

## 1. First, the uncomfortable truth: recall is not learning

A vault plus retrieval gives you an agent that *remembers*. That's necessary but it isn't intelligence growth — an agent that recalls 10,000 raw episodic facts is marginally better than one that recalls none, and past a point it's *worse* (retrieval noise, contradictory stale facts, context bloat). Swarms get smarter only when raw experience is repeatedly **compressed into more reusable forms**. That compression pipeline is the intelligence loop, and OpenTusk's job is to be the machine that runs it.

The ladder of compression, lowest to highest value:

```
episodic fact        "deploy failed at 14:02, missing env var WALRUS_EPOCHS"
   ↓ distill
semantic fact        "staging requires WALRUS_EPOCHS set; prod injects it via CI"
   ↓ generalize
procedure            "before any deploy: verify env parity with scripts/envdiff"
   ↓ package + validate
skill                versioned, loadable playbook any agent can execute
```

Each rung is smaller, truer, and more reusable than the one below it. "Getting smarter" = mass migrating up this ladder, across the whole swarm, with junk filtered out on the way.

---

## 2. The loop, stage by stage

```
        ┌──────────────────────────────────────────────────────┐
        ▼                                                      │
   1 EXPERIENCE  →  2 REFLECT  →  3 DISTILL  →  4 GATE  →  5 SHARE
        ▲                                                      │
        │                                                      ▼
   8 REFINE / RETIRE  ←  7 FEEDBACK  ←  6 REUSE (retrieval into new work)
```

**1. Experience (episodic capture).** Agents work; session traces and notable events land as `episodic` records in their private `agent:` scope. Cheap, noisy, high-volume — and deliberately quarantined there.

**2. Reflect (the trigger).** The Hermes lesson: capture fails if it relies on the agent spontaneously deciding to save. So reflection is *scaffolded*: `nudge_interval` fires periodically, flush-before-compaction fires at context limits, and `memory.reflect(session_id)` can be called explicitly. Reflection asks one question: *what in this session would change how a future agent acts?*

**3. Distill.** Reflection output isn't a transcript summary — it's typed candidates: semantic facts (durable truths), procedures (repeatable how-tos), *and corrections* (existing records this session contradicted, which become supersessions, not new clutter). Crucially, it also extracts **negative knowledge** — "approach X fails because Y" — which is the highest-value, most under-captured class of team learning.

**4. Gate (quality control).** The promotion gate is where "more memory" is prevented from becoming "worse memory": dedup against the target scope, contradiction check (bitemporal supersession if the new fact wins), scope classification, and per-scope policy — `auto` into project scopes, human `review` into org canon. The gate is the swarm's immune system; every shared-memory system that skips it drowns in pollution within weeks.

**5. Share.** Accepted records land in `project:`/`org` scopes, sync through the ZK pipeline, and are on every entitled agent's local index within the freshness SLA. A lesson one agent learned Tuesday is operational knowledge for the whole swarm by Tuesday-plus-30-seconds.

**6. Reuse.** Retrieval pulls shared procedures and facts into new sessions. This is where compounding becomes visible: agent B starts from agent A's ceiling, not from zero.

**7. Feedback (the stage most systems skip — and the one that makes it a *loop*).** Reuse must report back. A lightweight `memory.feedback(id, outcome)` call — worked / failed / partially — accumulates on each record: `uses`, `successes`, `last_used`. Without this, the swarm can't tell its good procedures from its plausible-sounding bad ones, and retrieval can't rank by demonstrated value.

**8. Refine or retire.** High-use, high-success procedures get *hardened* (periodic reflection pass merges variants, tightens wording, promotes to skill — §4). Low-success records get superseded or `invalid_at`-ed. Unused records decay in retrieval rank. The canon stays small, current, and battle-tested — which is exactly what "the swarm got smarter" means operationally.

---

## 3. Why this compounds across a *swarm* specifically

Three multiplier effects a single agent can't get:

- **Parallel experience, shared distillation.** Ten agents encounter ten different edge cases in a week; the gate merges them into one canon. Learning rate scales with fleet size, not calendar time.
- **Cross-role transfer.** The support agent's distilled resolution patterns become the product agent's context; the deploy agent's negative knowledge saves the coding agent from a doomed approach. Scopes make this deliberate rather than accidental.
- **Statistical validation.** One agent using a procedure once proves little. Forty uses across twelve agents with a 92% success signal is *evidence* — and it's the feedback loop (stage 7) that turns swarm scale into confidence you can rank on.

The bitemporal layer quietly does heavy lifting here: when the world changes (API deprecated, policy updated), the correction *supersedes* everywhere at once instead of fighting stale copies in ten private stores — the swarm un-learns as one, which matters as much as learning.

---

## 4. Skills: do you need to set them up?

**Reframe: a skill is not something you author beside the memory system — it's the top rung of the memory ladder.** A skill is a procedure that has been validated by reuse, packaged with structure (when to trigger, steps, checks, failure modes), and versioned. The design goal is that the *loop manufactures skills*; hand-authoring is just seeding.

So: `type: skill` becomes the fifth record type — a procedural record whose body **is a SKILL.md** (name, trigger description, instructions), carried in the same vault, same scopes, same sync, same Seal ACL.

**How a skill is born (the graduation path):**
```
procedure record accumulates: uses ≥ N, success ≥ threshold, ≥ M distinct agents
  → reflection "hardening" pass: merge variants, add preconditions/failure modes,
    write trigger description, emit SKILL.md body
  → promotion gate (skills default to `review` — they change behavior, so a
    human eyeballs them once)
  → skill record in project/org scope, version 1
  → future feedback drives refinement passes → version bumps via supersession
```

**Distribution is nearly free.** Skills sync like any record; `memory.search(type: skill)` gives dynamic discovery, and because the body is standard SKILL.md, tuskd can **materialize entitled skills into a local `skills/` directory** that Claude Code, OpenClaw, and Hermes load natively. OpenTusk becomes the swarm's private, access-controlled, self-updating skills registry — the artifact format the ecosystem already speaks, minus the public-registry trust problem.

**What you do still author (the honest part):**
1. **Seed skills** — a handful of hand-written SKILL.mds encoding what you already know, so the swarm doesn't start from zero. Hours, not weeks.
2. **The reflection skill itself** — the meta-prompt that runs stages 2–3 well is the highest-leverage prompt in the system. Ship it as a built-in, tunable per deployment.
3. **Graduation thresholds and review policy** — config, not code.

Everything past seeding, the loop should produce. If it doesn't — if six months in the skill shelf is still only your seeds — the loop is broken, and that's the metric to watch.

---

## 5. What this adds to the spec (delta)

- **Record type:** `skill` (procedural + SKILL.md body, trigger description, version chain via `supersedes`).
- **Telemetry fields:** `uses`, `successes`, `last_used` on procedural/skill records (updated locally, synced like content).
- **MCP additions:** `memory.feedback(id, outcome, note?)`; `skill.list(scopes?)`; `skill.materialize(dir)` (or tuskd auto-materializes on sync).
- **Reflection hardening pass:** scheduled job (weekly cron per project scope) that merges procedure variants and proposes graduations.
- **Web UI:** skills shelf per scope — versions, usage/success stats, review queue for graduations.
- **Defaults:** graduation at `uses ≥ 5, success ≥ 0.8, agents ≥ 2`; skills promote via `review` everywhere.

---

## 6. Failure modes to design against

- **Pollution** — one verbose agent flooding shared scope. Defense: the gate, per-agent write quotas into shared scopes, provenance-weighted trust.
- **Staleness** — last year's procedure confidently wrong. Defense: feedback-driven decay, supersession, `last_used` aging in rank.
- **Overfitting** — one lucky success enshrined as canon. Defense: multi-agent thresholds before graduation (the `agents ≥ 2` rule exists precisely for this).
- **Reflection junk** — bad distillation poisons everything downstream. Defense: treat the reflection prompt as versioned, evaluated infrastructure; A/B it like retrieval.
- **Feedback apathy** — agents never call `memory.feedback`. Defense: make it ambient — integration guides wire it into task-completion hooks rather than trusting the model to volunteer it (same lesson as the nudge: scaffold, don't hope).

---

## 7. What "smarter" looks like on a dashboard

If the loop works, these curves move: **time-to-competence** for a newly provisioned agent (should collapse toward minutes as canon grows); **repeat-failure rate** on known failure classes (negative knowledge doing its job); **skill shelf composition** (agent-authored overtaking seeded); **canon size vs. success rate** (flat-to-shrinking record count with rising success = compression working; ballooning count = the gate is leaking). Put these four on the web UI's overview panel — they're the product's proof, and eventually its sales demo.
