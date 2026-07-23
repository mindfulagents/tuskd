//! P3 exit tests — keyring & ACL (build-loop §2 P3).

use tusk_core::keyring::{Keyring, Verb};
use tusk_core::scope::Scope;

fn ring() -> (tempfile::TempDir, Keyring) {
    let dir = tempfile::tempdir().unwrap();
    let ring = Keyring::open(&dir.path().join("agents.json")).unwrap();
    (dir, ring)
}

fn s(x: &str) -> Scope {
    Scope::parse(x).unwrap()
}

#[test]
fn create_returns_token_and_pem_once_and_persists() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("agents.json");
    let ring = Keyring::open(&path).unwrap();
    let created = ring
        .create(
            "hermes-dev",
            &["project:opentusk", "user"],
            &[],
            &["project:opentusk"],
        )
        .unwrap();
    assert_eq!(created.id, "hermes-dev");
    assert!(!created.token.is_empty());
    assert!(created.private_key_pem.contains("PRIVATE KEY"));

    // Token/private key are not persisted — only hash + public key.
    let raw = std::fs::read_to_string(&path).unwrap();
    assert!(!raw.contains(&created.token));
    assert!(!raw.contains("PRIVATE KEY"));
    assert!(raw.contains("PUBLIC KEY"));

    // Reload from disk: agent still there, token still authenticates.
    let ring2 = Keyring::open(&path).unwrap();
    let agent = ring2.auth_by_token(&created.token).unwrap();
    assert_eq!(agent.id, "hermes-dev");

    // Duplicate id refused.
    assert!(ring.create("hermes-dev", &[], &[], &[]).is_err());
}

#[test]
fn wildcard_matching_table() {
    let (_d, ring) = ring();
    ring.create("a", &["project:*"], &["project:opentusk"], &[])
        .unwrap();
    // project:* matches any project, not agent scopes.
    assert!(ring.can("a", Verb::Read, &s("project:x")).unwrap());
    assert!(ring.can("a", Verb::Read, &s("project:opentusk")).unwrap());
    assert!(!ring.can("a", Verb::Read, &s("agent:x")).unwrap());
    assert!(!ring.can("a", Verb::Read, &s("user")).unwrap());
    assert!(!ring.can("a", Verb::Read, &s("org")).unwrap());
    // Exact write grant does not leak to other projects.
    assert!(ring.can("a", Verb::Write, &s("project:opentusk")).unwrap());
    assert!(!ring.can("a", Verb::Write, &s("project:x")).unwrap());
    // No promote grants at all.
    assert!(!ring
        .can("a", Verb::Promote, &s("project:opentusk"))
        .unwrap());
}

#[test]
fn own_scope_implicit_rights_present_even_with_empty_grants() {
    let (_d, ring) = ring();
    ring.create("solo", &[], &[], &[]).unwrap();
    assert!(ring.can("solo", Verb::Read, &s("agent:solo")).unwrap());
    assert!(ring.can("solo", Verb::Write, &s("agent:solo")).unwrap());
    // But not promote, and not other agents' scopes.
    assert!(!ring.can("solo", Verb::Promote, &s("agent:solo")).unwrap());
    assert!(!ring.can("solo", Verb::Read, &s("agent:other")).unwrap());
}

#[test]
fn revoked_agent_fails_auth_and_all_can() {
    let (_d, ring) = ring();
    let created = ring.create("gone", &["project:*"], &[], &[]).unwrap();
    assert!(ring.auth_by_token(&created.token).is_ok());
    ring.revoke("gone").unwrap();
    assert!(ring.auth_by_token(&created.token).is_err());
    assert!(!ring.can("gone", Verb::Read, &s("project:x")).unwrap());
    assert!(!ring.can("gone", Verb::Read, &s("agent:gone")).unwrap());
}

#[test]
fn unknown_agent_and_bad_token() {
    let (_d, ring) = ring();
    ring.create("a", &[], &[], &[]).unwrap();
    assert!(ring.auth_by_token("not-a-token").is_err());
    assert!(ring.get("nobody").unwrap().is_none());
    assert!(!ring.can("nobody", Verb::Read, &s("user")).unwrap());
}

#[test]
fn grant_adds_and_persists() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("agents.json");
    let ring = Keyring::open(&path).unwrap();
    ring.create("a", &[], &[], &[]).unwrap();
    assert!(!ring.can("a", Verb::Promote, &s("org")).unwrap());
    ring.grant("a", Verb::Promote, "org").unwrap();
    assert!(ring.can("a", Verb::Promote, &s("org")).unwrap());
    let ring2 = Keyring::open(&path).unwrap();
    assert!(ring2.can("a", Verb::Promote, &s("org")).unwrap());
    // Granting to unknown agent fails; bad scope pattern fails.
    assert!(ring.grant("nobody", Verb::Read, "user").is_err());
    assert!(ring.grant("a", Verb::Read, "bogus:scope").is_err());
}

/// Structural constant-time requirement: token auth must go through `subtle`'s
/// constant-time equality on the sha256 digest (build-loop §2 P3). This test
/// pins the digest comparison helper so a refactor to `==` breaks it.
#[test]
fn token_compare_is_constant_time_helper() {
    let a = [0u8; 32];
    let mut b = [0u8; 32];
    assert!(tusk_core::keyring::ct_eq_digest(&a, &b));
    b[31] = 1;
    assert!(!tusk_core::keyring::ct_eq_digest(&a, &b));
}
