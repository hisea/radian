//! The `/login` wizard (ADR 0026 D3): a pure state machine, like [`crate::app::App`].
//!
//! One step at a time — a built-in provider asks for its key; "custom" asks for the name,
//! the protocol, the base URL and then the key (all of it lands in **one** `auth.json`
//! entry, D10). The key is collected into `self.key` and is **never** rendered: the drawer
//! masks it, and no `Display`/`Debug` of this type may print it — hence the manual `Debug`.
//!
//! Everything network-shaped (the verification request, the `auth.json` write) is an
//! [`LoginOutcome`] the loop executes; a step only ever edits strings.

/// The wizard's choice for "not one of the built-ins" (D10: a named provider).
pub const CUSTOM: &str = "custom";

/// One key press or paste inside the wizard. The keys are **hardcoded** (a login prompt is
/// sensitive; ADR 0023 U4 keeps approval answers hardcoded for the same reason), so this is
/// its own tiny vocabulary rather than the keymap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginKey {
    Char(char),
    Backspace,
    /// Paste: the text joins the focused field character by character (masked like typing,
    /// so a pasted key is never echoed).
    Paste(String),
    Enter,
    Esc,
    Up,
    Down,
}

/// The wizard's steps, in the order a custom login walks them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginStep {
    /// Pick a provider: the built-in descriptors, or `custom` (D10).
    ChooseProvider { cursor: usize },
    /// The custom provider's name — also its `auth.json` key slot.
    CustomName,
    /// The wire protocol: `openai-compatible`, `anthropic` or `openai-responses` (D10).
    CustomProtocol { cursor: usize },
    /// The endpoint.
    CustomBaseUrl,
    /// The key. Masked on screen; typed or pasted alike.
    KeyInput,
    /// The entry already has a key: replace it, or add another (ADR 0026 D3).
    ReplaceOrAdd { cursor: usize },
    /// The new key's name for `a` (empty is allowed; an unnamed key shows masked).
    AddName,
    /// The verification request is in flight (D3). `Esc` still cancels.
    Verifying,
    /// The verification failed; `y` saves anyway, `n`/`Esc` cancels.
    VerifyFailed { reason: String },
    /// Sign in with ChatGPT (ADR 0026): the loop has bound the loopback listener
    /// and is waiting for the browser redirect. `url` is `None` for the brief moment before
    /// the loop's effect reports it back (`Input::LoginOAuthUrlReady`) — shown once known so
    /// a remote/headless session (or a browser the host failed to open) has something to
    /// copy. `Esc` cancels the wizard; the background exchange, already running, finishes
    /// unobserved either way (the same "a late answer is dropped" rule `Verifying` follows).
    OAuthBrowserWaiting { url: Option<String> },
    /// Sign in with ChatGPT, device-code flow: the code to enter at `verification_url`, shown
    /// once the loop's effect starts polling (`Input::LoginOAuthDeviceReady`). Empty strings
    /// until then.
    OAuthDeviceWaiting {
        user_code: String,
        verification_url: String,
    },
}

/// The protocol choices, in display order (ADR 0026 D10).
pub const PROTOCOLS: [&str; 3] = ["openai-compatible", "anthropic", "openai-responses"];

/// One wizard.
pub struct LoginWizard {
    pub step: LoginStep,
    /// The chosen built-in descriptor id, or `None` for a custom provider.
    pub built_in: Option<&'static str>,
    /// The custom name (also the `auth.json` key slot).
    pub name: String,
    /// Index into [`PROTOCOLS`].
    pub protocol: usize,
    /// The custom endpoint.
    pub base_url: String,
    /// The key. Never rendered (the `Debug` impl lies on purpose).
    pub key: String,
    /// The name of a key to **add** to an entry that already has one (the `a` choice).
    /// `None` before the choice is made.
    pub add_name: String,
    /// Whether the entry already had a key when the verification answered (D3's
    /// replace-or-add choice).
    pub existing: bool,
    /// A step-local error line (a bad name, a bad URL); cleared on the next edit.
    pub error: Option<String>,
}

impl std::fmt::Debug for LoginWizard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoginWizard")
            .field("step", &self.step)
            .field("built_in", &self.built_in)
            .field("name", &self.name)
            .field("protocol", &self.protocol)
            .field("base_url", &self.base_url)
            .field("key", &"<redacted>")
            .field("add_name", &self.add_name)
            .field("existing", &self.existing)
            .field("error", &self.error)
            .finish()
    }
}

impl LoginWizard {
    /// A wizard starting at `step` (the `/login` argument may skip the choice).
    pub fn new(step: LoginStep) -> Self {
        Self {
            step,
            built_in: None,
            name: String::new(),
            protocol: 0,
            base_url: String::new(),
            key: String::new(),
            add_name: String::new(),
            existing: false,
            error: None,
        }
    }

    /// A wizard that continues an explicit `/login <id>`: a built-in goes straight to its
    /// key; `custom` starts at the name.
    pub fn for_provider(id: &str) -> Self {
        if id == CUSTOM {
            return Self::new(LoginStep::CustomName);
        }
        let mut wizard = Self::new(LoginStep::KeyInput);
        wizard.built_in = radian_host::descriptor(id).map(|descriptor| descriptor.id);
        wizard
    }

    /// The provider picker's candidates: the built-in descriptors, the two ChatGPT OAuth
    /// choices (ADR 0026), then `custom`. Kept byte-identical to
    /// `radian_host::login_choices()` (same ids, same order, same text) — a pre-existing
    /// duplication (`radian login`'s numbered list builds the same list independently) this
    /// change does not introduce.
    pub fn choices() -> Vec<(String, String)> {
        // `chatgpt` is now a real descriptor (ADR 0026 slice 3: so `/model` lists
        // its models), but its login is the two OAuth entries pushed below, not the generic
        // "ask for an API key" flow every other descriptor gets from this loop.
        let mut choices: Vec<(String, String)> = radian_host::descriptors()
            .iter()
            .filter(|d| d.id != radian_host::CHATGPT_PROVIDER_ID)
            .map(|d| (d.id.to_owned(), d.display_name.to_owned()))
            .collect();
        choices.push((
            radian_host::CHATGPT_PROVIDER_ID.to_owned(),
            "Sign in with ChatGPT (opens your browser)".to_owned(),
        ));
        choices.push((
            radian_host::CHATGPT_DEVICE_LOGIN.to_owned(),
            "ChatGPT (device code; no browser needed on this machine)".to_owned(),
        ));
        choices.push((
            CUSTOM.to_owned(),
            "a custom endpoint: name, protocol, base URL and key".to_owned(),
        ));
        choices
    }

    /// The provider the wizard will store the key under: the built-in id, or the custom name.
    pub fn provider_name(&self) -> &str {
        match self.built_in {
            Some(id) => id,
            None => &self.name,
        }
    }

    /// The wire protocol's `kind` spelling.
    pub fn protocol_kind(&self) -> &'static str {
        match self.built_in {
            Some(id) => radian_host::descriptor(id)
                .map(|descriptor| descriptor.default_protocol.kind())
                .unwrap_or("openai-compatible"),
            None => PROTOCOLS[self.protocol.min(PROTOCOLS.len() - 1)],
        }
    }

    /// The `env` entries the saved record carries: a custom provider names its endpoint and
    /// protocol (D10); a built-in needs none.
    pub fn env(&self) -> Vec<(String, String)> {
        match self.built_in {
            Some(_) => Vec::new(),
            None => vec![
                (
                    radian_host::RADIAN_BASE_URL.to_owned(),
                    self.base_url.clone(),
                ),
                (
                    radian_host::RADIAN_KIND.to_owned(),
                    self.protocol_kind().to_owned(),
                ),
            ],
        }
    }

    /// Applies one key. Returns what the loop must do.
    pub fn on_key(&mut self, key: LoginKey) -> LoginOutcome {
        match key {
            LoginKey::Esc => return LoginOutcome::Cancel,
            LoginKey::Char(c) if c.is_control() => return LoginOutcome::Stay,
            _ => {}
        }
        match &mut self.step {
            LoginStep::ChooseProvider { cursor } => match key {
                LoginKey::Up => {
                    *cursor = cursor.saturating_sub(1);
                }
                LoginKey::Down => {
                    *cursor = (*cursor + 1).min(Self::choices().len() - 1);
                }
                LoginKey::Enter => {
                    let choices = Self::choices();
                    let Some((insert, _)) = choices.get(*cursor) else {
                        return LoginOutcome::Stay;
                    };
                    if insert == CUSTOM {
                        self.step = LoginStep::CustomName;
                    } else if insert == radian_host::CHATGPT_PROVIDER_ID {
                        self.step = LoginStep::OAuthBrowserWaiting { url: None };
                        return LoginOutcome::BeginOAuthBrowser;
                    } else if insert == radian_host::CHATGPT_DEVICE_LOGIN {
                        self.step = LoginStep::OAuthDeviceWaiting {
                            user_code: String::new(),
                            verification_url: String::new(),
                        };
                        return LoginOutcome::BeginOAuthDevice;
                    } else {
                        // The choice is a descriptor id, so this always resolves.
                        self.built_in = radian_host::descriptor(insert).map(|d| d.id);
                        self.step = LoginStep::KeyInput;
                    }
                }
                _ => {}
            },
            LoginStep::CustomName => match key {
                LoginKey::Char(c) => {
                    self.error = None;
                    self.name.push(c);
                }
                LoginKey::Backspace => {
                    self.error = None;
                    self.name.pop();
                }
                LoginKey::Paste(text) => {
                    self.error = None;
                    self.name.push_str(&text);
                }
                LoginKey::Enter => {
                    if let Some(reason) = radian_host::provider::name_error(&self.name) {
                        self.error = Some(reason);
                    } else {
                        self.step = LoginStep::CustomProtocol { cursor: 0 };
                    }
                }
                _ => {}
            },
            LoginStep::CustomProtocol { cursor } => match key {
                LoginKey::Up => {
                    *cursor = cursor.saturating_sub(1);
                }
                LoginKey::Down => {
                    *cursor = (*cursor + 1).min(PROTOCOLS.len() - 1);
                }
                LoginKey::Enter => {
                    self.protocol = *cursor;
                    self.step = LoginStep::CustomBaseUrl;
                }
                _ => {}
            },
            LoginStep::CustomBaseUrl => match key {
                LoginKey::Char(c) => {
                    self.error = None;
                    self.base_url.push(c);
                }
                LoginKey::Backspace => {
                    self.error = None;
                    self.base_url.pop();
                }
                LoginKey::Paste(text) => {
                    self.error = None;
                    self.base_url.push_str(&text);
                }
                LoginKey::Enter => {
                    let url = self.base_url.trim();
                    if !(url.starts_with("http://") || url.starts_with("https://")) {
                        self.error =
                            Some("the base URL must start with http:// or https://".into());
                    } else {
                        self.base_url = url.to_owned();
                        self.step = LoginStep::KeyInput;
                    }
                }
                _ => {}
            },
            LoginStep::KeyInput => match key {
                LoginKey::Char(c) => self.key.push(c),
                LoginKey::Backspace => {
                    self.key.pop();
                }
                LoginKey::Paste(text) => self.key.push_str(&text),
                LoginKey::Enter => {
                    if self.key.is_empty() {
                        return LoginOutcome::Stay;
                    }
                    self.step = LoginStep::Verifying;
                    return LoginOutcome::Verify {
                        name: self.provider_name().to_owned(),
                        protocol: self.protocol_kind(),
                        base_url: self.verification_base_url(),
                        key: self.key.clone(),
                    };
                }
                _ => {}
            },
            LoginStep::Verifying => {} // only Esc (handled above)
            LoginStep::ReplaceOrAdd { cursor } => match key {
                LoginKey::Up => *cursor = cursor.saturating_sub(1),
                LoginKey::Down => *cursor = (*cursor + 1).min(1),
                LoginKey::Char('r') | LoginKey::Char('R') => return self.save(None),
                LoginKey::Char('a') | LoginKey::Char('A') => {
                    self.step = LoginStep::AddName;
                }
                LoginKey::Enter => {
                    if *cursor == 0 {
                        return self.save(None);
                    }
                    self.step = LoginStep::AddName;
                }
                _ => {}
            },
            LoginStep::AddName => match key {
                LoginKey::Char(c) => self.add_name.push(c),
                LoginKey::Backspace => {
                    self.add_name.pop();
                }
                LoginKey::Paste(text) => self.add_name.push_str(&text),
                LoginKey::Enter => {
                    return self.save(Some(self.add_name.trim().to_owned()));
                }
                _ => {}
            },
            LoginStep::OAuthBrowserWaiting { .. } => {} // only Esc (handled above)
            LoginStep::OAuthDeviceWaiting { .. } => {}  // only Esc (handled above)
            LoginStep::VerifyFailed { .. } => match key {
                LoginKey::Char('y') | LoginKey::Char('Y') => {
                    if self.existing {
                        self.step = LoginStep::ReplaceOrAdd { cursor: 0 };
                        return LoginOutcome::Stay;
                    }
                    return self.save(None);
                }
                _ => return LoginOutcome::Cancel,
            },
        }
        LoginOutcome::Stay
    }

    /// The saved record: the provider and its key, with `add` naming the new key when this is
    /// an append to an entry that already had one.
    fn save(&self, add: Option<String>) -> LoginOutcome {
        LoginOutcome::Save {
            name: self.provider_name().to_owned(),
            key: self.key.clone(),
            env: self.env(),
            add,
        }
    }

    /// The endpoint the verification request goes to: the custom base URL, or the built-in
    /// descriptor's default. A named endpoint is exactly what was typed (D3).
    fn verification_base_url(&self) -> String {
        match self.built_in {
            Some(id) => radian_host::descriptor(id)
                .map(|d| d.default_base_url.to_owned())
                .unwrap_or_default(),
            None => self.base_url.clone(),
        }
    }

    /// The verification finished. `Ok` goes straight to saving, unless the entry already had
    /// a key: then replace-or-add is asked first (ADR 0026 D3). `Err` asks save-or-cancel.
    pub fn verified(
        &mut self,
        result: &Result<(), String>,
        existing: bool,
    ) -> Option<LoginOutcome> {
        let LoginStep::Verifying = self.step else {
            return None; // a late answer for a wizard that moved on
        };
        self.existing = existing;
        match result {
            Ok(()) => {
                if existing {
                    self.step = LoginStep::ReplaceOrAdd { cursor: 0 };
                    None
                } else {
                    Some(self.save(None))
                }
            }
            Err(reason) => {
                self.step = LoginStep::VerifyFailed {
                    reason: reason.clone(),
                };
                None
            }
        }
    }

    /// The masked text a field renders: one `*` per character, so typing and pasting are
    /// both visible as *happening* without ever showing a character of the value. An empty
    /// field shows the hint instead.
    pub fn masked(value: &str) -> String {
        "*".repeat(value.chars().count())
    }

    /// The loop's effect bound the browser-flow listener and has the authorization URL
    /// (`Input::LoginOAuthUrlReady`). A no-op for a wizard that moved on (cancelled, or never
    /// in this step — the same "late answer" rule `verified` follows).
    pub fn oauth_browser_url_ready(&mut self, url: &str) {
        if let LoginStep::OAuthBrowserWaiting { url: slot } = &mut self.step {
            *slot = Some(url.to_owned());
        }
    }

    /// The loop's effect started the device-code poll and has the code to show
    /// (`Input::LoginOAuthDeviceReady`). A no-op for a wizard that moved on.
    pub fn oauth_device_ready(&mut self, user_code: &str, verification_url: &str) {
        if let LoginStep::OAuthDeviceWaiting {
            user_code: code_slot,
            verification_url: url_slot,
        } = &mut self.step
        {
            *code_slot = user_code.to_owned();
            *url_slot = verification_url.to_owned();
        }
    }
}

/// What one key produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginOutcome {
    /// The wizard handled the key and stays up.
    Stay,
    /// Run the verification request (D3): one `GET` of the descriptor's verification URL.
    Verify {
        name: String,
        protocol: &'static str,
        base_url: String,
        key: String,
    },
    /// Write the `auth.json` entry (after a passing verification, or "save anyway"). `add`
    /// names a key to append to an entry that already has one; `None` replaces the entry's
    /// first key.
    Save {
        name: String,
        key: String,
        env: Vec<(String, String)>,
        add: Option<String>,
    },
    /// Close without saving.
    Cancel,
    /// Start the ChatGPT browser OAuth flow (`Effect::LoginOAuthBrowser`): bind the loopback
    /// listener, open/print the authorize URL, wait for the redirect, exchange, save.
    BeginOAuthBrowser,
    /// Start the ChatGPT device-code flow (`Effect::LoginOAuthDevice`): ask for a user code,
    /// show it, poll, exchange, save.
    BeginOAuthDevice,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn type_string(wizard: &mut LoginWizard, text: &str) {
        for c in text.chars() {
            assert_eq!(wizard.on_key(LoginKey::Char(c)), LoginOutcome::Stay);
        }
    }

    #[test]
    fn the_choice_list_is_the_builtins_then_chatgpt_oauth_then_custom() {
        let choices = LoginWizard::choices();
        assert_eq!(
            choices
                .iter()
                .map(|(id, _)| id.as_str())
                .collect::<Vec<_>>(),
            [
                "opencode-go",
                "anthropic",
                "openai",
                "openai-compatible",
                radian_host::CHATGPT_PROVIDER_ID,
                radian_host::CHATGPT_DEVICE_LOGIN,
                CUSTOM,
            ]
        );
        // Mirrors `radian_host::login_choices()` exactly (ids and order), so the TUI picker
        // and `radian login`'s numbered list cannot drift.
        assert_eq!(
            choices.iter().map(|(id, _)| id.clone()).collect::<Vec<_>>(),
            radian_host::login_choices()
                .into_iter()
                .map(|(id, _)| id)
                .collect::<Vec<_>>(),
        );
    }

    #[test]
    fn choosing_sign_in_with_chatgpt_begins_the_browser_flow() {
        let mut wizard = LoginWizard::new(LoginStep::ChooseProvider {
            cursor: LoginWizard::choices()
                .iter()
                .position(|(id, _)| id == radian_host::CHATGPT_PROVIDER_ID)
                .unwrap(),
        });
        assert_eq!(
            wizard.on_key(LoginKey::Enter),
            LoginOutcome::BeginOAuthBrowser
        );
        assert!(matches!(
            wizard.step,
            LoginStep::OAuthBrowserWaiting { url: None }
        ));
        // The loop reports the URL once the listener is bound.
        wizard.oauth_browser_url_ready("https://auth.openai.com/oauth/authorize?...");
        assert!(matches!(
            &wizard.step,
            LoginStep::OAuthBrowserWaiting { url: Some(u) }
                if u == "https://auth.openai.com/oauth/authorize?..."
        ));
    }

    #[test]
    fn choosing_the_device_code_begins_that_flow_and_shows_the_code_once_reported() {
        let mut wizard = LoginWizard::new(LoginStep::ChooseProvider {
            cursor: LoginWizard::choices()
                .iter()
                .position(|(id, _)| id == radian_host::CHATGPT_DEVICE_LOGIN)
                .unwrap(),
        });
        assert_eq!(
            wizard.on_key(LoginKey::Enter),
            LoginOutcome::BeginOAuthDevice
        );
        assert!(matches!(wizard.step, LoginStep::OAuthDeviceWaiting { .. }));
        wizard.oauth_device_ready("ABCD-1234", "https://auth.openai.com/codex/device");
        match &wizard.step {
            LoginStep::OAuthDeviceWaiting {
                user_code,
                verification_url,
            } => {
                assert_eq!(user_code, "ABCD-1234");
                assert_eq!(verification_url, "https://auth.openai.com/codex/device");
            }
            other => panic!("{other:?}"),
        }
        // A late report for a wizard that moved on (e.g. cancelled) is dropped.
        let mut moved_on = LoginWizard::new(LoginStep::CustomName);
        moved_on.oauth_device_ready("SHOULD-NOT-APPEAR", "https://example.com");
        assert!(matches!(moved_on.step, LoginStep::CustomName));
    }

    #[test]
    fn a_builtin_login_asks_only_for_the_key() {
        let mut wizard = LoginWizard::for_provider("anthropic");
        assert!(matches!(wizard.step, LoginStep::KeyInput));
        assert_eq!(wizard.provider_name(), "anthropic");
        assert!(wizard.env().is_empty(), "a built-in carries no env");
        type_string(&mut wizard, "sk-ant-1");
        // Enter → verify against the built-in's own endpoint, then save on success.
        assert_eq!(
            wizard.on_key(LoginKey::Enter),
            LoginOutcome::Verify {
                name: "anthropic".to_owned(),
                protocol: "anthropic",
                base_url: "https://api.anthropic.com".to_owned(),
                key: "sk-ant-1".to_owned(),
            }
        );
        // The 200 → save.
        assert_eq!(
            wizard.verified(&Ok(()), false).expect("save after verify"),
            LoginOutcome::Save {
                name: "anthropic".to_owned(),
                key: "sk-ant-1".to_owned(),
                env: Vec::new(),
                add: None,
            }
        );
    }

    #[test]
    fn a_custom_login_walks_name_protocol_url_key() {
        let mut wizard = LoginWizard::for_provider(CUSTOM);
        assert!(matches!(wizard.step, LoginStep::CustomName));
        assert_eq!(wizard.provider_name(), "", "no name yet");

        // An invalid name is an inline error, not a step forward.
        type_string(&mut wizard, "openai-compatible");
        assert_eq!(wizard.on_key(LoginKey::Enter), LoginOutcome::Stay);
        assert!(wizard.error.is_some(), "a reserved name is refused inline");
        for _ in 0.."openai-compatible".len() {
            wizard.on_key(LoginKey::Backspace);
        }
        type_string(&mut wizard, "corp");
        assert_eq!(wizard.on_key(LoginKey::Enter), LoginOutcome::Stay);
        assert!(matches!(wizard.step, LoginStep::CustomProtocol { .. }));
        assert_eq!(wizard.provider_name(), "corp");

        // Protocol defaults to openai-compatible; Down picks anthropic. The third entry is
        // openai-responses (ADR 0026 D10's third kind).
        assert_eq!(
            PROTOCOLS,
            ["openai-compatible", "anthropic", "openai-responses"]
        );
        assert_eq!(wizard.protocol_kind(), "openai-compatible");
        wizard.on_key(LoginKey::Down);
        wizard.on_key(LoginKey::Enter);
        assert!(matches!(wizard.step, LoginStep::CustomBaseUrl));
        assert_eq!(wizard.protocol_kind(), "anthropic");

        // A bad URL is an inline error; fixing it moves on.
        type_string(&mut wizard, "corp.example.com");
        assert_eq!(wizard.on_key(LoginKey::Enter), LoginOutcome::Stay);
        assert!(wizard.error.is_some());
        for _ in 0.."corp.example.com".len() {
            wizard.on_key(LoginKey::Backspace);
        }
        type_string(&mut wizard, "http://corp.example.com/");
        assert_eq!(wizard.on_key(LoginKey::Enter), LoginOutcome::Stay);
        assert!(matches!(wizard.step, LoginStep::KeyInput));

        // The key: pasted text is accepted but never displayed.
        assert_eq!(
            wizard.on_key(LoginKey::Paste("sk-relay-222".to_owned())),
            LoginOutcome::Stay
        );
        assert_eq!(LoginWizard::masked(&wizard.key), "*".repeat(12));
        assert_eq!(
            wizard.on_key(LoginKey::Enter),
            LoginOutcome::Verify {
                name: "corp".to_owned(),
                protocol: "anthropic",
                base_url: "http://corp.example.com/".to_owned(),
                key: "sk-relay-222".to_owned(),
            }
        );
        // The saved record carries the endpoint and protocol in `env` (D10).
        assert_eq!(
            wizard.verified(&Ok(()), false).expect("save after verify"),
            LoginOutcome::Save {
                name: "corp".to_owned(),
                key: "sk-relay-222".to_owned(),
                env: vec![
                    (
                        radian_host::RADIAN_BASE_URL.to_owned(),
                        "http://corp.example.com/".to_owned()
                    ),
                    ("RADIAN_KIND".to_owned(), "anthropic".to_owned()),
                ],
                add: None,
            }
        );
    }

    #[test]
    fn a_failed_verification_asks_save_or_cancel() {
        let mut wizard = LoginWizard::for_provider("openai");
        type_string(&mut wizard, "sk-oai");
        let outcome = wizard.on_key(LoginKey::Enter);
        assert!(matches!(outcome, LoginOutcome::Verify { .. }));
        assert!(
            wizard
                .verified(&Err("401 rejected".to_owned()), false)
                .is_none()
        );
        assert!(matches!(wizard.step, LoginStep::VerifyFailed { .. }));
        // `y` saves anyway.
        assert!(matches!(
            wizard.on_key(LoginKey::Char('y')),
            LoginOutcome::Save { .. }
        ));
        // A fresh wizard: `n` cancels (and Esc would too).
        let mut wizard = LoginWizard::for_provider("openai");
        type_string(&mut wizard, "sk-oai");
        assert!(matches!(
            wizard.on_key(LoginKey::Enter),
            LoginOutcome::Verify { .. }
        ));
        assert!(wizard.verified(&Err("no".to_owned()), false).is_none());
        assert_eq!(wizard.on_key(LoginKey::Char('n')), LoginOutcome::Cancel);
    }

    /// ADR 0026 D3: an entry that already has a key offers replace-or-add; `a` asks a name and
    /// appends, `r` replaces. A failed verification answers the same question before saving.
    #[test]
    fn an_existing_key_offers_replace_or_add() {
        let mut wizard = LoginWizard::for_provider("opencode-go");
        type_string(&mut wizard, "sk-second");
        assert!(matches!(
            wizard.on_key(LoginKey::Enter),
            LoginOutcome::Verify { .. }
        ));
        // The verification passed and the entry already had a key: no save yet.
        assert!(wizard.verified(&Ok(()), true).is_none());
        assert!(matches!(wizard.step, LoginStep::ReplaceOrAdd { cursor: 0 }));
        // `a` asks the name; Enter saves the append, an empty name allowed.
        assert_eq!(wizard.on_key(LoginKey::Char('a')), LoginOutcome::Stay);
        assert!(matches!(wizard.step, LoginStep::AddName));
        type_string(&mut wizard, "team");
        assert_eq!(
            wizard.on_key(LoginKey::Enter),
            LoginOutcome::Save {
                name: "opencode-go".to_owned(),
                key: "sk-second".to_owned(),
                env: Vec::new(),
                add: Some("team".to_owned()),
            }
        );
        // An empty name still appends (the item is then shown masked).
        let mut wizard = LoginWizard::for_provider("openai");
        type_string(&mut wizard, "sk-next");
        wizard.on_key(LoginKey::Enter);
        wizard.verified(&Ok(()), true);
        wizard.on_key(LoginKey::Char('a'));
        assert_eq!(
            wizard.on_key(LoginKey::Enter),
            LoginOutcome::Save {
                name: "openai".to_owned(),
                key: "sk-next".to_owned(),
                env: Vec::new(),
                add: Some(String::new()),
            }
        );
        // `r` replaces the first key.
        let mut wizard = LoginWizard::for_provider("openai");
        type_string(&mut wizard, "sk-replacement");
        wizard.on_key(LoginKey::Enter);
        wizard.verified(&Ok(()), true);
        assert!(matches!(
            wizard.on_key(LoginKey::Char('r')),
            LoginOutcome::Save { add: None, .. }
        ));
        // A failed verification's `y` also offers the choice when a key existed.
        let mut wizard = LoginWizard::for_provider("openai");
        type_string(&mut wizard, "sk-third");
        wizard.on_key(LoginKey::Enter);
        assert!(wizard.verified(&Err("401".to_owned()), true).is_none());
        assert!(matches!(
            wizard.on_key(LoginKey::Char('y')),
            LoginOutcome::Stay
        ));
        assert!(matches!(wizard.step, LoginStep::ReplaceOrAdd { cursor: 0 }));
    }

    #[test]
    fn esc_cancels_from_any_step_and_a_late_verify_is_ignored() {
        for step in [
            LoginStep::ChooseProvider { cursor: 0 },
            LoginStep::CustomName,
            LoginStep::CustomProtocol { cursor: 0 },
            LoginStep::CustomBaseUrl,
            LoginStep::KeyInput,
            LoginStep::ReplaceOrAdd { cursor: 0 },
            LoginStep::AddName,
            LoginStep::Verifying,
            LoginStep::OAuthBrowserWaiting { url: None },
            LoginStep::OAuthDeviceWaiting {
                user_code: String::new(),
                verification_url: String::new(),
            },
        ] {
            let mut wizard = LoginWizard::new(step.clone());
            assert_eq!(
                wizard.on_key(LoginKey::Esc),
                LoginOutcome::Cancel,
                "{step:?}"
            );
        }
        // A verification result for a wizard that is not waiting is dropped.
        let mut wizard = LoginWizard::for_provider(CUSTOM);
        assert!(wizard.verified(&Ok(()), false).is_none());
    }

    #[test]
    fn the_key_never_appears_in_the_debug_of_the_wizard() {
        let mut wizard = LoginWizard::for_provider("openai");
        type_string(&mut wizard, "sk-SUPER-SECRET-KEY");
        let text = format!("{wizard:?}");
        assert!(!text.contains("sk-SUPER-SECRET-KEY"), "{text}");
        assert!(text.contains("<redacted>"), "{text}");
    }
}
