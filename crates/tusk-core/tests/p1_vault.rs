//! P1 exit tests — vault & records (build-loop §2 P1).

use chrono::{Duration, TimeZone, Utc};
use proptest::prelude::*;
use std::sync::Arc;
use tusk_core::clock::FakeClock;
use tusk_core::record::{Record, RecordType};
use tusk_core::scope::Scope;
use tusk_core::vault::VaultStore;

fn fake_clock() -> FakeClock {
    FakeClock::new(Utc.with_ymd_and_hms(2026, 1, 1, 12, 0, 0).unwrap())
}

fn store(dir: &std::path::Path) -> (VaultStore, FakeClock) {
    let clock = fake_clock();
    let vs = VaultStore::init(dir, Arc::new(clock.clone())).unwrap();
    (vs, clock)
}

fn sample(vs: &VaultStore, body: &str) -> Record {
    vs.new_record(
        RecordType::Semantic,
        Scope::parse("project:opentusk").unwrap(),
        "hermes-dev",
        body,
    )
}

#[test]
fn write_then_get_equality() {
    let dir = tempfile::tempdir().unwrap();
    let (vs, _) = store(dir.path());
    let mut rec = sample(&vs, "Env parity matters.\n\nSecond paragraph.");
    rec.entities = vec!["staging".into(), "env vars".into()];
    rec.tags = vec!["deploy".into()];
    rec.trust = 0.7;
    vs.write(&rec).unwrap();
    let (_, got) = vs.get(&rec.id).unwrap();
    assert_eq!(rec, got);
}

#[test]
fn unicode_bodies_survive() {
    let dir = tempfile::tempdir().unwrap();
    let (vs, _) = store(dir.path());
    let body = "héllo wörld — 日本語テスト 🚀 \"quotes\" [brackets], colons: yes";
    let rec = sample(&vs, body);
    vs.write(&rec).unwrap();
    let (_, got) = vs.get(&rec.id).unwrap();
    assert_eq!(got.body, body);
}

#[test]
fn supersede_sets_invalid_at_on_old_and_supersedes_on_new() {
    let dir = tempfile::tempdir().unwrap();
    let (vs, clock) = store(dir.path());
    let old = sample(&vs, "Old fact.");
    vs.write(&old).unwrap();

    clock.advance(Duration::seconds(60));
    let mut new = sample(&vs, "Corrected fact.");
    vs.supersede(&mut new, &old.id).unwrap();

    let (_, old_after) = vs.get(&old.id).unwrap();
    let (_, new_after) = vs.get(&new.id).unwrap();
    assert_eq!(new_after.supersedes.as_deref(), Some(old.id.as_str()));
    assert_eq!(old_after.invalid_at, Some(clock_now(&clock)));
    // Old record's body untouched (corrections never edit in place).
    assert_eq!(old_after.body, "Old fact.");
}

fn clock_now(c: &FakeClock) -> chrono::DateTime<Utc> {
    use tusk_core::clock::Clock;
    c.now()
}

#[test]
fn forget_removes_file() {
    let dir = tempfile::tempdir().unwrap();
    let (vs, _) = store(dir.path());
    let rec = sample(&vs, "ephemeral");
    let path = vs.write(&rec).unwrap();
    assert!(path.exists());
    vs.forget(&rec.id).unwrap();
    assert!(!path.exists());
    assert!(vs.get(&rec.id).is_err());
}

#[test]
fn invalid_scope_rejected() {
    assert!(Scope::parse("agent:hermes-dev").is_ok());
    assert!(Scope::parse("project:opentusk").is_ok());
    assert!(Scope::parse("user").is_ok());
    assert!(Scope::parse("org").is_ok());
    for bad in [
        "",
        "agent:",
        "project:",
        "team:x",
        "agent:../../etc",
        "agent:a/b",
        "project:has space",
        "user:x",
        "org:x",
        "project:*",
    ] {
        assert!(Scope::parse(bad).is_err(), "should reject {bad:?}");
    }
}

#[test]
fn scope_path_mapping() {
    assert_eq!(
        Scope::parse("agent:hermes-dev").unwrap().rel_path(),
        std::path::Path::new("agent/hermes-dev")
    );
    assert_eq!(
        Scope::parse("project:opentusk").unwrap().rel_path(),
        std::path::Path::new("project/opentusk")
    );
    assert_eq!(
        Scope::parse("user").unwrap().rel_path(),
        std::path::Path::new("user")
    );
    assert_eq!(
        Scope::parse("org").unwrap().rel_path(),
        std::path::Path::new("org")
    );
    assert_eq!(
        Scope::parse("project:opentusk").unwrap().dashed(),
        "project-opentusk"
    );
}

#[test]
fn update_telemetry_math() {
    let dir = tempfile::tempdir().unwrap();
    let (vs, clock) = store(dir.path());
    let rec = sample(&vs, "procedure body");
    vs.write(&rec).unwrap();
    clock.advance(Duration::seconds(5));
    let now = clock_now(&clock);
    vs.update_telemetry(&rec.id, 1, 1.0, now).unwrap();
    vs.update_telemetry(&rec.id, 1, 0.5, now).unwrap();
    let (_, got) = vs.get(&rec.id).unwrap();
    assert_eq!(got.uses, 2);
    assert!((got.successes - 1.5).abs() < 1e-9);
    assert_eq!(got.last_used, Some(now));
}

#[test]
fn walk_lists_all_records() {
    let dir = tempfile::tempdir().unwrap();
    let (vs, _) = store(dir.path());
    let a = sample(&vs, "one");
    let b = vs.new_record(
        RecordType::Episodic,
        Scope::parse("agent:hermes-dev").unwrap(),
        "hermes-dev",
        "two",
    );
    vs.write(&a).unwrap();
    vs.write(&b).unwrap();
    let all = vs.walk().unwrap();
    let ids: Vec<_> = all.iter().map(|(_, r)| r.id.clone()).collect();
    assert_eq!(all.len(), 2);
    assert!(ids.contains(&a.id) && ids.contains(&b.id));
}

// Round-trip property test over arbitrary safe strings (no NUL; frontmatter
// strings additionally exercise quoting/escaping paths).
proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn roundtrip_serialize_parse(
        body in "\\PC{0,400}",
        tag in "[a-zA-Z0-9 _:,\\[\\]\"'.-]{0,40}",
        entity in "\\PC{0,40}",
        trigger in "\\PC{0,80}",
        trust in 0.0f64..=1.0f64,
        uses in 0i64..1000,
    ) {
        let mut rec = Record::new_at(
            "01JZ9BLLPPQQRRSSTTUUVVWWXX".to_string(),
            RecordType::Skill,
            Scope::parse("project:opentusk").unwrap(),
            "tester",
            Utc.with_ymd_and_hms(2026, 1, 2, 3, 4, 5).unwrap(),
            body.clone(),
        );
        rec.tags = vec![tag];
        rec.entities = vec![entity];
        rec.trigger = Some(trigger);
        rec.trust = trust;
        rec.uses = uses;
        rec.successes = uses as f64 * 0.5;
        rec.version = Some(2);
        let text = rec.to_markdown().unwrap();
        let parsed = Record::from_markdown(&text).unwrap();
        prop_assert_eq!(rec, parsed);
    }
}
