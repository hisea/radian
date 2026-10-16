//! ChatGPT-subscription OAuth (ADR 0026, slice ①): PKCE browser login over a
//! one-shot loopback callback, the device-code flow, token exchange and refresh, and
//! decoding the `chatgpt_account_id` claim.
//!
//! Every endpoint this module calls is a field of [`Endpoints`], so a test builds one that
//! points at a local fake server; this module itself never hardcodes a reachable
//! `auth.openai.com`/`chatgpt.com` URL into a call site, only into [`Endpoints::default`]
//! (AGENTS.md: a test must never contact the real network). `CLIENT_ID` is OpenAI's own Codex
//! CLI client id, verified against `@oh-my-pi/pi-ai/src/registry/oauth/openai-codex.ts:13-29`
//! (2026-10-16, read-only reference) — there is no public client id for third-party
//! ChatGPT-subscription access; this is the accepted risk in ADR 0026's background.
//! [`ORIGINATOR`] is sent as `radian` on every request this module makes, never Codex's own
//! value (ADR 0026).

use std::net::SocketAddr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// OpenAI Codex's own OAuth client id (not a secret: PKCE is the proof of possession, not
/// this id).
pub const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";

/// The loopback port OpenAI's authorization server allowlists. **Fixed** for the real flow —
/// a different port's redirect URI is rejected server-side, so there is no fallback to try
/// another port (unlike a generic OAuth client). [`login_browser`] always binds this port;
/// [`wait_for_callback`] takes an explicit address so a test can bind an ephemeral one instead
/// and exercise the listener's logic without colliding with another test or a real login.
pub const CALLBACK_PORT: u16 = 1455;
pub const CALLBACK_PATH: &str = "/auth/callback";

/// `https://auth.openai.com/deviceauth/callback` — not a reachable endpoint of ours; it is the
/// fixed `redirect_uri` value the device-code token exchange expects (verified
/// `openai-codex.ts:24,331`).
pub const DEVICE_REDIRECT_URI: &str = "https://auth.openai.com/deviceauth/callback";

/// `https://auth.openai.com/codex/device` — where the user is told to go and type the device
/// code (verified `openai-codex.ts:25,284`).
pub const DEVICE_VERIFICATION_URL: &str = "https://auth.openai.com/codex/device";

pub const SCOPE: &str =
    "openid profile email offline_access api.connectors.read api.connectors.invoke";

/// Sent as `originator` on every request this module makes — never Codex's own value (ADR
/// 0026 revision 1 decision 1: not impersonating the Codex CLI).
pub const ORIGINATOR: &str = "radian";

/// The claim namespace the ChatGPT workspace id lives under.
const JWT_CLAIM_PATH: &str = "https://api.openai.com/auth";

/// How long a token-endpoint request may take.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// How long [`login_browser`] waits for the browser redirect before giving up.
pub const CALLBACK_TIMEOUT: Duration = Duration::from_secs(300);

/// How long before the recorded `expires` a token is refreshed. Chosen as comfortably more
/// than one turn's round trip plus clock skew between this machine and OpenAI's, so a refresh
/// never races a request that is already in flight; it is not tied to any observed token
/// lifetime (OpenAI's `expires_in` is typically on the order of an hour).
pub const REFRESH_MARGIN: Duration = Duration::from_secs(120);

/// The device-code poll interval's floor: a provider that answers `interval: 0` must not spin
/// the loop as hard as the network allows (verified pattern: `openai-codex.ts`'s
/// `DEVICE_POLL_SAFETY_MARGIN_MS`, a fixed margin added to whatever interval the server names).
pub const DEVICE_POLL_MIN_INTERVAL: Duration = Duration::from_secs(5);

/// Every URL this module calls, factored out so a test points the whole flow at a local fake
/// authorization server. [`Default`] is the real OpenAI endpoints; production code never
/// constructs anything else.
#[derive(Debug, Clone)]
pub struct Endpoints {
    pub authorize_url: String,
    pub token_url: String,
    pub device_usercode_url: String,
    pub device_token_url: String,
}

impl Default for Endpoints {
    fn default() -> Self {
        Self {
            authorize_url: "https://auth.openai.com/oauth/authorize".to_owned(),
            token_url: "https://auth.openai.com/oauth/token".to_owned(),
            device_usercode_url: "https://auth.openai.com/api/accounts/deviceauth/usercode"
                .to_owned(),
            device_token_url: "https://auth.openai.com/api/accounts/deviceauth/token".to_owned(),
        }
    }
}

/// Why an OAuth step failed. No variant's `Display` ever includes an access/refresh token, an
/// authorization code, or a PKCE verifier — those are not printed, logged, or returned to a
/// caller (ADR 0026; ADR 0013).
#[derive(Debug)]
pub enum OAuthError {
    /// Binding the fixed callback port failed because something else already holds it.
    PortBusy { port: u16 },
    /// A transport-level failure binding or accepting on the loopback listener.
    Io(std::io::Error),
    /// No callback (matching or not) arrived before the deadline.
    Timeout,
    /// The callback's `state` did not match the one this login generated. This is refused
    /// without touching the token endpoint.
    StateMismatch,
    /// The callback carried no `code` and no `error`.
    MissingCode,
    /// The authorization server (or the user, at the consent screen) reported an error.
    AuthorizationDenied { reason: String },
    /// A request to the loopback listener was for some other path.
    UnexpectedCallback { path: String },
    /// A transport-level failure talking to the token endpoint. The message is `reqwest`'s
    /// own, which does not interpolate request bodies or headers — only the target URL, which
    /// for every call in this module is a fixed, credential-free endpoint.
    Request(String),
    /// The token endpoint answered with a non-2xx status.
    TokenEndpoint { status: u16, message: String },
    /// A 2xx response was missing a required field, or the access token was not a 3-part JWT
    /// carrying the ChatGPT workspace claim — in either case, not a usable ChatGPT credential.
    MalformedToken,
    /// Device-code initiation failed.
    DeviceInit { status: u16 },
    /// A device-code poll failed for a reason other than "still pending".
    DevicePoll { status: u16 },
    /// The user did not approve the device code in time.
    DeviceExpired,
}

impl std::fmt::Display for OAuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PortBusy { port } => write!(
                f,
                "port {port} is already in use (ChatGPT's login only accepts this exact \
                 loopback port); close whatever is using it, or run the device-code login \
                 instead"
            ),
            Self::Io(source) => write!(f, "the local callback listener failed: {source}"),
            Self::Timeout => write!(f, "timed out waiting for the browser to finish sign-in"),
            Self::StateMismatch => write!(
                f,
                "the callback's state did not match this login; refusing it"
            ),
            Self::MissingCode => write!(f, "the callback carried no authorization code"),
            Self::AuthorizationDenied { reason } => {
                write!(f, "sign-in was not completed: {reason}")
            }
            Self::UnexpectedCallback { path } => {
                write!(f, "a request to {path} is not this login's callback")
            }
            Self::Request(message) => write!(f, "could not reach OpenAI: {message}"),
            Self::TokenEndpoint { status, message } => {
                write!(f, "OpenAI rejected the request (HTTP {status}): {message}")
            }
            Self::MalformedToken => {
                write!(f, "OpenAI's response was not a usable ChatGPT credential")
            }
            Self::DeviceInit { status } => {
                write!(f, "could not start device-code sign-in (HTTP {status})")
            }
            Self::DevicePoll { status } => write!(f, "device-code sign-in failed (HTTP {status})"),
            Self::DeviceExpired => write!(
                f,
                "the device code expired before sign-in was completed; run it again"
            ),
        }
    }
}

impl std::error::Error for OAuthError {}

/// A ready-to-store ChatGPT credential: the pieces `auth.json`'s `oauth` entry wants
/// (ADR 0026 D1's reserved shape — `access`, `refresh`, `expires`). `account_id` is **not**
/// persisted (ADR 0026: decoded from the access token each time it is
/// needed); it is returned here only so a login can show the user which ChatGPT workspace it
/// signed into.
#[derive(Debug, Clone)]
pub struct TokenResponse {
    pub access: String,
    pub refresh: String,
    /// Unix epoch milliseconds, matching pi's `auth.json` `expires` field (pi computes the
    /// same `Date.now() + expires_in * 1000`).
    pub expires_at_millis: u64,
    pub account_id: String,
}

/// PKCE (S256): a random verifier, and its SHA-256 digest as the challenge sent up front.
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

/// Generates a PKCE pair: a 64-byte random verifier (far above RFC 7636's 43-character
/// minimum) and its S256 challenge.
pub fn generate_pkce() -> Result<Pkce, OAuthError> {
    let mut bytes = [0u8; 64];
    getrandom::fill(&mut bytes).map_err(|_| OAuthError::Io(std::io::Error::other("no OS RNG")))?;
    let verifier = base64url_encode(&bytes);
    let challenge = base64url_encode(&Sha256::digest(verifier.as_bytes()));
    Ok(Pkce {
        verifier,
        challenge,
    })
}

/// A CSRF `state` nonce: 16 random bytes, hex-encoded (so it is safe unescaped in a query
/// string and in the HTML the callback page echoes back).
pub fn generate_state() -> Result<String, OAuthError> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|_| OAuthError::Io(std::io::Error::other("no OS RNG")))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Builds the authorization URL a browser is sent to. `redirect_uri` is always
/// `http://localhost:{CALLBACK_PORT}{CALLBACK_PATH}` in production; a test may pass a
/// different one to match its fake server.
pub fn authorize_url(
    endpoints: &Endpoints,
    state: &str,
    challenge: &str,
    redirect_uri: &str,
) -> String {
    let params = [
        ("response_type", "code"),
        ("client_id", CLIENT_ID),
        ("redirect_uri", redirect_uri),
        ("scope", SCOPE),
        ("code_challenge", challenge),
        ("code_challenge_method", "S256"),
        ("state", state),
        ("id_token_add_organizations", "true"),
        ("codex_cli_simplified_flow", "true"),
        ("originator", ORIGINATOR),
    ];
    let query: String = params
        .iter()
        .map(|(name, value)| format!("{name}={}", percent_encode(value)))
        .collect::<Vec<_>>()
        .join("&");
    format!("{}?{query}", endpoints.authorize_url)
}

/// The bound loopback socket a login waits for its one redirect on. Split from
/// [`wait_for_callback`] into `bind` + `wait` so a caller (or a test) can learn the actual
/// bound port before anything tries to connect to it — essential when `bind_addr`'s port is
/// `0` (an ephemeral port a test picks to avoid colliding with another test or a real login).
#[derive(Debug)]
pub struct CallbackListener {
    listener: TcpListener,
}

impl CallbackListener {
    /// Binds `bind_addr`. `AddrInUse` becomes [`OAuthError::PortBusy`] — the signal
    /// [`BrowserLogin::start`] uses to tell a caller to suggest the device-code flow instead,
    /// before anything has been shown to the user.
    pub async fn bind(bind_addr: SocketAddr) -> Result<Self, OAuthError> {
        let listener = TcpListener::bind(bind_addr).await.map_err(|source| {
            if source.kind() == std::io::ErrorKind::AddrInUse {
                OAuthError::PortBusy {
                    port: bind_addr.port(),
                }
            } else {
                OAuthError::Io(source)
            }
        })?;
        Ok(Self { listener })
    }

    /// The address actually bound (useful when [`Self::bind`] was asked for an ephemeral
    /// port).
    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// Waits for exactly one HTTP request to `path` and returns its authorization `code` —
    /// **one-shot**: whether that single request matches `expected_state` or not, this answers
    /// it and returns; the listener is then dropped, so a second connection attempt afterwards
    /// gets a refused connection, not a second chance.
    ///
    /// A mismatched `state` is refused (`Err(StateMismatch)`) **without** forwarding anything
    /// to the token endpoint — the one HTTP exchange this performs is only ever the local
    /// browser redirect, never a network call.
    pub async fn wait(
        self,
        path: &str,
        expected_state: &str,
        timeout: Duration,
    ) -> Result<String, OAuthError> {
        let (stream, _) = tokio::time::timeout(timeout, self.listener.accept())
            .await
            .map_err(|_| OAuthError::Timeout)?
            .map_err(OAuthError::Io)?;
        handle_one_callback(stream, path, expected_state).await
    }
}

/// [`CallbackListener::bind`] followed by [`CallbackListener::wait`], for a caller that has no
/// use for the bound address (production: the port is always the fixed [`CALLBACK_PORT`]).
pub async fn wait_for_callback(
    bind_addr: SocketAddr,
    path: &str,
    expected_state: &str,
    timeout: Duration,
) -> Result<String, OAuthError> {
    CallbackListener::bind(bind_addr)
        .await?
        .wait(path, expected_state, timeout)
        .await
}

/// Reads one GET's request line, answers it, and returns the outcome. The connection is
/// dropped on return either way — this is the "one-shot" half of [`wait_for_callback`]'s
/// contract; the caller never calls `accept` again.
async fn handle_one_callback(
    mut stream: TcpStream,
    path: &str,
    expected_state: &str,
) -> Result<String, OAuthError> {
    let request_line = read_request_line(&mut stream).await?;
    let result = parse_callback(&request_line, path, expected_state);
    let (status, body) = match &result {
        Ok(_) => (
            "200 OK",
            "Signed in. You can close this tab and return to the terminal.",
        ),
        Err(_) => (
            "400 Bad Request",
            "Sign-in was not completed. Return to the terminal.",
        ),
    };
    let _ = write_html_response(&mut stream, status, body).await;
    result
}

/// Parses `GET {path}?...` and checks it against `expected_state`. Pulled out of
/// [`handle_one_callback`] so the state-mismatch and missing-code cases have a direct unit
/// test with no socket involved.
fn parse_callback(
    request_line: &str,
    expected_path: &str,
    expected_state: &str,
) -> Result<String, OAuthError> {
    let mut parts = request_line.split_whitespace();
    let _method = parts.next();
    let target = parts.next().unwrap_or("");
    let (req_path, query) = target.split_once('?').unwrap_or((target, ""));
    if req_path != expected_path {
        return Err(OAuthError::UnexpectedCallback {
            path: req_path.to_owned(),
        });
    }
    let params = parse_query(query);
    if let Some(error) = params.get("error") {
        let reason = params
            .get("error_description")
            .cloned()
            .unwrap_or_else(|| error.clone());
        return Err(OAuthError::AuthorizationDenied { reason });
    }
    let state = params.get("state").cloned().unwrap_or_default();
    if state != expected_state {
        return Err(OAuthError::StateMismatch);
    }
    params.get("code").cloned().ok_or(OAuthError::MissingCode)
}

const MAX_REQUEST_LINE_BYTES: usize = 8 * 1024;

/// Reads bytes until the request line's terminating `\r\n` (a GET redirect has no body, so
/// nothing past the first line is needed).
async fn read_request_line(stream: &mut TcpStream) -> Result<String, OAuthError> {
    let mut buffer = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let n = stream.read(&mut byte).await.map_err(OAuthError::Io)?;
        if n == 0 {
            return Err(OAuthError::Io(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "connection closed before a request line arrived",
            )));
        }
        if byte[0] == b'\n' {
            if buffer.last() == Some(&b'\r') {
                buffer.pop();
            }
            break;
        }
        buffer.push(byte[0]);
        if buffer.len() > MAX_REQUEST_LINE_BYTES {
            return Err(OAuthError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "request line too long",
            )));
        }
    }
    Ok(String::from_utf8_lossy(&buffer).into_owned())
}

async fn write_html_response(
    stream: &mut TcpStream,
    status: &str,
    body: &str,
) -> std::io::Result<()> {
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: \
         {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await?;
    stream.shutdown().await
}

/// [`wait_for_callback`] bound to the fixed production address
/// (`127.0.0.1:{CALLBACK_PORT}{CALLBACK_PATH}`). A test exercises the listener logic through
/// [`wait_for_callback`] directly on an ephemeral port instead; this wrapper is what makes the
/// fixed-port behavior (and its busy-port error) testable in isolation (`tests::port_busy_*`).
pub async fn wait_for_production_callback(
    expected_state: &str,
    timeout: Duration,
) -> Result<String, OAuthError> {
    wait_for_callback(
        SocketAddr::from(([127, 0, 0, 1], CALLBACK_PORT)),
        CALLBACK_PATH,
        expected_state,
        timeout,
    )
    .await
}

/// The full browser flow: generates PKCE + state, returns the authorization URL and a future
/// that waits for the callback and exchanges the code. Split this way so a caller can print /
/// open the URL *before* awaiting the callback (the listener is already bound by the time this
/// returns, so the redirect can never race it).
pub struct BrowserLogin {
    pub authorize_url: String,
    state: String,
    pkce: Pkce,
    redirect_uri: String,
}

impl std::fmt::Debug for BrowserLogin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BrowserLogin")
            .field("authorize_url", &self.authorize_url)
            .field("state", &self.state)
            .field("pkce_verifier", &"<redacted>")
            .field("redirect_uri", &self.redirect_uri)
            .finish()
    }
}

impl BrowserLogin {
    /// Binds the production callback port and builds the authorization URL. `Err(PortBusy)`
    /// here is the signal to suggest the device-code flow instead — nothing has been shown to
    /// the user yet.
    pub async fn start(endpoints: &Endpoints) -> Result<(Self, CallbackListener), OAuthError> {
        let addr = SocketAddr::from(([127, 0, 0, 1], CALLBACK_PORT));
        let listener = CallbackListener::bind(addr).await?;
        let state = generate_state()?;
        let pkce = generate_pkce()?;
        let redirect_uri = format!("http://localhost:{CALLBACK_PORT}{CALLBACK_PATH}");
        let url = authorize_url(endpoints, &state, &pkce.challenge, &redirect_uri);
        Ok((
            Self {
                authorize_url: url,
                state,
                pkce,
                redirect_uri,
            },
            listener,
        ))
    }

    /// Waits for the callback on the given listener and exchanges the code for tokens.
    pub async fn finish(
        self,
        endpoints: &Endpoints,
        listener: CallbackListener,
        timeout: Duration,
    ) -> Result<TokenResponse, OAuthError> {
        let code = listener.wait(CALLBACK_PATH, &self.state, timeout).await?;
        exchange_code(endpoints, &code, &self.pkce.verifier, &self.redirect_uri).await
    }
}

/// Exchanges an authorization code for tokens.
pub async fn exchange_code(
    endpoints: &Endpoints,
    code: &str,
    verifier: &str,
    redirect_uri: &str,
) -> Result<TokenResponse, OAuthError> {
    let payload = post_token(
        endpoints,
        &[
            ("grant_type", "authorization_code"),
            ("client_id", CLIENT_ID),
            ("code", code),
            ("code_verifier", verifier),
            ("redirect_uri", redirect_uri),
        ],
    )
    .await?;
    build_response(payload, None)
}

/// Refreshes a token. OpenAI may or may not rotate the refresh token on a given call; when it
/// does not, the caller's `refresh_token` is kept (verified `openai-codex.ts:379`:
/// `tokenData.refresh_token || refreshToken`).
pub async fn refresh_token(
    endpoints: &Endpoints,
    refresh_token: &str,
) -> Result<TokenResponse, OAuthError> {
    let payload = post_token(
        endpoints,
        &[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", CLIENT_ID),
        ],
    )
    .await?;
    build_response(payload, Some(refresh_token))
}

#[derive(Deserialize, Default)]
struct TokenPayload {
    access_token: Option<String>,
    refresh_token: Option<String>,
    expires_in: Option<u64>,
}

fn build_client() -> Result<reqwest::Client, OAuthError> {
    reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()
        .map_err(|error| OAuthError::Request(error.to_string()))
}

async fn post_token(
    endpoints: &Endpoints,
    form: &[(&str, &str)],
) -> Result<TokenPayload, OAuthError> {
    let client = build_client()?;
    let response = client
        .post(&endpoints.token_url)
        .form(form)
        .send()
        .await
        .map_err(|error| OAuthError::Request(error.to_string()))?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(OAuthError::TokenEndpoint {
            status: status.as_u16(),
            message: format_token_error(&body),
        });
    }
    response
        .json::<TokenPayload>()
        .await
        .map_err(|_| OAuthError::MalformedToken)
}

/// Pulls `error`/`error_description` out of an OAuth error body for a one-line message; falls
/// back to the raw (trimmed) body when it is not JSON-shaped.
fn format_token_error(body: &str) -> String {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return "no response body".to_owned();
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) else {
        return trimmed.to_owned();
    };
    let error = value.get("error").and_then(serde_json::Value::as_str);
    let description = value
        .get("error_description")
        .and_then(serde_json::Value::as_str);
    match (error, description) {
        (Some(e), Some(d)) if e != d => format!("{e}: {d}"),
        (Some(e), _) => e.to_owned(),
        (None, Some(d)) => d.to_owned(),
        (None, None) => trimmed.to_owned(),
    }
}

fn build_response(
    payload: TokenPayload,
    fallback_refresh: Option<&str>,
) -> Result<TokenResponse, OAuthError> {
    let access = payload
        .access_token
        .filter(|s| !s.is_empty())
        .ok_or(OAuthError::MalformedToken)?;
    let refresh = payload
        .refresh_token
        .filter(|s| !s.is_empty())
        .or_else(|| fallback_refresh.map(str::to_owned))
        .ok_or(OAuthError::MalformedToken)?;
    let expires_in = payload.expires_in.ok_or(OAuthError::MalformedToken)?;
    let account_id = decode_account_id(&access)?;
    Ok(TokenResponse {
        access,
        refresh,
        expires_at_millis: now_millis().saturating_add(expires_in.saturating_mul(1000)),
        account_id,
    })
}

/// Decodes the `chatgpt_account_id` claim from an access token's `https://api.openai.com/auth`
/// claim. **No signature check**: this token is bearer-authenticated directly against
/// OpenAI's API on every request a later slice makes with it, so a forged claim with no valid
/// signature buys an attacker nothing — the claim is read here only to label Radian's own
/// requests (and, at login, to confirm the token is actually ChatGPT-shaped) and never to
/// authorize a decision inside Radian itself. Verifying the signature would need a JWKS fetch
/// and key-rotation handling for no corresponding security benefit.
pub fn decode_account_id(access_token: &str) -> Result<String, OAuthError> {
    let payload = decode_jwt_payload(access_token)?;
    payload
        .get(JWT_CLAIM_PATH)
        .and_then(|claim| claim.get("chatgpt_account_id"))
        .and_then(serde_json::Value::as_str)
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
        .ok_or(OAuthError::MalformedToken)
}

fn decode_jwt_payload(token: &str) -> Result<serde_json::Value, OAuthError> {
    let mut parts = token.split('.');
    let _header = parts.next().ok_or(OAuthError::MalformedToken)?;
    let payload_segment = parts.next().ok_or(OAuthError::MalformedToken)?;
    let bytes = base64url_decode(payload_segment)?;
    serde_json::from_slice(&bytes).map_err(|_| OAuthError::MalformedToken)
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

// --- the device-code flow (RFC 8628-shaped; OpenAI's own pending/expired signaling, verified
// against `openai-codex.ts:248-337`) ---

/// What `/login`'s device-code step shows the user right away, before any polling starts.
pub struct DeviceStart {
    pub device_auth_id: String,
    pub user_code: String,
    /// The provider's requested poll interval, floored at [`DEVICE_POLL_MIN_INTERVAL`].
    pub interval: Duration,
}

/// Starts the device-code flow: asks for a user code to show, and the id used to poll for it.
pub async fn device_start(endpoints: &Endpoints) -> Result<DeviceStart, OAuthError> {
    let client = build_client()?;
    let response = client
        .post(&endpoints.device_usercode_url)
        .json(&serde_json::json!({ "client_id": CLIENT_ID }))
        .send()
        .await
        .map_err(|error| OAuthError::Request(error.to_string()))?;
    let status = response.status();
    if !status.is_success() {
        return Err(OAuthError::DeviceInit {
            status: status.as_u16(),
        });
    }
    #[derive(Deserialize)]
    struct Init {
        device_auth_id: Option<String>,
        user_code: Option<String>,
        interval: Option<serde_json::Value>,
    }
    let init: Init = response
        .json()
        .await
        .map_err(|_| OAuthError::MalformedToken)?;
    let device_auth_id = init.device_auth_id.ok_or(OAuthError::MalformedToken)?;
    let user_code = init.user_code.ok_or(OAuthError::MalformedToken)?;
    let interval_secs = init
        .interval
        .and_then(|value| {
            value
                .as_u64()
                .or_else(|| value.as_str().and_then(|s| s.parse().ok()))
        })
        .unwrap_or(5);
    Ok(DeviceStart {
        device_auth_id,
        user_code,
        interval: Duration::from_secs(interval_secs).max(DEVICE_POLL_MIN_INTERVAL),
    })
}

/// One device-code poll's outcome.
#[derive(Debug)]
pub enum DevicePoll {
    /// The user has not finished the browser step yet; poll again after the interval.
    Pending,
    /// Sign-in finished; the credential is ready.
    Done(TokenResponse),
}

/// Polls once. Pending is the normal case while the user has not yet entered the code
/// (OpenAI's own device endpoint answers `403`/`404` for that, not an RFC 8628 error body —
/// verified `openai-codex.ts:307-310`); a JSON `{"error":"expired_token"}` body on any other
/// non-2xx status is reported as [`OAuthError::DeviceExpired`], and any other failure is
/// [`OAuthError::DevicePoll`].
pub async fn device_poll_once(
    endpoints: &Endpoints,
    device_auth_id: &str,
    user_code: &str,
) -> Result<DevicePoll, OAuthError> {
    let client = build_client()?;
    let response = client
        .post(&endpoints.device_token_url)
        .json(&serde_json::json!({
            "device_auth_id": device_auth_id,
            "user_code": user_code,
        }))
        .send()
        .await
        .map_err(|error| OAuthError::Request(error.to_string()))?;
    let status = response.status();
    if status.as_u16() == 403 || status.as_u16() == 404 {
        return Ok(DevicePoll::Pending);
    }
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        if serde_json::from_str::<serde_json::Value>(&body)
            .ok()
            .and_then(|value| {
                value
                    .get("error")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned)
            })
            .as_deref()
            == Some("expired_token")
        {
            return Err(OAuthError::DeviceExpired);
        }
        return Err(OAuthError::DevicePoll {
            status: status.as_u16(),
        });
    }
    #[derive(Deserialize)]
    struct Poll {
        authorization_code: Option<String>,
        code_verifier: Option<String>,
    }
    let poll: Poll = response
        .json()
        .await
        .map_err(|_| OAuthError::MalformedToken)?;
    let code = poll.authorization_code.ok_or(OAuthError::MalformedToken)?;
    let verifier = poll.code_verifier.ok_or(OAuthError::MalformedToken)?;
    let payload = post_token(
        endpoints,
        &[
            ("grant_type", "authorization_code"),
            ("client_id", CLIENT_ID),
            ("code", &code),
            ("code_verifier", &verifier),
            ("redirect_uri", DEVICE_REDIRECT_URI),
        ],
    )
    .await?;
    Ok(DevicePoll::Done(build_response(payload, None)?))
}

/// Polls until done, expired, or `max_polls` is reached, sleeping `interval` between attempts
/// (the first poll fires immediately). A test passes a near-zero `interval` so the "pending
/// then success" and "expired" cases run fast.
pub async fn poll_device_until_done(
    endpoints: &Endpoints,
    start: &DeviceStart,
    interval: Duration,
    max_polls: u32,
) -> Result<TokenResponse, OAuthError> {
    for attempt in 0..max_polls {
        if attempt > 0 {
            tokio::time::sleep(interval).await;
        }
        match device_poll_once(endpoints, &start.device_auth_id, &start.user_code).await? {
            DevicePoll::Done(token) => return Ok(token),
            DevicePoll::Pending => {}
        }
    }
    Err(OAuthError::Timeout)
}

/// The `auth.json` provider id this module's credential is stored under (ADR 0026
/// decision 1).
pub const PROVIDER_ID: &str = "chatgpt";

/// Why [`ensure_fresh`] could not return a usable access token.
#[derive(Debug)]
pub enum RefreshError {
    /// There is no `chatgpt` `oauth` entry in `auth.json` at all.
    NotLoggedIn,
    /// The refresh request itself failed. The message already says to log in again.
    Failed(String),
    /// Reading or writing `auth.json` failed.
    Auth(crate::auth::AuthError),
}

impl std::fmt::Display for RefreshError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotLoggedIn => write!(f, "not logged in to ChatGPT; run `/login chatgpt`"),
            Self::Failed(message) => write!(f, "{message}"),
            Self::Auth(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for RefreshError {}

/// Writes `token` as the `chatgpt` `oauth` entry (login and refresh share this).
pub fn store_token(
    store: &mut crate::auth::AuthStore,
    token: &TokenResponse,
) -> Result<(), crate::auth::AuthError> {
    store.upsert_oauth(
        PROVIDER_ID,
        &crate::auth::OAuthEntry {
            access: token.access.clone(),
            refresh: token.refresh.clone(),
            expires_at_millis: token.expires_at_millis,
        },
    )
}

/// A currently-valid `chatgpt` access token, refreshing first if the stored one is within
/// [`REFRESH_MARGIN`] of expiring (or already expired). [`ensure_fresh_in`] is the same against
/// an explicit store (what the tests use); this one opens the process's own `auth.json`.
pub async fn ensure_fresh(endpoints: &Endpoints) -> Result<String, RefreshError> {
    let mut store = crate::auth::AuthStore::open().map_err(RefreshError::Auth)?;
    ensure_fresh_in(endpoints, &mut store).await
}

/// [`ensure_fresh`] against an explicit store.
///
/// Concurrent refreshes from two sessions reuse `auth.json`'s existing write path
/// (`AuthStore::write`: temp file + atomic rename) rather than adding a file lock: the file
/// can never land corrupted (a rename is atomic), and the documented tradeoff is the same one
/// `upsert_api_key` already accepts — the later write wins, so the earlier session's freshly
/// refreshed token can be overwritten by a session that started its own refresh moments
/// earlier with the old one. That is a *lost* refresh, not a corruption: the overwriting
/// session's own token is itself fresh (it just finished refreshing), and the next refresh
/// cycle before either token expires reconciles it. If OpenAI rotates refresh tokens
/// (single-use), the loser's *next* refresh attempt fails with [`RefreshError::Failed`],
/// which already tells the user to log in again — an acceptable outcome for a race that
/// requires two sessions to refresh within the same `auth.json` write's window.
pub async fn ensure_fresh_in(
    endpoints: &Endpoints,
    store: &mut crate::auth::AuthStore,
) -> Result<String, RefreshError> {
    let entry = store
        .oauth_entry(PROVIDER_ID)
        .ok_or(RefreshError::NotLoggedIn)?;
    let margin_deadline = now_millis().saturating_add(REFRESH_MARGIN.as_millis() as u64);
    if entry.expires_at_millis > margin_deadline {
        return Ok(entry.access);
    }
    let refreshed = refresh_token(endpoints, &entry.refresh)
        .await
        .map_err(|error| {
            RefreshError::Failed(format!(
                "could not refresh the ChatGPT sign-in ({error}); run `/login chatgpt` again"
            ))
        })?;
    store_token(store, &refreshed).map_err(RefreshError::Auth)?;
    Ok(refreshed.access)
}

// --- small, dependency-free encoders (base64url RFC 4648 §5, percent-encoding) ---

const BASE64URL_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

fn base64url_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(BASE64URL_ALPHABET[((n >> 18) & 0x3f) as usize] as char);
        out.push(BASE64URL_ALPHABET[((n >> 12) & 0x3f) as usize] as char);
        if chunk.len() > 1 {
            out.push(BASE64URL_ALPHABET[((n >> 6) & 0x3f) as usize] as char);
        }
        if chunk.len() > 2 {
            out.push(BASE64URL_ALPHABET[(n & 0x3f) as usize] as char);
        }
    }
    out
}

fn base64url_decode(input: &str) -> Result<Vec<u8>, OAuthError> {
    let mut reverse = [255u8; 256];
    for (index, &symbol) in BASE64URL_ALPHABET.iter().enumerate() {
        reverse[symbol as usize] = index as u8;
    }
    let mut bits: u32 = 0;
    let mut bit_count = 0u32;
    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    for byte in input.bytes() {
        let value = reverse[byte as usize];
        if value == 255 {
            return Err(OAuthError::MalformedToken);
        }
        bits = (bits << 6) | u32::from(value);
        bit_count += 6;
        if bit_count >= 8 {
            bit_count -= 8;
            out.push(((bits >> bit_count) & 0xff) as u8);
        }
    }
    Ok(out)
}

/// Percent-encodes everything outside RFC 3986's unreserved set — enough for the query
/// parameters this module builds (none contain raw binary; all are ASCII identifiers, a
/// base64url string, or a fixed URL).
fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).ok();
            if let Some(decoded) = hex.and_then(|hex| u8::from_str_radix(hex, 16).ok()) {
                out.push(decoded);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn parse_query(query: &str) -> std::collections::HashMap<String, String> {
    query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .filter_map(|pair| pair.split_once('='))
        .map(|(name, value)| (percent_decode(name), percent_decode(value)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64url_round_trips_and_matches_a_known_vector() {
        // RFC 4648's own test vector ("Man" -> "TWFu"), which happens to use no `+`/`/`/pad,
        // so it is also a valid base64url vector.
        assert_eq!(base64url_encode(b"Man"), "TWFu");
        for input in [
            b"".as_slice(),
            b"f",
            b"fo",
            b"foo",
            b"foob",
            b"fooba",
            b"foobar",
        ] {
            let encoded = base64url_encode(input);
            assert!(!encoded.contains('+') && !encoded.contains('/') && !encoded.contains('='));
            assert_eq!(base64url_decode(&encoded).unwrap(), input);
        }
    }

    #[test]
    fn pkce_challenge_is_the_sha256_of_the_verifier() {
        let pkce = generate_pkce().unwrap();
        assert_eq!(pkce.verifier.len(), 86, "64 bytes base64url = 86 chars");
        let expected = base64url_encode(&Sha256::digest(pkce.verifier.as_bytes()));
        assert_eq!(pkce.challenge, expected);
        // Two calls never collide.
        let other = generate_pkce().unwrap();
        assert_ne!(pkce.verifier, other.verifier);
    }

    #[test]
    fn state_is_random_hex() {
        let a = generate_state().unwrap();
        let b = generate_state().unwrap();
        assert_ne!(a, b);
        assert_eq!(a.len(), 32);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn authorize_url_carries_the_fixed_constants_and_originator_radian() {
        let endpoints = Endpoints::default();
        let url = authorize_url(
            &endpoints,
            "the-state",
            "the-challenge",
            "http://localhost:1455/auth/callback",
        );
        assert!(url.starts_with("https://auth.openai.com/oauth/authorize?"));
        assert!(url.contains("client_id=app_EMoamEEZ73f0CkXaXp7hrann"));
        assert!(url.contains("code_challenge=the-challenge"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("state=the-state"));
        assert!(url.contains("originator=radian"));
        assert!(
            !url.contains("originator=codex"),
            "must not impersonate Codex"
        );
        assert!(url.contains(&percent_encode("http://localhost:1455/auth/callback")));
    }

    #[test]
    fn a_matching_callback_yields_the_code() {
        let line = "GET /auth/callback?code=abc123&state=xyz HTTP/1.1";
        assert_eq!(
            parse_callback(line, "/auth/callback", "xyz").unwrap(),
            "abc123"
        );
    }

    #[test]
    fn a_state_mismatch_is_refused_without_reading_the_code() {
        let line = "GET /auth/callback?code=abc123&state=WRONG HTTP/1.1";
        let error = parse_callback(line, "/auth/callback", "xyz").unwrap_err();
        assert!(matches!(error, OAuthError::StateMismatch), "{error}");
    }

    #[test]
    fn a_denial_is_reported_without_a_code() {
        let line = "GET /auth/callback?error=access_denied&error_description=user%20said%20no&state=xyz \
                     HTTP/1.1";
        let error = parse_callback(line, "/auth/callback", "xyz").unwrap_err();
        match error {
            OAuthError::AuthorizationDenied { reason } => assert_eq!(reason, "user said no"),
            other => panic!("{other}"),
        }
    }

    #[test]
    fn a_request_to_another_path_is_not_this_callback() {
        let line = "GET /favicon.ico HTTP/1.1";
        let error = parse_callback(line, "/auth/callback", "xyz").unwrap_err();
        assert!(
            matches!(error, OAuthError::UnexpectedCallback { .. }),
            "{error}"
        );
    }

    #[test]
    fn no_error_ever_prints_a_secret() {
        for error in [
            OAuthError::PortBusy { port: 1455 },
            OAuthError::Timeout,
            OAuthError::StateMismatch,
            OAuthError::MissingCode,
            OAuthError::AuthorizationDenied {
                reason: "denied".into(),
            },
            OAuthError::Request("connect failed".into()),
            OAuthError::TokenEndpoint {
                status: 400,
                message: "invalid_grant".into(),
            },
            OAuthError::MalformedToken,
            OAuthError::DeviceExpired,
        ] {
            let text = error.to_string();
            assert!(!text.contains("sk-"), "{text}");
        }
    }
}
