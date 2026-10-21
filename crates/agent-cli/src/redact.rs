//! Redaction (ADR 0013 decisions 3/T4, ADR 0026): the values
//! that must never appear in anything this program prints or writes — every key in
//! `auth.json`, every `--header` value, and every value registered since startup.
//!
//! Split from `lib.rs` by pure move (ADR 0043 decision 5): no behaviour change.

use super::Cli;

/// The values that must never appear in an export: the keys in `auth.json` and every
/// `--header` value. Redaction itself is not optional (ADR 0013 decision 3). An export does
/// not know which provider wrote the session, so every usable key in the store is redacted —
/// and, through [`redaction_secrets`], every value registered since startup (a `chatgpt` token
/// refreshed mid-session, ADR 0026).
pub(crate) fn export_secrets(cli: &Cli) -> Vec<String> {
    // The shared rule: every resolvable key in `auth.json` (ADR 0013 T4), so the CLI and the
    // TUI redact the same set.
    let mut secrets = radian_host::resolved_keys();
    for header in &cli.header {
        if let Some((_, value)) = header.split_once('=')
            && !value.is_empty()
        {
            secrets.push(value.to_owned());
        }
    }
    secrets.retain(|secret| !secret.is_empty());
    redaction_secrets(&secrets)
}

/// A server may echo the key or a header value back in an error body; nothing this program
/// prints may contain them, so every error goes through here. The list is read through
/// [`redaction_secrets`], so a value registered after startup is covered too.
pub(crate) fn redact(text: &str, secrets: &[String]) -> String {
    let mut out = text.to_owned();
    for secret in redaction_secrets(secrets) {
        if !secret.is_empty() {
            out = out.replace(secret.as_str(), "<redacted>");
        }
    }
    out
}

/// The values to redact **now**: the list this run read at startup (every key in `auth.json`
/// and every `--header` value, ADR 0013 T4) plus everything `radian-host` has registered since.
/// A `chatgpt` access and refresh token refreshed mid-session is written and registered after
/// `resolved_keys()` was read (ADR 0026), so a copy taken at startup stops
/// covering the token the run is actually using — a `401` echoing it would then be printed
/// verbatim. Read at each redaction, never once.
fn redaction_secrets(startup: &[String]) -> Vec<String> {
    let mut secrets = startup.to_vec();
    for value in radian_host::registered_secrets() {
        if !value.is_empty() && !secrets.contains(&value) {
            secrets.push(value);
        }
    }
    secrets
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ADR 0026: a `chatgpt` access/refresh pair refreshed mid-session is
    /// written and registered **after** the startup list was read, so both CLI readers (the
    /// error path through `redact`, the export list) must carry it. The registry is
    /// process-global: the value here is unique to this test and it never asserts the registry
    /// is empty.
    #[test]
    fn the_error_and_export_lists_carry_a_value_registered_after_startup() {
        // `export_secrets` reads `resolved_keys()`, which opens `$RADIAN_HOME/credentials/
        // auth.json`: point that at a scratch directory so the test can never read the
        // real logins (AGENTS.md) or depend on this machine's.
        let home =
            std::env::temp_dir().join(format!("radian-cli-redaction-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        let previous = std::env::var_os("RADIAN_HOME");
        // SAFETY: `RADIAN_HOME` is read here and nowhere else in this test binary, and the
        // previous value is put back before the test ends.
        unsafe { std::env::set_var("RADIAN_HOME", &home) };

        let late = format!("late-cli-token-{}", std::process::id());
        // The mid-session refresh: `store_token` writes the pair and registers both values.
        radian_host::register_secret(&late);

        // The error list: `Setup::secrets`/`Run::secrets` were read before that refresh, so
        // what keeps the token out of this line is the read through `redaction_secrets`.
        let startup: Vec<String> = vec!["startup-api-key".to_string()];
        let error = redact(&format!("401 for Bearer {late}"), &startup);
        assert!(!error.contains(&late), "{error}");
        assert!(error.contains("<redacted>"), "{error}");

        // The export list: `--export` hands `radian_host::sessions::export` this one.
        let exported = export_secrets(&Cli::default());
        assert!(
            exported.contains(&late),
            "the export list must carry the registered token: {exported:?}"
        );

        match previous {
            Some(value) => unsafe { std::env::set_var("RADIAN_HOME", value) },
            None => unsafe { std::env::remove_var("RADIAN_HOME") },
        }
        let _ = std::fs::remove_dir_all(&home);
    }
}
