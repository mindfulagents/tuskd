//! Terminal styling for human-facing CLI output (D35).
//!
//! Semantics over decoration: one brand accent plus three states, mapped
//! from the opentusk.ai palette (gold `#dca85e`, sage `#79a88d`). All
//! styled output flows through `anstream`, which strips escapes when
//! stdout/stderr is not a terminal or `NO_COLOR` is set — piped output is
//! byte-identical to the unstyled text. Strings meant to be copied
//! (tokens, phrases, URLs, JSON) must never carry styles.

use anstyle::{Ansi256Color, AnsiColor, Style};

/// Brand gold — headers and key values.
pub const ACCENT: Style = Style::new()
    .fg_color(Some(anstyle::Color::Ansi256(Ansi256Color(178))))
    .bold();
/// Sage — ✓ success states.
pub const OK: Style = Style::new().fg_color(Some(anstyle::Color::Ansi256(Ansi256Color(108))));
/// Red — ✗ errors and shown-once warnings.
pub const ERR: Style = Style::new()
    .fg_color(Some(anstyle::Color::Ansi(AnsiColor::Red)))
    .bold();
/// Dim — paths, timestamps, hints, metadata.
pub const DIM: Style = Style::new().dimmed();
/// Bold — emphasis without color (matched search terms, table headers).
pub const BOLD: Style = Style::new().bold();

/// Whether stdout is a terminal: the switch between human panels and the
/// machine (JSON) views. Color stripping is separate and automatic.
pub fn stdout_is_tty() -> bool {
    use std::io::IsTerminal;
    std::io::stdout().is_terminal()
}

/// Dim single-line hint, e.g. `next: tuskd start -d`.
pub fn hint(text: &str) {
    anstream::println!("{DIM}{text}{DIM:#}");
}

/// clap `--help` styling: brand gold headers, bold literals.
pub const HELP_STYLES: clap::builder::Styles = clap::builder::Styles::styled()
    .header(ACCENT)
    .usage(ACCENT)
    .literal(BOLD)
    .placeholder(DIM);

/// `1234567` → `1,234,567`.
pub fn group_thousands(n: i64) -> String {
    let raw = n.abs().to_string();
    let mut out = String::new();
    for (i, c) in raw.chars().enumerate() {
        if i > 0 && (raw.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    if n < 0 {
        format!("-{out}")
    } else {
        out
    }
}

/// `1234` → `1.2 KB` (decimal units, one fraction digit below 10).
pub fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else if value < 10.0 {
        format!("{value:.1} {}", UNITS[unit])
    } else {
        format!("{value:.0} {}", UNITS[unit])
    }
}

/// Seconds-ago → `just now` / `5m ago` / `3h ago` / `2d ago`.
pub fn human_ago(secs: u64) -> String {
    match secs {
        0..=59 => "just now".into(),
        60..=3599 => format!("{}m ago", secs / 60),
        3600..=86_399 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86_400),
    }
}

/// `$HOME`-relative display form of a path.
pub fn display_path(path: &std::path::Path) -> String {
    let shown = path.display().to_string();
    if let Some(home) = std::env::var_os("HOME") {
        let home = home.to_string_lossy();
        if !home.is_empty() {
            if let Some(rest) = shown.strip_prefix(home.as_ref()) {
                if rest.is_empty() {
                    return "~".into();
                }
                if rest.starts_with('/') {
                    return format!("~{rest}");
                }
            }
        }
    }
    shown
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thousands_are_grouped() {
        assert_eq!(group_thousands(0), "0");
        assert_eq!(group_thousands(999), "999");
        assert_eq!(group_thousands(1000), "1,000");
        assert_eq!(group_thousands(1_234_567), "1,234,567");
        assert_eq!(group_thousands(-4321), "-4,321");
    }

    #[test]
    fn bytes_humanize() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(999), "999 B");
        assert_eq!(human_bytes(1234), "1.2 KB");
        assert_eq!(human_bytes(52_400_000), "52 MB");
        assert_eq!(human_bytes(1_260_000_000), "1.3 GB");
    }

    #[test]
    fn ago_humanizes() {
        assert_eq!(human_ago(5), "just now");
        assert_eq!(human_ago(120), "2m ago");
        assert_eq!(human_ago(7200), "2h ago");
        assert_eq!(human_ago(200_000), "2d ago");
    }

    #[test]
    fn home_collapses_to_tilde() {
        let home = std::env::var("HOME").unwrap();
        assert_eq!(
            display_path(std::path::Path::new(&format!("{home}/notes"))),
            "~/notes"
        );
        assert_eq!(display_path(std::path::Path::new("/tmp/x")), "/tmp/x");
    }
}
