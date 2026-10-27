//! `radian login` / `radian logout` (ADR 0026 D3): the CLI side of the credential wizard.
//!
//! The steps mirror the TUI's `/login`: a built-in provider asks for its key; a custom one
//! (D10) asks for the name, the protocol, the base URL and then the key — all of it into
//! **one** `auth.json` entry. The key is read with the terminal's echo **off**, and the echo
//! is restored on every path, Ctrl-C included: the signals that would kill the process are
//! blocked across the echo-off window (a pending Ctrl-C is delivered right after the
//! restore, with the default handler), and a guard restores on any other early exit.
//!
//! Without a terminal the command refuses (a key pasted into a pipe would be visible in
//! `ps`/history); the message points at `auth.json` for that case. Only `auth.json` is
//! written — never the config file (D3).

use std::collections::BTreeMap;
use std::io::Write;

use crate::USAGE_ERROR;

/// A unix-only module (the terminal handling is termios). On other platforms the command
/// refuses with the same "needs a terminal" message as a non-tty unix run.
#[cfg(unix)]
mod termios {
    use std::os::fd::AsRawFd;

    /// One echo-off window: the saved settings, restored on drop, over a `/dev/tty` the guard
    /// owns (a temporary `File` made from the raw fd would close it out from under the read).
    /// Signals that would end the process are blocked while the window is open, so a Ctrl-C
    /// during the key prompt is delivered *after* [`Drop::drop`] has restored the echo —
    /// with the default handler.
    pub struct EchoGuard {
        tty: std::fs::File,
        saved: libc::termios,
    }

    impl EchoGuard {
        /// Opens the terminal (`/dev/tty`; when there is no controlling terminal but stdin is
        /// a terminal — a pty test, an ssh pipe setup — stdin itself), turns the echo off,
        /// and blocks the terminating signals. The caller must call
        /// [`Self::unblock_signals`] (the pending Ctrl-C is what ends the process afterwards,
        /// as the user asked).
        pub fn new() -> Result<Self, String> {
            use std::os::fd::AsRawFd;
            let tty = match std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open("/dev/tty")
            {
                Ok(tty) => tty,
                Err(_) if unsafe { libc::isatty(0) } == 1 => {
                    // SAFETY: fd 0 stays owned by the process; this dup is a fresh fd the
                    // guard owns.
                    let dup = unsafe { libc::dup(0) };
                    if dup < 0 {
                        return Err("no terminal for the key prompt".to_owned());
                    }
                    // SAFETY: dup is a fresh owned fd.
                    unsafe { std::os::fd::FromRawFd::from_raw_fd(dup) }
                }
                Err(error) => return Err(format!("cannot open /dev/tty: {error}")),
            };
            let fd = tty.as_raw_fd();
            let mut saved = unsafe { std::mem::zeroed::<libc::termios>() };
            if unsafe { libc::tcgetattr(fd, &mut saved) } != 0 {
                let error = std::io::Error::last_os_error();
                return Err(format!("cannot read the terminal settings: {error}"));
            }
            let mut raw = saved;
            unsafe { libc::cfmakeraw(&mut raw) };
            // Raw mode disables output processing too; keep the terminal's own output flags
            // (ONLCR) so prompts keep their newlines. (libc names the field `c_oflag`.)
            raw.c_oflag = saved.c_oflag;
            if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) } != 0 {
                let error = std::io::Error::last_os_error();
                return Err(format!("cannot turn the echo off: {error}"));
            }
            Self::block_signals();
            Ok(Self { tty, saved })
        }

        /// Restores the saved settings right now (idempotent).
        pub fn restore(&mut self) {
            unsafe { libc::tcsetattr(self.tty.as_raw_fd(), libc::TCSANOW, &self.saved) };
        }

        /// The terminal, for the prompt and the read.
        pub fn tty(&mut self) -> &mut std::fs::File {
            &mut self.tty
        }

        /// Un-blocks the deferred signals (SIGINT/SIGTERM/SIGHUP): a Ctrl-C typed during the
        /// echo-off window ends the process here, with the echo already restored.
        pub fn unblock_signals(&self) {
            unsafe {
                let mut empty: libc::sigset_t = std::mem::zeroed();
                libc::sigemptyset(&mut empty);
                libc::pthread_sigmask(libc::SIG_SETMASK, &empty, std::ptr::null_mut());
            }
        }

        fn block_signals() {
            unsafe {
                let mut set: libc::sigset_t = std::mem::zeroed();
                libc::sigemptyset(&mut set);
                for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
                    libc::sigaddset(&mut set, signal);
                }
                libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
            }
        }
    }

    impl Drop for EchoGuard {
        fn drop(&mut self) {
            self.restore();
        }
    }
}

/// Whether the process has a terminal on stdin and stdout (the login is interactive).
fn is_tty() -> bool {
    #[cfg(unix)]
    {
        unsafe { libc::isatty(0) == 1 && libc::isatty(1) == 1 }
    }
    #[cfg(not(unix))]
    {
        false
    }
}

/// Reads one line from the terminal, with the echo **off** (D3: the key must not be visible,
/// typed or pasted). The prompt goes to the same terminal. The echo is restored before the
/// answer returns, and a Ctrl-C during the read ends the process after the restore.
#[cfg(unix)]
fn read_hidden(prompt: &str) -> Result<String, String> {
    use std::io::Write;
    let mut guard = termios::EchoGuard::new()?;
    write!(guard.tty(), "{prompt}").map_err(|error| format!("cannot write the prompt: {error}"))?;
    guard
        .tty()
        .flush()
        .map_err(|error| format!("cannot write the prompt: {error}"))?;
    let mut line = String::new();
    let mut one = [0u8; 1];
    loop {
        use std::io::Read;
        match guard.tty().read(&mut one) {
            Ok(0) => break, // EOF: treat as Enter
            Ok(_) => {}
            Err(error) => return Err(format!("cannot read: {error}")),
        }
        let byte = one[0];
        if byte == b'\n' || byte == b'\r' {
            break;
        }
        if byte == 3 {
            // Ctrl-C in raw mode arrives as a byte, not a signal: restore, unblock, and end
            // like the default handler would.
            guard.restore();
            guard.unblock_signals();
            std::process::exit(130);
        }
        if byte == 0x7f || byte == 8 {
            line.pop();
            continue;
        }
        if byte >= 0x20 {
            line.push(byte as char);
        }
    }
    guard.restore();
    guard.unblock_signals();
    Ok(line)
}

/// Reads one visible line (the name, the URL — echo stays on). The same terminal choice as
/// the hidden read: `/dev/tty`, or stdin when it is a terminal.
#[cfg(unix)]
fn read_line(prompt: &str) -> Result<String, String> {
    use std::io::BufRead;
    let tty = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
    {
        Ok(tty) => tty,
        Err(_) if unsafe { libc::isatty(0) } == 1 => {
            // SAFETY: fd 0 stays owned by the process; this dup is a fresh fd.
            let dup = unsafe { libc::dup(0) };
            if dup < 0 {
                return Err("no terminal for the prompt".to_owned());
            }
            // SAFETY: dup is a fresh owned fd.
            unsafe { std::os::fd::FromRawFd::from_raw_fd(dup) }
        }
        Err(error) => return Err(format!("cannot open /dev/tty: {error}")),
    };
    let mut tty = tty;
    write!(tty, "{prompt}").map_err(|error| format!("cannot write the prompt: {error}"))?;
    tty.flush()
        .map_err(|error| format!("cannot write the prompt: {error}"))?;
    let mut line = String::new();
    std::io::BufReader::new(tty)
        .read_line(&mut line)
        .map_err(|error| format!("cannot read: {error}"))?;
    Ok(line.trim_end_matches(['\r', '\n']).to_owned())
}

/// The exit code of an aborted login (a refusal or a cancel): usage-shaped, not a model
/// failure.
fn aborted(message: String) -> i32 {
    eprintln!("radian: {message}");
    USAGE_ERROR
}

/// `radian login [provider]` (ADR 0026 D3). Without a terminal it refuses and points at
/// `auth.json`; the key is never echoed.
pub async fn run_login(provider: Option<String>) -> i32 {
    // The two ChatGPT OAuth choices (ADR 0026) never ask for a typed secret, so
    // they skip the "needs a terminal" gate below — that gate exists only because a key
    // pasted into a pipe would be visible (D3), which does not apply here.
    match provider.as_deref() {
        Some(id) if id == radian_host::CHATGPT_PROVIDER_ID => {
            return run_chatgpt_browser_login().await;
        }
        Some(id) if id == radian_host::CHATGPT_DEVICE_LOGIN => {
            return run_chatgpt_device_login().await;
        }
        _ => {}
    }
    if !is_tty() {
        return aborted(
            "login needs a terminal (a key in a pipe would be visible); edit \
             ~/.radian/credentials/auth.json instead — one JSON object, the provider id as \
             the key, e.g. {\"openai\": {\"type\": \"api_key\", \"key\": \"…\"}}"
                .to_owned(),
        );
    }
    let (name, kind, base_url) = match provider.as_deref() {
        // `custom` skips the list and goes straight to the custom flow (D10), like the
        // TUI's `/login custom`.
        Some(id) if id == radian_host::CUSTOM_LOGIN => {
            let (name, kind, base_url) = match ask_custom_steps() {
                Ok(steps) => steps,
                Err(code) => return code,
            };
            (name, kind, base_url)
        }
        Some(id) => match radian_host::descriptor(id) {
            Some(descriptor) => {
                println!("logging in to {} ({id})", descriptor.display_name);
                (
                    id.to_owned(),
                    descriptor.default_protocol.kind().to_owned(),
                    descriptor.default_base_url.to_owned(),
                )
            }
            None => {
                return aborted(format!(
                    "unknown provider {id:?}: choose one of {} (or `custom`)",
                    radian_host::descriptors()
                        .iter()
                        .map(|entry| entry.id)
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
        },
        None => {
            // The choice list: built-ins + custom (D10's named providers are born here);
            // shared with the TUI's wizard so the two cannot drift.
            println!("choose a provider:");
            let choices = radian_host::login_choices();
            for (index, (id, description)) in choices.iter().enumerate() {
                println!("  {}. {id:<20} {description}", index + 1);
            }
            let answer = match read_line("number or name: ") {
                Ok(answer) => answer,
                Err(error) => return aborted(error),
            };
            let picked = match answer.trim().parse::<usize>() {
                Ok(index) if (1..=choices.len()).contains(&index) => choices[index - 1].0.clone(),
                _ => answer.trim().to_owned(),
            };
            if picked == radian_host::CHATGPT_PROVIDER_ID {
                return run_chatgpt_browser_login().await;
            }
            if picked == radian_host::CHATGPT_DEVICE_LOGIN {
                return run_chatgpt_device_login().await;
            }
            if picked == radian_host::CUSTOM_LOGIN {
                match ask_custom_steps() {
                    Ok(steps) => steps,
                    Err(code) => return code,
                }
            } else {
                match radian_host::descriptor(&picked) {
                    Some(descriptor) => (
                        descriptor.id.to_owned(),
                        descriptor.default_protocol.kind().to_owned(),
                        descriptor.default_base_url.to_owned(),
                    ),
                    None => {
                        return aborted(format!(
                            "unknown provider {picked:?}: choose one of {} (or `custom`)",
                            radian_host::descriptors()
                                .iter()
                                .map(|entry| entry.id)
                                .collect::<Vec<_>>()
                                .join(", ")
                        ));
                    }
                }
            }
        }
    };

    // The key: echo off, typed or pasted alike (D3).
    let key = match read_hidden("API key (hidden; Enter to submit): ") {
        Ok(key) => key.trim().to_owned(),
        Err(error) if error == "cancelled" => {
            println!("login cancelled; nothing was saved");
            return 0;
        }
        Err(error) => return aborted(error),
    };
    if key.is_empty() {
        return aborted("empty key; nothing was saved".to_owned());
    }

    // The verification request (D3): one GET of the descriptor's verification URL
    // (`<base>/models` unless the descriptor names its own) before anything is saved.
    let protocol =
        radian_host::Protocol::from_kind(&kind).unwrap_or(radian_host::Protocol::OpenAiCompatible);
    let verify_url = radian_host::login::verification_url(&name, protocol, &base_url);
    print!("verifying against {verify_url} … ");
    let _ = std::io::stdout().flush();
    match radian_host::login::verify(&name, protocol, &base_url, &key).await {
        Ok(()) => println!("ok"),
        Err(reason) => {
            println!("failed");
            eprintln!("radian: {reason}");
            let answer = match read_line("save anyway? [y/N] ") {
                Ok(answer) => answer.trim().to_ascii_lowercase(),
                Err(error) => return aborted(error),
            };
            if answer != "y" && answer != "yes" {
                println!("login cancelled; nothing was saved");
                return 0;
            }
        }
    }

    // The save: `auth.json` only (D3). A custom provider carries its endpoint and protocol
    // in the entry's `env` (D10).
    let mut env = BTreeMap::new();
    if Some(&base_url) != default_base_url_of(&name).as_ref() {
        // Not exactly the built-in default: carry the endpoint (also for a self-hosted
        // generic endpoint).
        env.insert(radian_host::RADIAN_BASE_URL.to_owned(), base_url.clone());
    }
    if is_custom(&name) {
        env.insert(radian_host::RADIAN_KIND.to_owned(), kind);
    }
    let mut store = match radian_host::AuthStore::open() {
        Ok(store) => store,
        Err(error) => return aborted(format!("could not save the key: {error}")),
    };
    let existing = store.key_choices(&name).unwrap_or_default();
    let add = if existing.is_empty() {
        None
    } else {
        println!(
            "`{name}` already has {} key(s): {}",
            existing.len(),
            existing
                .iter()
                .map(|key| format!("{} ({})", key.name, key.masked))
                .collect::<Vec<_>>()
                .join(", ")
        );
        match read_line("replace the current key, or add another? [r/a] ") {
            Ok(answer) if answer.trim().eq_ignore_ascii_case("a") => {
                match read_line("name for the new key (empty allowed): ") {
                    Ok(name) => Some(name),
                    Err(error) => return aborted(error),
                }
            }
            Ok(_) => None,
            Err(error) => return aborted(error),
        }
    };
    match add {
        Some(ref key_name) => store.add_api_key(&name, &key, Some(key_name), &env),
        None => store.upsert_api_key(&name, &key, &env),
    }
    .map_or_else(
        |error| aborted(format!("could not save the key: {error}")),
        |()| {
            println!(
                "saved the key for `{name}` in credentials/auth.json; use it with \\
                     --provider {name} --model <model> (or /model {name}/<model>)"
            );
            0
        },
    )
}

/// The custom provider's questions (D10): the name — its `auth.json` slot — the protocol,
/// the endpoint. Echo stays on for all three.
#[cfg(unix)]
fn ask_custom_steps() -> Result<(String, String, String), i32> {
    let name = match read_line("provider name (its auth.json entry): ") {
        Ok(name) => name.trim().to_owned(),
        Err(error) => return Err(aborted(error)),
    };
    if let Some(reason) = radian_host::provider::name_error(&name) {
        return Err(aborted(reason));
    }
    println!("  1. openai-compatible   2. anthropic   3. openai-responses");
    let kind = match read_line("protocol [1]: ") {
        Ok(answer) => match answer.trim() {
            "" | "1" => "openai-compatible".to_owned(),
            "2" => "anthropic".to_owned(),
            "3" => "openai-responses".to_owned(),
            other => match radian_host::Protocol::from_kind(other) {
                Some(protocol) => protocol.kind().to_owned(),
                None => {
                    return Err(aborted(format!(
                        "unknown protocol {other:?} ({})",
                        radian_host::kind_list()
                    )));
                }
            },
        },
        Err(error) => return Err(aborted(error)),
    };
    let base_url = match read_line("base URL: ") {
        Ok(url) => url.trim().to_owned(),
        Err(error) => return Err(aborted(error)),
    };
    if !(base_url.starts_with("http://") || base_url.starts_with("https://")) {
        return Err(aborted(
            "the base URL must start with http:// or https://".to_owned(),
        ));
    }
    Ok((name, kind, base_url))
}

/// `radian login chatgpt` (ADR 0026): PKCE + the fixed loopback callback. Printed,
/// not hidden — there is no key to type, only a URL. A busy port 1455 is reported by
/// `BrowserLogin::start`'s own error, which already names the device-code login as the
/// alternative (`oauth_chatgpt::OAuthError::PortBusy`'s `Display`).
async fn run_chatgpt_browser_login() -> i32 {
    let endpoints = radian_host::OAuthEndpoints::default();
    let (login, listener) = match radian_host::BrowserLogin::start(&endpoints).await {
        Ok(pair) => pair,
        Err(error) => return aborted(error.to_string()),
    };
    println!("opening your browser to sign in with ChatGPT…");
    println!("if it does not open, visit:\n  {}", login.authorize_url);
    radian_host::oauth_chatgpt::try_open_browser(&login.authorize_url);
    println!("waiting for the browser to finish sign-in…");
    let token = match login
        .finish(
            &endpoints,
            listener,
            radian_host::oauth_chatgpt::CALLBACK_TIMEOUT,
        )
        .await
    {
        Ok(token) => token,
        Err(error) => return aborted(error.to_string()),
    };
    finish_chatgpt_login(&token)
}

/// `radian login chatgpt-device`: no loopback listener, no browser required on this machine.
async fn run_chatgpt_device_login() -> i32 {
    let endpoints = radian_host::OAuthEndpoints::default();
    let start = match radian_host::oauth_device_start(&endpoints).await {
        Ok(start) => start,
        Err(error) => return aborted(error.to_string()),
    };
    println!(
        "go to {} and enter this code:\n  {}",
        radian_host::oauth_chatgpt::DEVICE_VERIFICATION_URL,
        start.user_code
    );
    println!("waiting for you to finish sign-in…");
    // The same overall budget as the browser flow's callback wait, at the server's own poll
    // interval (floored at `DEVICE_POLL_MIN_INTERVAL`).
    let max_polls = (radian_host::oauth_chatgpt::CALLBACK_TIMEOUT.as_secs()
        / start.interval.as_secs().max(1))
    .max(1) as u32;
    let token = match radian_host::oauth_poll_device_until_done(
        &endpoints,
        &start,
        start.interval,
        max_polls,
    )
    .await
    {
        Ok(token) => token,
        Err(error) => return aborted(error.to_string()),
    };
    finish_chatgpt_login(&token)
}

/// Saves the credential and prints the honest "what next" line: the Responses adapter and
/// the `chatgpt` provider descriptor are later slices (ADR 0026), so there is
/// nothing to switch to yet — only `radian login`'s own job (storing the credential) is done.
fn finish_chatgpt_login(token: &radian_host::OAuthTokenResponse) -> i32 {
    let result = radian_host::AuthStore::open()
        .and_then(|mut store| radian_host::oauth_store_token(&mut store, token))
        .map_err(|error| error.to_string());
    match result {
        Ok(()) => {
            println!(
                "signed in to ChatGPT; saved in credentials/auth.json. Nothing uses it yet — \
                 the ChatGPT provider and its request adapter ship in a later change."
            );
            0
        }
        Err(error) => aborted(format!("could not save the sign-in: {error}")),
    }
}

/// `radian logout <provider> [name]`: confirm, then remove **only** that entry (or, with a
/// name, that key without asking). Several keys list and ask; `all` removes every key (D3).
pub async fn run_logout(provider: String, name: Option<String>) -> i32 {
    if let Some(name) = name {
        // A named removal is not a question: no terminal is needed and nothing else is read.
        return match radian_host::AuthStore::open()
            .and_then(|mut store| store.remove_key(&provider, Some(&name)))
        {
            Ok(()) => {
                println!("removed the key named `{name}` from `{provider}`");
                0
            }
            Err(error) => aborted(format!("could not remove `{name}`: {error}")),
        };
    }
    if !is_tty() {
        return aborted(
            "logout asks for a confirmation, so it needs a terminal; remove the entry from \
             ~/.radian/credentials/auth.json instead"
                .to_owned(),
        );
    }
    let store = match radian_host::AuthStore::open() {
        Ok(store) => store,
        Err(error) => return aborted(format!("could not open auth.json: {error}")),
    };
    let stored: Vec<_> = store
        .list()
        .into_iter()
        .map(|entry| entry.provider_id)
        .collect();
    if !stored.contains(&provider) {
        return aborted(format!(
            "`{provider}` has no stored credential; stored: {}",
            if stored.is_empty() {
                "(none)".to_owned()
            } else {
                stored.join(", ")
            }
        ));
    }
    let keys = store.key_choices(&provider).unwrap_or_default();
    if keys.len() > 1 {
        println!("`{provider}` has several keys:");
        for (index, key) in keys.iter().enumerate() {
            println!("  {}. {} ({})", index + 1, key.name, key.masked);
        }
        let answer = match read_line("remove which one? number, name, or `all`: ") {
            Ok(answer) => answer.trim().to_owned(),
            Err(error) => return aborted(error),
        };
        if answer.eq_ignore_ascii_case("all") {
            return match radian_host::AuthStore::open()
                .and_then(|mut store| store.remove_key(&provider, None))
            {
                Ok(()) => {
                    println!("removed every key for `{provider}`; every other entry is untouched");
                    0
                }
                Err(error) => aborted(format!("could not remove: {error}")),
            };
        }
        let picked = answer
            .parse::<usize>()
            .ok()
            .and_then(|index| keys.get(index.wrapping_sub(1)).map(|key| key.name.clone()))
            .or_else(|| {
                keys.iter()
                    .any(|key| key.name == answer)
                    .then(|| answer.clone())
            });
        let Some(picked) = picked else {
            return aborted(format!(
                "`{provider}` has no key named `{answer}`; known: {}",
                keys.iter()
                    .map(|key| key.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        };
        return match radian_host::AuthStore::open()
            .and_then(|mut store| store.remove_key(&provider, Some(&picked)))
        {
            Ok(()) => {
                println!("removed the key named `{picked}` from `{provider}`");
                0
            }
            Err(error) => aborted(format!("could not remove: {error}")),
        };
    }
    let answer = match read_line(&format!(
        "remove the stored credential for `{provider}`? [y/N] "
    )) {
        Ok(answer) => answer.trim().to_ascii_lowercase(),
        Err(error) => return aborted(error),
    };
    if answer != "y" && answer != "yes" {
        println!("kept `{provider}`");
        return 0;
    }
    match radian_host::AuthStore::open() {
        Ok(mut store) => match store.remove_key(&provider, None) {
            Ok(()) => {
                println!("removed `{provider}`; every other entry is untouched");
                0
            }
            Err(error) => aborted(format!("could not remove: {error}")),
        },
        Err(error) => aborted(format!("could not remove: {error}")),
    }
}

/// The built-in default endpoint of `name`, or `None` when it is not built in.
fn default_base_url_of(name: &str) -> Option<String> {
    radian_host::descriptor(name).map(|descriptor| descriptor.default_base_url.to_owned())
}

/// Whether `name` is a custom provider (D10), i.e. not a built-in descriptor id.
fn is_custom(name: &str) -> bool {
    radian_host::descriptor(name).is_none()
}
