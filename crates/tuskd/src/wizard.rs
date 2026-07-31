//! `tuskd setup` — the guided onboarding wizard (D36).
//!
//! A checklist runner, not a questionnaire: every run derives the todo
//! list from on-disk state (vault? daemon? session? connection?
//! approval?) and only asks about steps that are actually missing. There
//! is no wizard state file, which makes the wizard resumable (Ctrl-C and
//! rerun), a repair tool (rerun lands on the broken step), and safe on a
//! healthy install. Nothing here deletes or revokes anything.
//!
//! Every step it executes prints the equivalent plain command in dim
//! text — the wizard teaches the CLI rather than hiding it. Scripts keep
//! using the granular verbs: `setup` refuses to run without a terminal.

use crate::style::{ACCENT, DIM, ERR, OK};
use std::path::Path;
use tusk_core::error::CoreError;

/// Interactive questions, behind a trait so tests can script answers.
pub trait Prompt {
    /// Free-form line; empty input returns `default` when given.
    fn line(&mut self, question: &str, default: Option<&str>) -> Result<String, CoreError>;
    /// Yes/no; empty input returns `default_yes`.
    fn confirm(&mut self, question: &str, default_yes: bool) -> Result<bool, CoreError>;
    /// Numbered menu; returns the chosen index (empty input → 0).
    fn select(&mut self, options: &[&str]) -> Result<usize, CoreError>;
}

/// Stdin/stdout prompt for real runs.
pub struct StdPrompt;

impl StdPrompt {
    fn read_line() -> Result<String, CoreError> {
        use std::io::Write;
        std::io::stdout()
            .flush()
            .map_err(|e| CoreError::Other(format!("stdout: {e}")))?;
        let mut line = String::new();
        let n = std::io::stdin()
            .read_line(&mut line)
            .map_err(|e| CoreError::Other(format!("stdin: {e}")))?;
        if n == 0 {
            return Err(CoreError::Other("stdin closed — aborting setup".into()));
        }
        Ok(line.trim().to_string())
    }
}

impl Prompt for StdPrompt {
    fn line(&mut self, question: &str, default: Option<&str>) -> Result<String, CoreError> {
        match default {
            Some(d) => anstream::print!("{question} {DIM}[{d}]{DIM:#}: "),
            None => anstream::print!("{question}: "),
        }
        let raw = Self::read_line()?;
        Ok(if raw.is_empty() {
            default.unwrap_or("").to_string()
        } else {
            raw
        })
    }

    fn confirm(&mut self, question: &str, default_yes: bool) -> Result<bool, CoreError> {
        let hint = if default_yes { "[Y/n]" } else { "[y/N]" };
        loop {
            anstream::print!("{question} {DIM}{hint}{DIM:#} ");
            match Self::read_line()?.to_lowercase().as_str() {
                "" => return Ok(default_yes),
                "y" | "yes" => return Ok(true),
                "n" | "no" => return Ok(false),
                _ => anstream::println!("{DIM}please answer y or n{DIM:#}"),
            }
        }
    }

    fn select(&mut self, options: &[&str]) -> Result<usize, CoreError> {
        for (i, option) in options.iter().enumerate() {
            anstream::println!("  {ACCENT}{}{ACCENT:#}) {option}", i + 1);
        }
        loop {
            anstream::print!("choice {DIM}[1]{DIM:#}: ");
            let raw = Self::read_line()?;
            if raw.is_empty() {
                return Ok(0);
            }
            match raw.parse::<usize>() {
                Ok(n) if (1..=options.len()).contains(&n) => return Ok(n - 1),
                _ => {
                    anstream::println!("{DIM}pick a number between 1 and {}{DIM:#}", options.len())
                }
            }
        }
    }
}

/// Dim echo of the plain command a wizard step just ran.
fn ran(command: &str) {
    crate::style::hint(&format!("  ran: {command}"));
}

fn step(title: &str) {
    anstream::println!("\n{ACCENT}▸ {title}{ACCENT:#}");
}

/// `tuskd setup`. The caller (commands.rs) guarantees a terminal.
pub fn run(vault: &Path, prompt: &mut dyn Prompt) -> Result<(), CoreError> {
    anstream::println!(
        "{ACCENT}tuskd setup{ACCENT:#} — vault {DIM}{}{DIM:#}",
        crate::style::display_path(vault)
    );

    ensure_vault(vault, prompt)?;
    ensure_daemon(vault, prompt)?;
    offer_clients(vault, prompt)?;
    cloud(vault, prompt)?;

    step("all set");
    anstream::println!(
        "check any time with {ACCENT}tuskd status{ACCENT:#}; rerun {ACCENT}tuskd setup{ACCENT:#} \
         to repair or extend this setup"
    );
    Ok(())
}

fn ensure_vault(vault: &Path, prompt: &mut dyn Prompt) -> Result<(), CoreError> {
    step("vault");
    if vault.join(".tusk").is_dir() {
        anstream::println!("{OK}✓{OK:#} vault exists");
        return Ok(());
    }
    if !prompt.confirm(
        &format!("create a vault at {}?", crate::style::display_path(vault)),
        true,
    )? {
        return Err(CoreError::Other(
            "setup needs a vault — rerun with --vault <dir> to pick another spot".into(),
        ));
    }
    crate::commands::init(vault)?;
    ran("tuskd init");
    Ok(())
}

fn daemon_running(vault: &Path) -> bool {
    crate::config::load(vault)
        .map(|cfg| std::os::unix::net::UnixStream::connect(&cfg.uds_path).is_ok())
        .unwrap_or(false)
}

fn ensure_daemon(vault: &Path, prompt: &mut dyn Prompt) -> Result<(), CoreError> {
    step("daemon");
    if daemon_running(vault) {
        anstream::println!("{OK}✓{OK:#} daemon running");
        return Ok(());
    }
    if prompt.confirm(
        "start the daemon in the background? (it indexes, watches, and auto-syncs)",
        true,
    )? {
        crate::commands::start_detached(vault)?;
        ran("tuskd start -d");
    } else {
        crate::style::hint("  skipped — start later with: tuskd start -d");
    }
    Ok(())
}

fn offer_clients(vault: &Path, prompt: &mut dyn Prompt) -> Result<(), CoreError> {
    step("AI clients");
    let clients = crate::setup::detect_clients();
    let mut offered = false;
    for client in clients {
        if client.configured {
            anstream::println!("{OK}✓{OK:#} {} already configured", client.name);
            continue;
        }
        if !client.installed {
            continue;
        }
        offered = true;
        if prompt.confirm(
            &format!("configure {} to use this vault?", client.name),
            true,
        )? {
            crate::setup::run(
                vault,
                crate::setup::SetupArgs {
                    client: Some(client.name.to_string()),
                    agent: None,
                    http: false,
                    print: false,
                    remove: false,
                    yes: true,
                },
            )?;
            ran(&format!("tuskd agent setup {}", client.name));
        }
    }
    if !offered {
        crate::style::hint("  no unconfigured AI clients detected — see: tuskd agent setup list");
    }
    Ok(())
}

fn cloud(vault: &Path, prompt: &mut dyn Prompt) -> Result<(), CoreError> {
    step("cloud sync");
    if let Some(config) = crate::sync_cloud::connection(vault) {
        anstream::println!(
            "{OK}✓{OK:#} connected to repo {DIM}{}{DIM:#} on {}",
            config.repo_id,
            config.url
        );
        return repair_connection(vault, prompt);
    }
    if !prompt.confirm(
        "sync this vault to OpenTusk Cloud? (end-to-end encrypted; free plan: 1 repo)",
        false,
    )? {
        crate::style::hint("  staying local-only — rerun tuskd setup any time to enable sync");
        return Ok(());
    }
    ensure_session(vault, prompt)?;
    match prompt.select(&[
        "create a new cloud repo for this vault",
        "join an existing repo (this is a second device)",
    ])? {
        0 => create_repo(vault, prompt),
        _ => join_repo(vault, prompt),
    }
}

fn ensure_session(vault: &Path, prompt: &mut dyn Prompt) -> Result<(), CoreError> {
    if let Some(email) = crate::sync_cloud::session_email(vault) {
        anstream::println!("{OK}✓{OK:#} signed in as {email}");
        return Ok(());
    }
    let email = prompt.line("account email (a sign-in code will be emailed)", None)?;
    if email.is_empty() {
        return Err(CoreError::Other("an email address is required".into()));
    }
    crate::sync_cloud::login(vault, crate::sync_cloud::DEFAULT_CLOUD_URL, &email, None)?;
    ran(&format!("tuskd sync login --email {email}"));
    Ok(())
}

fn create_repo(vault: &Path, prompt: &mut dyn Prompt) -> Result<(), CoreError> {
    match crate::sync_cloud::init(vault, None, None) {
        Ok(()) => {
            ran("tuskd sync init");
            phrase_gate(prompt)?;
            if daemon_running(vault) {
                anstream::println!(
                    "{OK}✓{OK:#} the running daemon will push this vault automatically"
                );
            } else if prompt.confirm("push the vault now?", true)? {
                crate::sync_cloud::push(vault)?;
                ran("tuskd sync push");
            }
            Ok(())
        }
        // The plan-cap refusal (403) deserves choices, not a dead end. The
        // sync_cloud error is a plain message; match its stable prefix.
        Err(e) if e.to_string().contains("repo limit reached") => {
            anstream::println!("{ERR}✗{ERR:#} {e}");
            anstream::println!("your account's repos:");
            crate::sync_cloud::repos(vault)?;
            match prompt.select(&[
                "join one of those repos instead (connect this vault to it)",
                "stop here (free a slot with `tuskd sync delete-repo <id> --yes`, then rerun)",
            ])? {
                0 => join_repo(vault, prompt),
                _ => Ok(()),
            }
        }
        Err(e) => Err(e),
    }
}

/// The phrase is unrecoverable once this screen is gone — insist on a
/// typed word, not an easily-muscle-memoried Enter.
fn phrase_gate(prompt: &mut dyn Prompt) -> Result<(), CoreError> {
    loop {
        let answer = prompt.line(
            "type `saved` once the recovery phrase is written down",
            None,
        )?;
        if answer.eq_ignore_ascii_case("saved") {
            return Ok(());
        }
        anstream::println!(
            "{ERR}the phrase above is shown exactly once — save it before continuing{ERR:#}"
        );
    }
}

fn join_repo(vault: &Path, prompt: &mut dyn Prompt) -> Result<(), CoreError> {
    let repos = crate::sync_cloud::account_repos(vault)?;
    let repo_id = if repos.is_empty() {
        let id = prompt.line(
            "no repos on this account — repo id to join (from the repo owner)",
            None,
        )?;
        if id.is_empty() {
            return Err(CoreError::Other("a repo id is required to join".into()));
        }
        id
    } else {
        let labels: Vec<String> = repos
            .iter()
            .map(|r| format!("{}  {DIM}{}{DIM:#}", r.name, r.repo_id))
            .collect();
        let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
        repos[prompt.select(&refs)?].repo_id.clone()
    };
    let phrase = prompt.line(
        "24-word recovery phrase (Enter to wait for a device approval instead)",
        Some(""),
    )?;
    let phrase = if phrase.trim().is_empty() {
        None
    } else {
        Some(phrase.trim().to_string())
    };
    let had_phrase = phrase.is_some();
    crate::sync_cloud::connect(vault, None, &repo_id, None, phrase)?;
    ran(&format!("tuskd sync connect --repo {repo_id}"));
    if !had_phrase {
        wait_for_approval(vault)?;
    }
    match crate::sync_cloud::pull(vault) {
        Ok(()) => ran("tuskd sync pull"),
        Err(e) if e.to_string().contains("nothing pushed yet") => {
            crate::style::hint("  nothing to pull yet — the repo has no snapshot");
        }
        Err(e) => return Err(e),
    }
    Ok(())
}

/// Poll until another device approves this one. Ctrl-C is safe: every
/// step before this is persisted, and rerunning `tuskd setup` resumes
/// right here.
fn wait_for_approval(vault: &Path) -> Result<(), CoreError> {
    use tusk_sync::SyncError;
    anstream::println!(
        "{DIM}waiting for approval — Ctrl-C is safe; rerun tuskd setup to resume{DIM:#}"
    );
    loop {
        let (cloud, _) = crate::sync_cloud::client(vault)?;
        match cloud.fetch_wrap() {
            Ok(_) => {
                anstream::println!("{OK}✓{OK:#} approved");
                return Ok(());
            }
            Err(SyncError::Http { status: 403, .. }) => {
                anstream::eprint!("{DIM}.{DIM:#}");
                std::thread::sleep(std::time::Duration::from_secs(5));
            }
            Err(SyncError::Http { status: 404, .. }) => {
                // Approved before any wrap existed (fresh repo).
                anstream::println!("{OK}✓{OK:#} approved");
                return Ok(());
            }
            Err(e) => return Err(crate::sync_cloud::err(e)),
        }
    }
}

/// The repair half: an existing connection that is not fully healthy.
fn repair_connection(vault: &Path, prompt: &mut dyn Prompt) -> Result<(), CoreError> {
    use tusk_sync::SyncError;
    let (cloud, _) = crate::sync_cloud::client(vault)?;
    match cloud.fetch_wrap() {
        Ok(_) => {
            anstream::println!("{OK}✓{OK:#} device approved");
            Ok(())
        }
        Err(SyncError::Http { status: 403, .. }) => {
            anstream::println!("{ACCENT}this device is still pending approval{ACCENT:#}");
            crate::sync_cloud::status(vault)?;
            if prompt.confirm("wait for approval now?", true)? {
                wait_for_approval(vault)?;
            }
            Ok(())
        }
        Err(SyncError::Http { status: 404, .. }) => {
            anstream::println!("{OK}✓{OK:#} device approved");
            Ok(())
        }
        Err(e) => {
            anstream::println!("{ERR}✗{ERR:#} cloud unreachable: {e}");
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    /// Scripted prompt: pops pre-baked answers, records questions.
    struct FakePrompt {
        answers: VecDeque<&'static str>,
        asked: Vec<String>,
    }

    impl FakePrompt {
        fn new(answers: &[&'static str]) -> FakePrompt {
            FakePrompt {
                answers: answers.iter().copied().collect(),
                asked: Vec::new(),
            }
        }
        fn pop(&mut self, question: &str) -> String {
            self.asked.push(question.to_string());
            self.answers
                .pop_front()
                .unwrap_or_else(|| panic!("unscripted question: {question}"))
                .to_string()
        }
    }

    impl Prompt for FakePrompt {
        fn line(&mut self, question: &str, default: Option<&str>) -> Result<String, CoreError> {
            let raw = self.pop(question);
            Ok(if raw.is_empty() {
                default.unwrap_or("").to_string()
            } else {
                raw
            })
        }
        fn confirm(&mut self, question: &str, default_yes: bool) -> Result<bool, CoreError> {
            match self.pop(question).as_str() {
                "" => Ok(default_yes),
                "y" => Ok(true),
                _ => Ok(false),
            }
        }
        fn select(&mut self, options: &[&str]) -> Result<usize, CoreError> {
            let raw = self.pop(&options.join("|"));
            Ok(raw.parse().unwrap_or(0))
        }
    }

    #[test]
    fn vault_step_creates_on_yes_and_skips_when_present() {
        let dir = tempfile::tempdir().unwrap();
        let vault = dir.path().join("v");
        let mut prompt = FakePrompt::new(&["y"]);
        ensure_vault(&vault, &mut prompt).unwrap();
        assert!(vault.join(".tusk").is_dir());
        // Second run asks nothing.
        let mut prompt = FakePrompt::new(&[]);
        ensure_vault(&vault, &mut prompt).unwrap();
        assert!(prompt.asked.is_empty());
    }

    #[test]
    fn vault_step_declining_is_an_error_not_a_crash() {
        let dir = tempfile::tempdir().unwrap();
        let vault = dir.path().join("v");
        let mut prompt = FakePrompt::new(&["n"]);
        let err = ensure_vault(&vault, &mut prompt).unwrap_err();
        assert!(err.to_string().contains("--vault"));
        assert!(!vault.exists());
    }

    #[test]
    fn phrase_gate_insists_on_saved() {
        let mut prompt = FakePrompt::new(&["", "yes", "SAVED"]);
        phrase_gate(&mut prompt).unwrap();
        assert_eq!(prompt.asked.len(), 3);
    }

    #[test]
    fn cloud_step_respects_local_only_choice() {
        let dir = tempfile::tempdir().unwrap();
        let vault = dir.path().join("v");
        let mut prompt = FakePrompt::new(&["y"]);
        ensure_vault(&vault, &mut prompt).unwrap();
        // Decline cloud: no session, no connection, no further questions.
        let mut prompt = FakePrompt::new(&["n"]);
        cloud(&vault, &mut prompt).unwrap();
        assert_eq!(prompt.asked.len(), 1);
        assert!(crate::sync_cloud::session_email(&vault).is_none());
        assert!(crate::sync_cloud::connection(&vault).is_none());
    }

    #[test]
    fn daemon_step_skip_leaves_no_daemon() {
        let dir = tempfile::tempdir().unwrap();
        let vault = dir.path().join("v");
        let mut prompt = FakePrompt::new(&["y"]);
        ensure_vault(&vault, &mut prompt).unwrap();
        let mut prompt = FakePrompt::new(&["n"]);
        ensure_daemon(&vault, &mut prompt).unwrap();
        assert!(!daemon_running(&vault));
    }
}
