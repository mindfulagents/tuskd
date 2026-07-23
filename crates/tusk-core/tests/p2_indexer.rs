//! P2 exit tests — indexer (build-loop §2 P2).

use chrono::{Duration, TimeZone, Utc};
use std::sync::Arc;
use tusk_core::clock::{Clock, FakeClock};
use tusk_core::index::{Indexer, RankingConfig, SearchQuery};
use tusk_core::record::RecordType;
use tusk_core::scope::Scope;
use tusk_core::vault::VaultStore;

fn setup() -> (tempfile::TempDir, Arc<VaultStore>, Arc<Indexer>, FakeClock) {
    let dir = tempfile::tempdir().unwrap();
    let clock = FakeClock::new(Utc.with_ymd_and_hms(2026, 1, 1, 12, 0, 0).unwrap());
    let vault = Arc::new(VaultStore::init(dir.path(), Arc::new(clock.clone())).unwrap());
    let indexer = Arc::new(
        Indexer::open(&vault.tusk_dir().join("index.db"), RankingConfig::default()).unwrap(),
    );
    (dir, vault, indexer, clock)
}

fn q(query: &str) -> SearchQuery {
    SearchQuery {
        query: query.to_string(),
        ..Default::default()
    }
}

#[test]
fn rebuild_is_idempotent() {
    let (_dir, vault, indexer, _clock) = setup();
    for body in ["alpha beta gamma", "beta gamma delta", "unrelated text"] {
        let rec = vault.new_record(
            RecordType::Semantic,
            Scope::parse("project:opentusk").unwrap(),
            "a1",
            body,
        );
        vault.write(&rec).unwrap();
    }
    indexer.rebuild(&vault).unwrap();
    let first: Vec<_> = indexer
        .search(&q("beta gamma"))
        .unwrap()
        .into_iter()
        .map(|h| (h.id, format!("{:.6}", h.score)))
        .collect();
    indexer.rebuild(&vault).unwrap();
    let second: Vec<_> = indexer
        .search(&q("beta gamma"))
        .unwrap()
        .into_iter()
        .map(|h| (h.id, format!("{:.6}", h.score)))
        .collect();
    assert_eq!(first, second);
    assert_eq!(first.len(), 2);
}

#[test]
fn as_of_returns_superseded_at_mid_and_current_at_now() {
    let (_dir, vault, indexer, clock) = setup();
    let scope = Scope::parse("project:opentusk").unwrap();

    let old = vault.new_record(
        RecordType::Semantic,
        scope.clone(),
        "a1",
        "staging env parity",
    );
    vault.write(&old).unwrap();
    indexer.ingest_record(&vault.path_for(&old), &old).unwrap();

    clock.advance(Duration::seconds(100));
    let t_mid = clock.now();
    clock.advance(Duration::seconds(100));

    let mut new = vault.new_record(
        RecordType::Semantic,
        scope.clone(),
        "a1",
        "staging env parity — corrected",
    );
    vault.supersede(&mut new, &old.id).unwrap();
    indexer.rebuild(&vault).unwrap();

    // At t_mid the original is the valid record.
    let mut query = q("staging parity");
    query.as_of = Some(t_mid);
    let hits = indexer.search(&query).unwrap();
    let ids: Vec<_> = hits.iter().map(|h| h.id.as_str()).collect();
    assert!(ids.contains(&old.id.as_str()), "expected old at t_mid");
    assert!(
        !ids.contains(&new.id.as_str()),
        "new not yet valid at t_mid"
    );

    // Now: only the correction.
    let hits = indexer.search(&q("staging parity")).unwrap();
    let ids: Vec<_> = hits.iter().map(|h| h.id.as_str()).collect();
    assert!(ids.contains(&new.id.as_str()));
    assert!(!ids.contains(&old.id.as_str()));

    // A record created "now" is not valid before its creation (pitfall 3).
    let mut early = q("staging parity");
    early.as_of = Some(Utc.with_ymd_and_hms(2020, 1, 1, 0, 0, 0).unwrap());
    assert!(indexer.search(&early).unwrap().is_empty());
}

#[test]
fn tag_and_type_filters() {
    let (_dir, vault, indexer, _clock) = setup();
    let scope = Scope::parse("project:opentusk").unwrap();
    let mut a = vault.new_record(
        RecordType::Semantic,
        scope.clone(),
        "a1",
        "deploy checklist",
    );
    a.tags = vec!["deploy".into(), "ops".into()];
    let b = vault.new_record(
        RecordType::Procedural,
        scope.clone(),
        "a1",
        "deploy checklist",
    );
    vault.write(&a).unwrap();
    vault.write(&b).unwrap();
    indexer.rebuild(&vault).unwrap();

    let mut query = q("deploy");
    query.tags = Some(vec!["ops".into()]);
    let hits = indexer.search(&query).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].id, a.id);

    let mut query = q("deploy");
    query.kind = Some(RecordType::Procedural);
    let hits = indexer.search(&query).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].id, b.id);
}

#[test]
fn scope_filter_and_scopes_present() {
    let (_dir, vault, indexer, _clock) = setup();
    let p = vault.new_record(
        RecordType::Semantic,
        Scope::parse("project:opentusk").unwrap(),
        "a1",
        "shared knowledge",
    );
    let g = vault.new_record(
        RecordType::Episodic,
        Scope::parse("agent:hermes-dev").unwrap(),
        "hermes-dev",
        "shared knowledge private",
    );
    vault.write(&p).unwrap();
    vault.write(&g).unwrap();
    indexer.rebuild(&vault).unwrap();

    let mut query = q("shared knowledge");
    query.scopes = Some(vec![Scope::parse("agent:hermes-dev").unwrap()]);
    let hits = indexer.search(&query).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].id, g.id);

    let mut present = indexer.scopes_present().unwrap();
    present.sort();
    assert_eq!(present, vec!["agent:hermes-dev", "project:opentusk"]);
}

#[test]
fn telemetry_boosts_ranking() {
    let (_dir, vault, indexer, clock) = setup();
    let scope = Scope::parse("project:opentusk").unwrap();
    let proven = vault.new_record(
        RecordType::Procedural,
        scope.clone(),
        "a1",
        "run envdiff before deploys",
    );
    vault.write(&proven).unwrap();
    for _ in 0..7 {
        vault
            .update_telemetry(&proven.id, 1, 1.0, clock.now())
            .unwrap();
    }
    let unproven = vault.new_record(
        RecordType::Procedural,
        scope.clone(),
        "a2",
        "run envdiff before deploys",
    );
    vault.write(&unproven).unwrap();
    indexer.rebuild(&vault).unwrap();

    let hits = indexer.search(&q("envdiff deploys")).unwrap();
    assert_eq!(hits.len(), 2);
    assert_eq!(
        hits[0].id, proven.id,
        "uses=7/successes=7 must outrank uses=0"
    );
    assert!(hits[0].score > hits[1].score);
}

#[test]
fn empty_query_lists_by_recency() {
    let (_dir, vault, indexer, clock) = setup();
    let scope = Scope::parse("project:opentusk").unwrap();
    let first = vault.new_record(RecordType::Semantic, scope.clone(), "a1", "older");
    vault.write(&first).unwrap();
    clock.advance(Duration::seconds(60));
    let second = vault.new_record(RecordType::Semantic, scope.clone(), "a1", "newer");
    vault.write(&second).unwrap();
    indexer.rebuild(&vault).unwrap();

    let hits = indexer.search(&q("")).unwrap();
    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0].id, second.id);
    assert_eq!(hits[1].id, first.id);
}

#[test]
fn ingest_remove_and_stats() {
    let (_dir, vault, indexer, _clock) = setup();
    let rec = vault.new_record(
        RecordType::Semantic,
        Scope::parse("user").unwrap(),
        "a1",
        "remember me",
    );
    let path = vault.write(&rec).unwrap();
    indexer.ingest_record(&path, &rec).unwrap();
    assert_eq!(indexer.search(&q("remember")).unwrap().len(), 1);

    let stats = indexer.stats().unwrap();
    assert_eq!(stats.total, 1);
    assert_eq!(stats.valid, 1);

    indexer.remove(&rec.id).unwrap();
    assert!(indexer.search(&q("remember")).unwrap().is_empty());
    assert_eq!(indexer.stats().unwrap().total, 0);
}

#[test]
fn watcher_picks_up_new_and_deleted_files() {
    let (_dir, vault, indexer, _clock) = setup();
    indexer.rebuild(&vault).unwrap();
    let watcher = tusk_core::watch::VaultWatcher::start(
        Arc::clone(&vault),
        Arc::clone(&indexer),
        std::time::Duration::from_millis(50),
    )
    .unwrap();

    let rec = vault.new_record(
        RecordType::Semantic,
        Scope::parse("project:opentusk").unwrap(),
        "a1",
        "zebra quokka xylophone",
    );
    let path = vault.write(&rec).unwrap();

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
    loop {
        if !indexer.search(&q("quokka")).unwrap().is_empty() {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "new file not searchable within 1s"
        );
        std::thread::sleep(std::time::Duration::from_millis(25));
    }

    std::fs::remove_file(&path).unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
    loop {
        if indexer.search(&q("quokka")).unwrap().is_empty() {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "deleted file still searchable after 1s"
        );
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    watcher.stop();
}

#[test]
fn watcher_tolerates_partial_writes() {
    let (_dir, vault, indexer, _clock) = setup();
    indexer.rebuild(&vault).unwrap();
    let watcher = tusk_core::watch::VaultWatcher::start(
        Arc::clone(&vault),
        Arc::clone(&indexer),
        std::time::Duration::from_millis(50),
    )
    .unwrap();

    // A half-written record (no closing fence) must be skipped, not crash.
    let dir = vault.memory_dir().join("project").join("opentusk");
    std::fs::create_dir_all(&dir).unwrap();
    let partial = dir.join("01PARTIALWRITE0000000000ZZ.md");
    std::fs::write(&partial, "---\nid: 01PARTIALWRITE0000000000ZZ\n").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(300));

    // Completing the file is picked up on the next event.
    let rec = vault.new_record(
        RecordType::Semantic,
        Scope::parse("project:opentusk").unwrap(),
        "a1",
        "flamingo aardvark",
    );
    let text = rec.to_markdown().unwrap();
    let text = text.replace(&rec.id, "01PARTIALWRITE0000000000ZZ");
    std::fs::write(&partial, text).unwrap();

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        if !indexer.search(&q("flamingo")).unwrap().is_empty() {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "completed file not picked up"
        );
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    watcher.stop();
}
