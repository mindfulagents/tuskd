//! P4 exit tests — gate & loop kernel (build-loop §2 P4).

use chrono::{TimeZone, Utc};
use std::sync::Arc;
use tusk_core::clock::FakeClock;
use tusk_core::gate::{Candidate, Gate, GateOutcome, GraduationConfig};
use tusk_core::index::{Indexer, RankingConfig, SearchQuery};
use tusk_core::record::RecordType;
use tusk_core::scope::Scope;
use tusk_core::vault::VaultStore;

struct Env {
    _dir: tempfile::TempDir,
    vault: Arc<VaultStore>,
    indexer: Arc<Indexer>,
    gate: Gate,
    #[allow(dead_code)]
    clock: FakeClock,
}

fn setup() -> Env {
    let dir = tempfile::tempdir().unwrap();
    let clock = FakeClock::new(Utc.with_ymd_and_hms(2026, 1, 1, 12, 0, 0).unwrap());
    let vault = Arc::new(VaultStore::init(dir.path(), Arc::new(clock.clone())).unwrap());
    let indexer = Arc::new(
        Indexer::open(&vault.tusk_dir().join("index.db"), RankingConfig::default()).unwrap(),
    );
    let gate = Gate::new(Arc::clone(&vault), Arc::clone(&indexer), Default::default()).unwrap();
    Env {
        _dir: dir,
        vault,
        indexer,
        gate,
        clock,
    }
}

fn cand(kind: RecordType, scope: &str, body: &str) -> Candidate {
    Candidate {
        kind,
        scope: Scope::parse(scope).unwrap(),
        body: body.to_string(),
        ..Default::default()
    }
}

#[test]
fn duplicate_rejected() {
    let env = setup();
    let c = cand(
        RecordType::Procedural,
        "project:opentusk",
        "run scripts/envdiff before deploys",
    );
    let first = env.gate.submit(c.clone(), "hermes-dev").unwrap();
    let GateOutcome::Committed { id } = &first else {
        panic!("expected committed, got {first:?}");
    };
    let second = env.gate.submit(c, "hermes-dev").unwrap();
    match second {
        GateOutcome::RejectedDuplicate { existing_id } => assert_eq!(&existing_id, id),
        other => panic!("expected rejected_duplicate, got {other:?}"),
    }
}

#[test]
fn near_duplicate_auto_supersedes() {
    let env = setup();
    let first = env
        .gate
        .submit(
            cand(
                RecordType::Semantic,
                "project:opentusk",
                "staging and production environment variables must stay in parity always",
            ),
            "hermes-dev",
        )
        .unwrap();
    let GateOutcome::Committed { id: old_id } = first else {
        panic!("expected committed");
    };
    // >0.7 token overlap with the first body, but not byte-identical.
    let second = env
        .gate
        .submit(
            cand(
                RecordType::Semantic,
                "project:opentusk",
                "staging and production environment variables must stay in parity",
            ),
            "hermes-dev",
        )
        .unwrap();
    match second {
        GateOutcome::SupersededExisting { id, superseded } => {
            assert_eq!(superseded, old_id);
            let (_, old) = env.vault.get(&old_id).unwrap();
            assert!(old.invalid_at.is_some());
            let (_, new) = env.vault.get(&id).unwrap();
            assert_eq!(new.supersedes.as_deref(), Some(old_id.as_str()));
        }
        other => panic!("expected superseded_existing, got {other:?}"),
    }
}

#[test]
fn distinct_content_commits_without_superseding() {
    let env = setup();
    env.gate
        .submit(
            cand(
                RecordType::Semantic,
                "project:opentusk",
                "staging and production must have environment variable parity",
            ),
            "hermes-dev",
        )
        .unwrap();
    let second = env
        .gate
        .submit(
            cand(
                RecordType::Procedural,
                "project:opentusk",
                "always run scripts/envdiff to compare configs before any deploy",
            ),
            "hermes-dev",
        )
        .unwrap();
    assert!(matches!(second, GateOutcome::Committed { .. }));
}

#[test]
fn explicit_corrects_supersedes() {
    let env = setup();
    let first = env
        .gate
        .submit(
            cand(RecordType::Semantic, "project:opentusk", "the port is 7477"),
            "hermes-dev",
        )
        .unwrap();
    let GateOutcome::Committed { id: old_id } = first else {
        panic!("expected committed");
    };
    let mut c = cand(
        RecordType::Semantic,
        "project:opentusk",
        "correction: completely different wording here entirely",
    );
    c.corrects = Some(old_id.clone());
    let out = env.gate.submit(c, "hermes-dev").unwrap();
    match out {
        GateOutcome::SupersededExisting { superseded, .. } => assert_eq!(superseded, old_id),
        other => panic!("expected superseded_existing, got {other:?}"),
    }
}

#[test]
fn org_queues_project_commits() {
    let env = setup();
    let org = env
        .gate
        .submit(
            cand(RecordType::Semantic, "org", "company-wide fact"),
            "hermes-dev",
        )
        .unwrap();
    assert!(matches!(org, GateOutcome::Queued { .. }), "org must queue");
    let proj = env
        .gate
        .submit(
            cand(RecordType::Semantic, "project:opentusk", "project fact"),
            "hermes-dev",
        )
        .unwrap();
    assert!(
        matches!(proj, GateOutcome::Committed { .. }),
        "project must auto-commit"
    );
    assert_eq!(env.gate.review_list().unwrap().len(), 1);
}

#[test]
fn skill_always_queues_even_in_auto_scope() {
    let env = setup();
    let mut c = cand(RecordType::Skill, "project:opentusk", "# Deploy\nsteps...");
    c.trigger = Some("when deploying".into());
    let out = env.gate.submit(c, "hermes-dev").unwrap();
    assert!(matches!(out, GateOutcome::Queued { .. }));
}

#[test]
fn approve_commits_and_materializes_skill() {
    let env = setup();
    let mut c = cand(
        RecordType::Skill,
        "project:opentusk",
        "# Env Diff Procedure\n\nRun scripts/envdiff before deploys.",
    );
    c.trigger = Some("use before every deploy".into());
    c.tags = vec!["graduated".into()];
    let GateOutcome::Queued { qid } = env.gate.submit(c, "hermes-dev").unwrap() else {
        panic!("expected queued");
    };

    let outcome = env.gate.review(&qid, true).unwrap().unwrap();
    let GateOutcome::Committed { id } = outcome else {
        panic!("expected committed on approve");
    };

    // Record exists and is a skill.
    let (_, rec) = env.vault.get(&id).unwrap();
    assert_eq!(rec.kind, RecordType::Skill);

    // Materialized SKILL.md with name: and description: frontmatter.
    let skill_path = env
        .vault
        .skills_dir()
        .join("project-opentusk")
        .join(&id)
        .join("SKILL.md");
    assert!(skill_path.exists(), "SKILL.md must be materialized");
    let text = std::fs::read_to_string(&skill_path).unwrap();
    assert!(text.starts_with("---\n"));
    assert!(text.contains("\nname: ") || text.contains("---\nname: "));
    assert!(text.contains("description: "));
    assert!(text.contains("use before every deploy"));

    // Queue drained.
    assert!(env.gate.review_list().unwrap().is_empty());
    // Searchable.
    let hits = env
        .indexer
        .search(&SearchQuery {
            query: "envdiff".into(),
            ..Default::default()
        })
        .unwrap();
    assert!(hits.iter().any(|h| h.id == id));
}

#[test]
fn reject_drains_queue_without_commit() {
    let env = setup();
    let GateOutcome::Queued { qid } = env
        .gate
        .submit(
            cand(RecordType::Semantic, "org", "org fact to reject"),
            "hermes-dev",
        )
        .unwrap()
    else {
        panic!("expected queued");
    };
    let outcome = env.gate.review(&qid, false).unwrap();
    assert!(outcome.is_none(), "reject commits nothing");
    assert!(env.gate.review_list().unwrap().is_empty());
    assert!(env.vault.walk().unwrap().is_empty());
    // Unknown qid errors.
    assert!(env.gate.review(&qid, true).is_err());
}

#[test]
fn graduation_thresholds() {
    let env = setup();
    // Qualifying procedure: uses=7, successes=7.
    let GateOutcome::Committed { id: proc_id } = env
        .gate
        .submit(
            cand(
                RecordType::Procedural,
                "project:opentusk",
                "Run scripts/envdiff before every deploy.\n\nDetails follow.",
            ),
            "hermes-dev",
        )
        .unwrap()
    else {
        panic!()
    };
    for _ in 0..7 {
        let rec = env
            .vault
            .update_telemetry(&proc_id, 1, 1.0, env.vault.now())
            .unwrap();
        let (path, _) = env.vault.get(&proc_id).unwrap();
        env.indexer.ingest_record(&path, &rec).unwrap();
    }
    // Below-threshold procedure: uses=2.
    let GateOutcome::Committed { id: weak_id } = env
        .gate
        .submit(
            cand(
                RecordType::Procedural,
                "project:opentusk",
                "Sometimes restart the cache maybe.",
            ),
            "hermes-dev",
        )
        .unwrap()
    else {
        panic!()
    };
    let rec = env
        .vault
        .update_telemetry(&weak_id, 2, 1.0, env.vault.now())
        .unwrap();
    let (path, _) = env.vault.get(&weak_id).unwrap();
    env.indexer.ingest_record(&path, &rec).unwrap();

    let queued = env.gate.graduate(&GraduationConfig::default()).unwrap();
    assert_eq!(queued.len(), 1, "exactly one skill candidate");
    let items = env.gate.review_list().unwrap();
    assert_eq!(items.len(), 1);
    let item = &items[0];
    assert_eq!(item.candidate.kind, RecordType::Skill);
    assert!(item.candidate.tags.contains(&"graduated".to_string()));
    assert!(
        item.candidate.trigger.is_some(),
        "trigger from first line of body"
    );
    assert!(
        item.candidate
            .trigger
            .as_deref()
            .unwrap()
            .contains("envdiff"),
        "trigger derived from first line"
    );
    assert!(
        item.candidate.body.contains(&proc_id),
        "provenance footer references source record"
    );

    // Running again does not duplicate the pending candidate.
    let again = env.gate.graduate(&GraduationConfig::default()).unwrap();
    assert!(again.is_empty(), "second scan must not re-queue");
    assert_eq!(env.gate.review_list().unwrap().len(), 1);
}
