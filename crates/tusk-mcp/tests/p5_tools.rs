//! P5 exit tests — MCP tool registry, in-process, no transport
//! (build-loop §2 P5): per-tool happy path + at least one ACL denial each.

use chrono::{TimeZone, Utc};
use serde_json::{json, Value};
use std::sync::Arc;
use tusk_core::clock::FakeClock;
use tusk_mcp::{ToolRegistry, TuskContext};

struct Env {
    _dir: tempfile::TempDir,
    ctx: Arc<TuskContext>,
    clock: FakeClock,
}

fn setup() -> Env {
    let dir = tempfile::tempdir().unwrap();
    let clock = FakeClock::new(Utc.with_ymd_and_hms(2026, 1, 1, 12, 0, 0).unwrap());
    let ctx = Arc::new(TuskContext::open(dir.path(), Arc::new(clock.clone())).unwrap());
    // The acceptance-suite pair of agents (build-loop §4).
    ctx.keyring
        .create(
            "hermes-dev",
            &["project:opentusk", "user"],
            &[],
            &["project:opentusk"],
        )
        .unwrap();
    ctx.keyring
        .create(
            "claude-code",
            &["project:opentusk"],
            &[],
            &["project:opentusk"],
        )
        .unwrap();
    Env {
        _dir: dir,
        ctx,
        clock,
    }
}

fn reg(env: &Env, agent: &str) -> ToolRegistry {
    ToolRegistry::new(Arc::clone(&env.ctx), agent.to_string())
}

/// Call a tool, asserting success, and parse the pretty-JSON text.
fn ok(reg: &ToolRegistry, tool: &str, args: Value) -> Value {
    let res = reg.call(tool, &args);
    assert!(!res.is_error, "{tool} errored: {}", res.text);
    serde_json::from_str(&res.text).unwrap_or_else(|_| panic!("{tool}: non-JSON: {}", res.text))
}

/// Call a tool, asserting an ACL denial.
fn denied(reg: &ToolRegistry, tool: &str, args: Value) {
    let res = reg.call(tool, &args);
    assert!(res.is_error, "{tool} unexpectedly succeeded: {}", res.text);
    assert!(
        res.text.starts_with("DENIED: "),
        "{tool}: expected DENIED, got {}",
        res.text
    );
}

#[test]
fn tools_list_has_all_nine() {
    let env = setup();
    let r = reg(&env, "hermes-dev");
    let names: Vec<String> = r.list_tools().iter().map(|t| t.name.clone()).collect();
    for expect in [
        "memory_write",
        "memory_search",
        "memory_get",
        "memory_promote",
        "memory_reflect",
        "memory_feedback",
        "memory_forget",
        "skill_list",
        "memory_status",
    ] {
        assert!(names.contains(&expect.to_string()), "missing {expect}");
    }
    assert_eq!(names.len(), 9);
}

#[test]
fn memory_write_defaults_to_own_scope_and_denies_ungranted() {
    let env = setup();
    let hermes = reg(&env, "hermes-dev");
    let out = ok(
        &hermes,
        "memory_write",
        json!({"content": "Deploy to staging failed — missing env var WALRUS_EPOCHS", "type": "episodic"}),
    );
    let id = out["id"].as_str().unwrap();
    let (_, rec) = env.ctx.vault.get(id).unwrap();
    assert_eq!(rec.scope.to_string(), "agent:hermes-dev");
    assert_eq!(rec.kind.to_string(), "episodic");

    // No write grant on project:opentusk (only read+promote).
    denied(
        &hermes,
        "memory_write",
        json!({"content": "x", "scope": "project:opentusk"}),
    );
    // supersedes path
    let out2 = ok(
        &hermes,
        "memory_write",
        json!({"content": "corrected episode", "supersedes": id}),
    );
    let (_, old) = env.ctx.vault.get(id).unwrap();
    assert!(old.invalid_at.is_some());
    let (_, new) = env.ctx.vault.get(out2["id"].as_str().unwrap()).unwrap();
    assert_eq!(new.supersedes.as_deref(), Some(id));
}

#[test]
fn memory_search_denies_unentitled_scope_and_expands_wildcards() {
    let env = setup();
    let hermes = reg(&env, "hermes-dev");
    ok(
        &hermes,
        "memory_write",
        json!({"content": "private hermes note about walrus"}),
    );

    // claude-code has no read grant on agent:hermes-dev → DENIED (step 2).
    let claude = reg(&env, "claude-code");
    denied(
        &claude,
        "memory_search",
        json!({"query": "walrus", "scopes": ["agent:hermes-dev"]}),
    );

    // Wildcard grants expand against scopes present in the index.
    let wild = env
        .ctx
        .keyring
        .create("wild", &["project:*"], &[], &[])
        .unwrap();
    assert!(!wild.token.is_empty());
    // Promote something into two project scopes via an agent with write.
    env.ctx
        .keyring
        .create("writer", &[], &["project:alpha", "project:beta"], &[])
        .unwrap();
    let writer = reg(&env, "writer");
    ok(
        &writer,
        "memory_write",
        json!({"content": "alpha xylophone fact", "scope": "project:alpha", "type": "semantic"}),
    );
    ok(
        &writer,
        "memory_write",
        json!({"content": "beta xylophone fact", "scope": "project:beta", "type": "semantic"}),
    );
    let wild_reg = reg(&env, "wild");
    let out = ok(&wild_reg, "memory_search", json!({"query": "xylophone"}));
    let hits = out["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 2, "wildcard must expand to both project scopes");
    // But hermes' private note stays invisible.
    let out = ok(&wild_reg, "memory_search", json!({"query": "walrus"}));
    assert!(out["hits"].as_array().unwrap().is_empty());
}

#[test]
fn memory_get_enforces_read_grant() {
    let env = setup();
    let hermes = reg(&env, "hermes-dev");
    let out = ok(
        &hermes,
        "memory_write",
        json!({"content": "secret episode"}),
    );
    let id = out["id"].as_str().unwrap();

    let got = ok(&hermes, "memory_get", json!({"id": id}));
    assert_eq!(got["body"].as_str().unwrap(), "secret episode");

    let claude = reg(&env, "claude-code");
    denied(&claude, "memory_get", json!({"id": id}));
    // Unknown id is an error but not a denial.
    let res = claude.call("memory_get", &json!({"id": "01UNKNOWN00000000000000000"}));
    assert!(res.is_error);
    assert!(!res.text.starts_with("DENIED"));
}

#[test]
fn memory_promote_gates_and_denies() {
    let env = setup();
    let hermes = reg(&env, "hermes-dev");
    let out = ok(
        &hermes,
        "memory_promote",
        json!({"content": "env parity matters a lot", "type": "semantic", "target_scope": "project:opentusk"}),
    );
    assert_eq!(out["action"], "committed");

    // Identical text again → rejected_duplicate (acceptance step 4).
    let out = ok(
        &hermes,
        "memory_promote",
        json!({"content": "env parity matters a lot", "type": "semantic", "target_scope": "project:opentusk"}),
    );
    assert_eq!(out["action"], "rejected_duplicate");

    // corrects path → superseded_existing (acceptance step 7).
    let first = ok(
        &hermes,
        "memory_promote",
        json!({"content": "the deploy queue drains hourly", "type": "semantic", "target_scope": "project:opentusk"}),
    );
    let fact_id = first["id"].as_str().unwrap();
    let out = ok(
        &hermes,
        "memory_promote",
        json!({"content": "correction: the deploy queue actually drains daily", "type": "semantic",
               "target_scope": "project:opentusk", "corrects": fact_id}),
    );
    assert_eq!(out["action"], "superseded_existing");
    assert_eq!(out["superseded"], *fact_id);

    // No promote/write grant on org → DENIED.
    denied(
        &hermes,
        "memory_promote",
        json!({"content": "org fact", "target_scope": "org"}),
    );
}

#[test]
fn memory_reflect_mixed_scopes_per_candidate_actions() {
    let env = setup();
    let hermes = reg(&env, "hermes-dev");
    let out = ok(
        &hermes,
        "memory_reflect",
        json!({"candidates": [
            {"type": "fact", "content": "staging and prod must share env keys", "scope": "project:opentusk"},
            {"type": "procedure", "content": "run scripts/envdiff before deploys", "scope": "project:opentusk"},
            {"type": "fact", "content": "I am not allowed here", "scope": "org"}
        ]}),
    );
    let results = out["results"].as_array().unwrap();
    assert_eq!(results.len(), 3);
    assert_eq!(results[0]["action"], "committed");
    assert_eq!(results[1]["action"], "committed");
    assert_eq!(results[2]["action"], "denied");
    // The committed procedure is a procedural record.
    let id = results[1]["id"].as_str().unwrap();
    let (_, rec) = env.ctx.vault.get(id).unwrap();
    assert_eq!(rec.kind.to_string(), "procedural");
}

#[test]
fn memory_feedback_math_and_denial() {
    let env = setup();
    let hermes = reg(&env, "hermes-dev");
    let out = ok(
        &hermes,
        "memory_reflect",
        json!({"candidates": [
            {"type": "procedure", "content": "run scripts/envdiff before deploys", "scope": "project:opentusk"}
        ]}),
    );
    let id = out["results"][0]["id"].as_str().unwrap().to_string();

    let claude = reg(&env, "claude-code");
    for _ in 0..2 {
        ok(
            &claude,
            "memory_feedback",
            json!({"id": &id, "outcome": "success"}),
        );
    }
    let out = ok(
        &claude,
        "memory_feedback",
        json!({"id": &id, "outcome": "partial"}),
    );
    assert_eq!(out["uses"], 3);
    assert_eq!(out["successes"], 2.5, "partial adds 0.5");
    let out = ok(
        &claude,
        "memory_feedback",
        json!({"id": &id, "outcome": "failure"}),
    );
    assert_eq!(out["uses"], 4);
    assert_eq!(out["successes"], 2.5, "failure adds 0");

    // Agent without read on the record's scope is denied.
    env.ctx.keyring.create("outsider", &[], &[], &[]).unwrap();
    let outsider = reg(&env, "outsider");
    denied(
        &outsider,
        "memory_feedback",
        json!({"id": &id, "outcome": "success"}),
    );
    // Bad outcome value errors.
    let res = claude.call("memory_feedback", &json!({"id": &id, "outcome": "meh"}));
    assert!(res.is_error);
}

#[test]
fn memory_forget_author_or_own_scope_only() {
    let env = setup();
    let hermes = reg(&env, "hermes-dev");
    // Own-scope record: forgettable.
    let own = ok(&hermes, "memory_write", json!({"content": "scratch"}));
    let own_id = own["id"].as_str().unwrap();
    ok(&hermes, "memory_forget", json!({"id": own_id}));
    assert!(env.ctx.vault.get(own_id).is_err());

    // Authored project record: hermes may forget its own authorship…
    let fact = ok(
        &hermes,
        "memory_promote",
        json!({"content": "temp project fact", "target_scope": "project:opentusk"}),
    );
    let fact_id = fact["id"].as_str().unwrap().to_string();
    // …but claude-code (not author, not own scope) may not.
    let claude = reg(&env, "claude-code");
    denied(&claude, "memory_forget", json!({"id": &fact_id}));
    ok(&hermes, "memory_forget", json!({"id": &fact_id}));
    // Gone from the index too.
    let out = ok(
        &hermes,
        "memory_search",
        json!({"query": "temp project fact", "scopes": ["project:opentusk"]}),
    );
    assert!(out["hits"].as_array().unwrap().is_empty());
}

#[test]
fn skill_list_scoped_to_entitlements() {
    let env = setup();
    let hermes = reg(&env, "hermes-dev");
    // Queue + approve a skill in project:opentusk.
    let queued = ok(
        &hermes,
        "memory_promote",
        json!({"content": "# Envdiff\nRun scripts/envdiff.", "type": "skill",
               "target_scope": "project:opentusk", "trigger": "before deploys"}),
    );
    let qid = queued["qid"].as_str().unwrap();
    env.ctx.gate.review(qid, true).unwrap().unwrap();

    let out = ok(&hermes, "skill_list", json!({}));
    let skills = out["skills"].as_array().unwrap();
    assert_eq!(skills.len(), 1);
    assert_eq!(skills[0]["trigger"], "before deploys");
    assert!(skills[0]["uses"].is_number());

    // An agent with no grants sees no skills.
    env.ctx.keyring.create("outsider", &[], &[], &[]).unwrap();
    let outsider = reg(&env, "outsider");
    let out = ok(&outsider, "skill_list", json!({}));
    assert!(out["skills"].as_array().unwrap().is_empty());
}

#[test]
fn memory_status_reports_grants_stats_queue() {
    let env = setup();
    let hermes = reg(&env, "hermes-dev");
    ok(&hermes, "memory_write", json!({"content": "one"}));
    // Queue an org item (hermes has no org grant — use gate directly).
    env.ctx
        .gate
        .submit(
            tusk_core::gate::Candidate {
                kind: tusk_core::record::RecordType::Semantic,
                scope: tusk_core::scope::Scope::Org,
                body: "org pending".into(),
                ..Default::default()
            },
            "system",
        )
        .unwrap();

    let out = ok(&hermes, "memory_status", json!({}));
    assert_eq!(out["agent"]["id"], "hermes-dev");
    assert!(out["agent"]["grants"]["read"]
        .as_array()
        .unwrap()
        .contains(&json!("project:opentusk")));
    assert_eq!(out["index"]["total"], 1);
    assert_eq!(out["review_queue_depth"], 1);

    // Unknown agent identity is refused at the registry boundary.
    let ghost = reg(&env, "ghost");
    let res = ghost.call("memory_status", &json!({}));
    assert!(res.is_error);

    let _ = env.clock; // clock kept for parity with other tests
}

#[test]
fn as_of_and_k_cap_in_search() {
    let env = setup();
    let hermes = reg(&env, "hermes-dev");
    let res = hermes.call(
        "memory_search",
        &json!({"query": "x", "k": 500, "as_of": "not-a-date"}),
    );
    assert!(res.is_error, "bad as_of must error");
    let out = ok(&hermes, "memory_search", json!({"query": "x", "k": 500}));
    assert!(out["hits"].as_array().unwrap().len() <= 50);
}
