//! `radian hub`'s HTTP API (ADR 0048 decision 16): loopback by default, a bearer token on
//! every request, the read routes that return exactly what `radian arc list --json` and
//! `radian arc show --json` print, and the owner's write routes.
//!
//! The listener binds `<[hub] bind>:<[hub] port>` and nothing else, and a bind that is not
//! loopback prints one warning at startup. The server takes `hub.lock`
//! before it binds, so a second hub is refused by the lock and not by the port; a port another
//! program holds is a startup error that names it. The token file is read at start and
//! re-read whenever its mtime changed, one `stat` per request, so `radian hub token rotate`
//! takes effect on the next request without a restart.
//!
//! The HTTP accept loop runs on a thread of its own, and so does the hub's scheduling loop:
//! the hub's own thread reconciles the registry, composes the status lines and joins the loops,
//! while each repository's pass runs on its own thread (ADR 0048 decision 16). A slow pass
//! therefore delays no request.
//!
//! The read routes are `GET` and answer JSON, except `GET /v1/events`, which is a
//! server-sent-event stream; the owner actions are
//! `POST /v1/repos/<name>/items/<id>/<action>`. The router is hand-written. The list and show
//! bodies come from [`crate::store::cli::list_json_text`] and
//! [`crate::store::cli::show_json_text`], and the write routes call the same library functions
//! the arc TUI calls, so a field or behavior change in the CLI changes the route.
//!
//! The route work (files, git, the token's `stat`) runs on `spawn_blocking`, never on the
//! runtime's async threads, so the scheduling loop's threads (ADR 0048 decision 13) are not
//! blocked by a request.

use std::collections::VecDeque;
use std::convert::Infallible;
use std::fs;
use std::future::poll_fn;
use std::io::IsTerminal;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant, SystemTime};

use hyper::body::{Body, Bytes, Frame, SizeHint};
use hyper::header::{
    AUTHORIZATION, CACHE_CONTROL, CONNECTION, CONTENT_LENGTH, CONTENT_TYPE, HeaderValue,
    WWW_AUTHENTICATE,
};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde_json::{Value, json};
use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::TcpListener;
use tokio::sync::{broadcast, mpsc};
// `--url` is a URL, not a shape this crate splits by hand (ADR 0048 §16): the WHATWG parser
// checks the scheme, the host and an IPv6 literal, and the checks beside it close what that
// parser leaves open (`http:///hub`, `http://host:` and `%zz`).
use url::{Host, Url};

use crate::gate::Stage;
use crate::gate::Verdict;
use crate::hub::lock::HubLock;
use crate::hub::registry::Registry;
use crate::hub::sessions::{self, SessionError, SessionHub};
use crate::hub::token;
use crate::read;
use crate::store::attempt_ops;
use crate::store::{Error, Store, cli, model, plan_ops};
use crate::transitions::{CHANNEL_INTERVAL_SECS, Poller, Repo, Transition};
use crate::{attempt, review};

/// The gap between two `: keep-alive` comments on an event stream that has nothing to send.
pub const KEEP_ALIVE_SECS: u64 = 15;

/// The API version `GET /v1/version` reports. It names the route set and its behavior; the
/// plan and task file shape is `schema_version` (ADR 0048 decision 5) and changes on its own.
/// Version `3` adds the session `close` route; version `2` added the session surface, and
/// version `1`'s routes and their behavior are unchanged.
pub const API_VERSION: u32 = 3;

/// The most diff text `GET …/diff/<path>` returns, in bytes.
pub const DIFF_TEXT_LIMIT: usize = 256 * 1024;

/// The address the hub binds: `[hub] bind` on `[hub] port` (ADR 0048 §16).
#[must_use]
pub fn bind_address(bind: IpAddr, port: u16) -> SocketAddr {
    SocketAddr::new(bind, port)
}

/// The one warning a non-loopback bind prints on stderr at startup (ADR 0048 §16): the
/// bearer token and every request travel unencrypted on that network. `None` for a loopback
/// address, so the default bind prints nothing.
fn bind_warning(address: IpAddr) -> Option<String> {
    (!address.is_loopback()).then(|| {
        format!(
            "hub: warning: {address} is not a loopback address; the bearer token and every \
             request travel unencrypted on that network. Use `tailscale serve` or an ssh \
             forward for an encrypted connection."
        )
    })
}

/// Runs the hub until the process ends: it installs the stop-signal handler, reads the
/// configuration, creates the token when it is missing, takes `hub.lock`, binds
/// `[hub] bind` on the port, prints `hub: listening on <addr>` as its first stdout line,
/// and then runs the hub's own thread (registry re-reads, status lines and joins) while the
/// HTTP API serves on a thread of its own and every repository's pass runs on its own thread
/// (ADR 0048 decision 16). A bind that is not loopback prints one warning line on stderr.
/// `config` is the top-level `--config` file the loop reads its `[hub]` keys from, the
/// session scanner its `[hub.hooks]` commands, and every hub-held process the path it is
/// started with (ADR 0048 §16). When no explicit file was given the machine's default path is
/// resolved, so the first start still writes the commented default template there (ADR 0008
/// §2).
///
/// The SIGINT and SIGTERM handler comes first, before the lock, the bind and the first line:
/// a test or a person stops the hub as soon as it announces itself, and the default action
/// would kill the process instead of letting every loop release its lock. The `exec` after a
/// rebuilt binary resets caught signals, so this is also what arms the process that starts
/// after the restart.
///
/// # Errors
///
/// The hub directory or the token cannot be prepared, the configuration cannot be read,
/// `hub.lock` is held by another hub, or the port cannot be bound.
pub fn serve(port: u16, config: Option<&Path>) -> Result<(), Error> {
    crate::run::catch_stop_signals();
    // ADR 0008 §2: a fresh install has no config file. The hub creates the commented default at
    // the default location, exactly as the TUI and `radian exec` do, so a machine whose first
    // radian process is the hub still gets the template. An explicit `--config`/`RADIAN_CONFIG`
    // path is never created; a failure warns once and the hub continues.
    match radian_host::ensure_default_config(config) {
        Ok(Some(path)) => eprintln!("created a default config at {}", path.display()),
        Ok(None) => {}
        Err(warning) => eprintln!("[config] {warning}; continuing without creating it"),
    }
    let registry = Registry::open()?;
    let dir = registry.dir().to_path_buf();
    radian_host::ensure_private_dir(&dir)
        .map_err(|error| Error::Failed(format!("cannot create {}: {error}", dir.display())))?;
    let value = token::ensure(&dir)?;
    // Before any bind or any repository: two hubs are refused by the lock, never by the port
    // (ADR 0048 §16).
    let lock = HubLock::take(&dir)?;
    // A bad `[hub] bind` is a configuration error before anything binds. The configuration is
    // read once here and shared by the listener, the reclaim window and the hooks.
    let config_value = load_config(config)?;
    let address = bind_address(config_value.hub.bind, port);
    let listener = bind_listener(address)?;
    let bound = listener
        .local_addr()
        .map_err(|error| Error::Failed(format!("cannot read the hub address: {error}")))?;
    radian_host::write_line(&format!("hub: listening on {bound}"))
        .map_err(|error| Error::Failed(format!("cannot print the hub address: {error}")))?;
    if let Some(warning) = bind_warning(bound.ip()) {
        eprintln!("{warning}");
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| Error::Failed(format!("cannot start the hub runtime: {error}")))?;
    let tokens = Arc::new(Mutex::new(TokenState::new(&dir, value)));
    let http_registry = registry.clone();
    // The reclaim window a hub-held process is told (ADR 0048 §16). `[hub] session_idle_secs`
    // comes from the same file the scheduling loop reads.
    let idle_secs = session_idle_secs(&config_value);
    // `[hub.hooks]` is read once at start; the hook list a running hub uses does not change
    // until it is restarted (the same rule as the other `[hub]` keys).
    let hooks = config_value.hub.hooks;
    // The file the hub itself reads, passed on to every hub-held process, so an explicit
    // `--config` reaches the sessions the hub starts (ADR 0048 §16). It is made absolute here,
    // while the hub's own working directory is still the one the flag was written against: a
    // child starts in the repository (`spawn_held`), where a relative path names another file.
    let config_path = match radian_host::config::resolved_path(config) {
        Some(path) if path.is_absolute() => Some(path),
        Some(path) => {
            let cwd = std::env::current_dir().map_err(|error| {
                Error::Failed(format!(
                    "cannot resolve the configuration path {}: {error}",
                    path.display()
                ))
            })?;
            Some(absolute_config(path, &cwd))
        }
        None => None,
    };
    // The live-session scanner: its own thread, its own blocking socket work, so no request and
    // no scheduling pass waits on a session's control port.
    let http_sessions = SessionHub::with_hooks(
        dir.parent()
            .map(std::path::Path::to_path_buf)
            .unwrap_or_else(|| dir.clone()),
        idle_secs,
        hooks,
        config_path,
    );
    http_sessions.start();
    // The accept loop has a thread and a runtime of its own; no request waits on a pass.
    let sessions_for_shutdown = Arc::clone(&http_sessions);
    std::thread::spawn(move || {
        runtime.block_on(serve_http(listener, http_registry, tokens, http_sessions));
    });
    let emit = Arc::new(|text: &str| {
        let _ = radian_host::write_stdout(text);
    });
    let result = super::r#loop::run(&registry, emit, lock, config);
    // SIGINT/SIGTERM (or a failed restart) stops the hub-held sessions cleanly before the
    // process leaves; `hub.lock` was released by the loop just above.
    sessions_for_shutdown.shutdown();
    result
}

/// `path` as a hub-held child is told it, absolute against `cwd`. A relative `--config
/// alt.toml` is the hub's file when the hub reads it, but the child runs in the repository
/// (`spawn_held`), so it must be given the absolute path. It is joined, not canonicalized: a
/// symlinked path still names the file the hub read (ADR 0048 §16).
fn absolute_config(path: PathBuf, cwd: &Path) -> PathBuf {
    if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    }
}

/// The configuration the hub's own loop reads, for the reclaim window.
fn load_config(explicit: Option<&Path>) -> Result<radian_host::config::Config, Error> {
    let path = radian_host::config::resolved_path(explicit);
    radian_host::config::load_path(path.as_deref())
        .map_err(|error| Error::Failed(format!("cannot read the configuration: {error}")))
}

/// `[hub] session_idle_secs` (ADR 0048 §16). `RADIAN_HUB_SESSION_IDLE_SECS` is a test-only
/// seam: a hub test needs a window shorter than the documented sixty-second minimum to see a
/// process reclaim itself and the next prompt resume it. radian never sets it and the manual
/// does not document it; `crates/radian-host/tests/env_docs.rs` lists it in `TEST_ONLY`.
fn session_idle_secs(config: &radian_host::config::Config) -> u64 {
    std::env::var("RADIAN_HUB_SESSION_IDLE_SECS")
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|secs| *secs > 0)
        .unwrap_or(config.hub.session_idle_secs)
}

/// Binds `address` with `SO_REUSEADDR`, so a rebuilt binary that `exec`s the hub binds the
/// same port while the previous process's last connections are still in `TIME_WAIT`. Without
/// it the new hub fails with `Address already in use` and exits `1` (ADR 0048 decision 16).
fn bind_listener(address: SocketAddr) -> Result<std::net::TcpListener, Error> {
    let domain = match address {
        SocketAddr::V4(_) => Domain::IPV4,
        SocketAddr::V6(_) => Domain::IPV6,
    };
    let socket = Socket::new(domain, Type::STREAM, Some(Protocol::TCP))
        .map_err(|error| Error::Failed(format!("cannot open a socket for {address}: {error}")))?;
    socket
        .set_reuse_address(true)
        .map_err(|error| Error::Failed(format!("cannot reuse {address}: {error}")))?;
    socket
        .bind(&address.into())
        .map_err(|error| Error::Failed(format!("cannot bind {address}: {error}")))?;
    socket
        .listen(1024)
        .map_err(|error| Error::Failed(format!("cannot listen on {address}: {error}")))?;
    let listener: std::net::TcpListener = socket.into();
    listener
        .set_nonblocking(true)
        .map_err(|error| Error::Failed(format!("cannot prepare {address}: {error}")))?;
    Ok(listener)
}

/// Serves the hub's HTTP API on `listener` until the process ends.
async fn serve_http(
    listener: std::net::TcpListener,
    registry: Registry,
    tokens: Arc<Mutex<TokenState>>,
    sessions: Arc<SessionHub>,
) {
    let listener = match TcpListener::from_std(listener) {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("hub: cannot use the hub listener: {error}");
            return;
        }
    };
    loop {
        let (stream, _) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(error) => {
                // A transient accept error (EMFILE, ECONNABORTED) must not kill the API for
                // the rest of the hub's life while it keeps its locks: back off briefly and
                // keep accepting.
                eprintln!("hub: cannot accept a hub connection: {error}");
                tokio::time::sleep(Duration::from_millis(250)).await;
                continue;
            }
        };
        let registry = registry.clone();
        let tokens = Arc::clone(&tokens);
        let sessions = Arc::clone(&sessions);
        tokio::spawn(async move {
            let service = service_fn(move |request| {
                let registry = registry.clone();
                let tokens = Arc::clone(&tokens);
                let sessions = Arc::clone(&sessions);
                async move {
                    let method = request.method().clone();
                    let path = request.uri().path().to_owned();
                    let query = request.uri().query().map(str::to_owned);
                    let header = request
                        .headers()
                        .get(AUTHORIZATION)
                        .and_then(|value| value.to_str().ok())
                        .map(str::to_owned);
                    // Authorization is decided before the body is read: an unauthenticated
                    // request never makes the hub buffer the body it sends.
                    if !authorized(&tokens, header).await {
                        return Ok::<_, Infallible>(unauthorized_response());
                    }
                    // Decide the route, the method and the body limit before reading any
                    // body: an unknown route or a wrong method answers `404`/`405`, and a
                    // read route answers without the body, whatever the client declares or
                    // sends.
                    let route = route(&path);
                    if matches!(route, Route::Unknown) {
                        return Ok::<_, Infallible>(error_response(
                            StatusCode::NOT_FOUND,
                            "no such route",
                        ));
                    }
                    if !accepts(&route, &method) {
                        return Ok::<_, Infallible>(error_response(
                            StatusCode::METHOD_NOT_ALLOWED,
                            "method not allowed",
                        ));
                    }
                    let body = match body_limit(&route, &method) {
                        Some(limit) => match request_body(request, limit).await {
                            RequestBody::Bytes(body) => body,
                            RequestBody::TooLarge => {
                                return Ok::<_, Infallible>(error_response(
                                    StatusCode::PAYLOAD_TOO_LARGE,
                                    "request body too large",
                                ));
                            }
                        },
                        None => Bytes::new(),
                    };
                    let response = tokio::task::spawn_blocking(move || {
                        handle(method, &path, query.as_deref(), &body, &registry, &sessions)
                    })
                    .await
                    .unwrap_or_else(|_| {
                        error_response(
                            StatusCode::INTERNAL_SERVER_ERROR,
                            "the request handler panicked",
                        )
                    });
                    Ok::<_, Infallible>(response)
                }
            });
            let _ = http1::Builder::new()
                .keep_alive(false)
                .serve_connection(TokioIo::new(stream), service)
                .await;
        });
    }
}

/// Prints what `radian hub pair` prints (ADR 0048 §16): the pairing URL, the token, one
/// `other: <url>` line per other candidate address when the URL is not loopback, and (on a
/// terminal, unless `no_qr`) the QR code of [`payload`] as Unicode half blocks. A pipe or file
/// gets the text lines only.
///
/// # Errors
///
/// `--url` is not an absolute `http` or `https` URL with a host (a usage error), the token
/// cannot be prepared, the QR code cannot be built, or stdout cannot be written.
pub fn pair(bind: IpAddr, port: u16, no_qr: bool, url: Option<&str>) -> Result<(), Error> {
    // Before the token: a refused `--url` changes no file.
    let pairing = pairing(bind, port, url, &interface_addresses())?;
    let registry = Registry::open()?;
    let value = token::ensure(registry.dir())?;
    radian_host::write_line(&pairing.url)
        .map_err(|error| Error::Failed(format!("cannot print the hub URL: {error}")))?;
    radian_host::write_line(&value)
        .map_err(|error| Error::Failed(format!("cannot print the hub token: {error}")))?;
    for other in &pairing.others {
        radian_host::write_line(&format!("other: {other}"))
            .map_err(|error| Error::Failed(format!("cannot print the hub URL: {error}")))?;
    }
    if pairing.fallback {
        eprintln!(
            "hub: no LAN address found; the pairing URL is {}",
            pairing.url
        );
    }
    if no_qr || !std::io::stdout().is_terminal() {
        return Ok(());
    }
    let code = qrcode::QrCode::new(payload(&pairing.url, &value).as_bytes())
        .map_err(|error| Error::Failed(format!("cannot build the pairing QR code: {error}")))?;
    let art = code.render::<qrcode::render::unicode::Dense1x2>().build();
    radian_host::write_stdout(&format!("{art}\n"))
        .map_err(|error| Error::Failed(format!("cannot print the pairing QR code: {error}")))
}

/// The QR payload a phone reads: `radian://hub?url=<url>&token=<token>`, with the URL
/// percent-encoded so a path or a query survives the QR reader (ADR 0048 §16).
#[must_use]
pub fn payload(url: &str, token: &str) -> String {
    format!("radian://hub?url={}&token={token}", percent_encode(url))
}

/// Percent-encodes every byte but an unreserved URL character (RFC 3986 §2.3), so `:` and `/`
/// in the URL reach the phone as data rather than as separators.
fn percent_encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// One IPv4 address an interface of this machine carries, in interface order, with the one flag
/// the LAN chooser reads (ADR 0048 §16).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LanCandidate {
    /// The address the interface carries.
    pub address: Ipv4Addr,
    /// Whether the interface is up (`IFF_UP`).
    pub up: bool,
}

/// What `radian hub pair` prints: the URL the first line and the QR code carry, the other
/// addresses this machine answers on, and whether an unspecified bind found no LAN address and
/// fell back to loopback (ADR 0048 §16).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pairing {
    /// The URL the first stdout line and the QR payload carry.
    pub url: String,
    /// The other candidate addresses, as URLs, one per line after the token.
    pub others: Vec<String>,
    /// True when `[hub] bind` is unspecified and this machine has no LAN address, so `url` is
    /// loopback and one stderr line says so.
    pub fallback: bool,
}

/// The pairing URL for `[hub] bind`, `[hub] port`, an optional `--url` and this machine's
/// candidate addresses (ADR 0048 §16): `--url` when given, else loopback for any loopback bind
/// (`127.0.0.0/8` or `::1`), else the bind's own address, else the first eligible LAN address
/// for an unspecified bind, else loopback with `fallback` set.
///
/// # Errors
///
/// `url` is not an absolute `http` or `https` URL with a host: a usage error that names
/// `--url`.
pub fn pairing(
    bind: IpAddr,
    port: u16,
    url: Option<&str>,
    candidates: &[LanCandidate],
) -> Result<Pairing, Error> {
    let (url, fallback) = match url {
        Some(url) => (pairing_url(url)?, false),
        None => match bind {
            IpAddr::V4(address) if address.is_loopback() => (loopback_url(port), false),
            IpAddr::V6(address) if address.is_loopback() => (loopback_url(port), false),
            IpAddr::V4(address) if address.is_unspecified() => unspecified_url(port, candidates),
            IpAddr::V6(address) if address.is_unspecified() => unspecified_url(port, candidates),
            IpAddr::V4(address) => (format!("http://{address}:{port}"), false),
            IpAddr::V6(address) => (format!("http://[{address}]:{port}"), false),
        },
    };
    let others = if url_host_is_loopback(&url) {
        Vec::new()
    } else {
        let mut others: Vec<String> = Vec::new();
        // Loopback first: it is the address that always answers, whatever the network is.
        let loopback = loopback_url(port);
        if loopback != url {
            others.push(loopback);
        }
        for candidate in candidates.iter().filter(|candidate| eligible(candidate)) {
            let candidate = format!("http://{}:{port}", candidate.address);
            if candidate != url && !others.contains(&candidate) {
                others.push(candidate);
            }
        }
        others
    };
    Ok(Pairing {
        url,
        others,
        fallback,
    })
}

/// `http://127.0.0.1:<port>`: the URL a loopback bind answers on.
fn loopback_url(port: u16) -> String {
    format!("http://127.0.0.1:{port}")
}

/// The URL for an unspecified bind (`0.0.0.0` or `::`): the first eligible LAN address, or
/// loopback with the fallback flag when this machine has none.
fn unspecified_url(port: u16, candidates: &[LanCandidate]) -> (String, bool) {
    match lan_address(candidates) {
        Some(address) => (format!("http://{address}:{port}"), false),
        None => (loopback_url(port), true),
    }
}

/// The first address `pair` may print for an unspecified bind (ADR 0048 §16): the first
/// candidate that is up, not loopback and not link-local, in interface order. `None` when the
/// list has no such address, and for an empty list.
#[must_use]
pub fn lan_address(candidates: &[LanCandidate]) -> Option<Ipv4Addr> {
    candidates
        .iter()
        .find(|candidate| eligible(candidate))
        .map(|candidate| candidate.address)
}

/// Whether a candidate may be the LAN address: up, not loopback, not link-local.
fn eligible(candidate: &LanCandidate) -> bool {
    candidate.up && !candidate.address.is_loopback() && !is_link_local(candidate.address)
}

/// Whether `address` is in `169.254.0.0/16`, the link-local range an interface picks when no
/// DHCP server answered; such an address is often not reachable from the phone's network.
fn is_link_local(address: Ipv4Addr) -> bool {
    let [first, second, ..] = address.octets();
    first == 169 && second == 254
}

/// Every IPv4 address this machine's interfaces carry, in interface order, from `getifaddrs`.
/// An interface that is down is kept with `up` false, so the chooser's rule is one list rather
/// than a filter here.
fn interface_addresses() -> Vec<LanCandidate> {
    let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: `getifaddrs` writes one pointer to a list it allocated, and 0 means it wrote one.
    if unsafe { libc::getifaddrs(&mut list) } != 0 {
        return Vec::new();
    }
    let mut candidates = Vec::new();
    let mut cursor = list;
    while !cursor.is_null() {
        // SAFETY: every node of the list `getifaddrs` returned is valid until `freeifaddrs`,
        // and each `ifa_next` either points at the next node or is null.
        let entry = unsafe { &*cursor };
        if !entry.ifa_addr.is_null() {
            // SAFETY: a non-null `ifa_addr` points at one `sockaddr` whose family decides its
            // shape; only the `AF_INET` shape is read below.
            let family = unsafe { (*entry.ifa_addr).sa_family as i32 };
            if family == libc::AF_INET {
                // SAFETY: `AF_INET` means the `sockaddr` is a `sockaddr_in`.
                let address = unsafe { (*(entry.ifa_addr as *const libc::sockaddr_in)).sin_addr };
                candidates.push(LanCandidate {
                    address: Ipv4Addr::from(u32::from_be(address.s_addr)),
                    up: entry.ifa_flags & libc::IFF_UP as libc::c_uint != 0,
                });
            }
        }
        cursor = entry.ifa_next;
    }
    // SAFETY: `list` is the head `getifaddrs` returned, freed once.
    unsafe { libc::freeifaddrs(list) };
    candidates
}

/// Checks a `--url` value (ADR 0048 §16): an absolute `http` or `https` URL with a host,
/// minus a trailing `/` of its path. A `/` that ends a query or a fragment is part of that
/// value and stays.
///
/// # Errors
///
/// The text is not such a URL: a usage error that names `--url` and exits `2`.
fn pairing_url(url: &str) -> Result<String, Error> {
    if !is_absolute_http_url(url) {
        return Err(Error::Usage(format!(
            "--url must be an absolute http or https URL with a host: {url:?}"
        )));
    }
    // The WHATWG parser has validated the URL, so a `?` or a `#` in the text starts a query or
    // a fragment: a `/` at the end there is a value, not the path's spelling.
    Ok(if url.ends_with('/') && !url.contains(['?', '#']) {
        url[..url.len() - 1].to_owned()
    } else {
        url.to_owned()
    })
}

/// Whether `url` parses as an absolute `http` or `https` URL with a host. The `url` crate
/// decides: a relative reference, another scheme, a missing host and a malformed IPv6 literal
/// are refused. Two leniencies the WHATWG parser keeps are closed here: the authority as the
/// text spells it must be present and must not end with `:` (`http:///hub` and `http://host:`
/// parse otherwise), and every `%` must start a valid escape (`%zz` parses otherwise).
fn is_absolute_http_url(url: &str) -> bool {
    Url::parse(url).is_ok_and(|parsed| {
        matches!(parsed.scheme(), "http" | "https")
            && parsed.host().is_some()
            && written_authority(url).is_some_and(|authority| !authority.ends_with(':'))
            && has_valid_percent_escapes(url)
    })
}

/// Whether every `%` in `url` starts a two-hex-digit escape. A path with `%zz` is not a URL
/// under RFC 3986 even though the WHATWG parser accepts it.
fn has_valid_percent_escapes(url: &str) -> bool {
    let bytes = url.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let escape = bytes.get(index + 1..index + 3);
            if !escape.is_some_and(|escape| escape.iter().all(u8::is_ascii_hexdigit)) {
                return false;
            }
            index += 3;
        } else {
            index += 1;
        }
    }
    true
}

/// The authority of `url` as the text spells it, past the scheme and any userinfo, or `None`
/// when it has none.
fn written_authority(url: &str) -> Option<&str> {
    let (_, rest) = url.split_once("://")?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host_port = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host_port)| host_port);
    (!host_port.is_empty()).then_some(host_port)
}

/// Whether the URL's host is a loopback IP address. A host name is not one: `localhost` is a
/// name, and the owner who wrote it can reach the hub by it.
fn url_host_is_loopback(url: &str) -> bool {
    Url::parse(url)
        .ok()
        .and_then(|parsed| match parsed.host()? {
            Host::Ipv4(address) => Some(IpAddr::V4(address)),
            Host::Ipv6(address) => Some(IpAddr::V6(address)),
            Host::Domain(_) => None,
        })
        .is_some_and(|address| address.is_loopback())
}

/// Writes a new token and prints `rotated`. A running hub re-reads the file on its next
/// request, so the old token stops working then.
///
/// # Errors
///
/// The token file cannot be written.
pub fn rotate() -> Result<(), Error> {
    let registry = Registry::open()?;
    token::rotate(registry.dir())?;
    radian_host::write_line("rotated")
        .map_err(|error| Error::Failed(format!("cannot print the rotation result: {error}")))
}

// ---- startup -----------------------------------------------------------------------------

/// The token the server accepts. [`TokenState::authorized`] is the one `stat` per request; the
/// file is read only when its mtime differs from the recorded one, so a rotation is picked up
/// without a restart.
struct TokenState {
    path: PathBuf,
    value: String,
    mtime: Option<SystemTime>,
}

impl TokenState {
    fn new(dir: &Path, value: String) -> Self {
        let path = token::path(dir);
        let mtime = fs::metadata(&path)
            .and_then(|metadata| metadata.modified())
            .ok();
        Self { path, value, mtime }
    }

    /// Whether `header` is `Bearer <token>` with the token the file holds now.
    fn authorized(&mut self, header: Option<&str>) -> bool {
        self.refresh();
        let Some(header) = header else {
            return false;
        };
        let Some((scheme, value)) = header.split_once(' ') else {
            return false;
        };
        scheme.eq_ignore_ascii_case("bearer") && value.trim() == self.value
    }

    fn refresh(&mut self) {
        let Ok(metadata) = fs::metadata(&self.path) else {
            return;
        };
        let mtime = metadata.modified().ok();
        if mtime == self.mtime {
            return;
        }
        if let Ok(text) = fs::read_to_string(&self.path) {
            let value = text.trim().to_owned();
            if !value.is_empty() {
                self.value = value;
            }
            self.mtime = mtime;
        }
    }
}

// ---- the router --------------------------------------------------------------------------

/// One route the router knows. A name and an id are percent-decoded; a diff path keeps its
/// `/` separators and is decoded after they are joined.
enum Route {
    Version,
    Repos,
    Items(String),
    Item(String, String),
    Report(String, String),
    Verdicts(String, String),
    Diff(String, String),
    DiffFile(String, String, String),
    Conversation(String, String),
    Sessions(String),
    Transcript(String, String),
    SessionEvents(String, String),
    SessionModels(String, String),
    SessionPrompt(String, String),
    SessionCancel(String, String),
    SessionAnswer(String, String),
    SessionConfig(String, String),
    SessionClose(String, String),
    Events,
    Action(String, String, OwnerAction),
    Unknown,
}

/// One owner action of the hub's write route (ADR 0048 decision 16). It is the set the TUI
/// offers, without `start` and `reset-budget`: those are not routes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OwnerAction {
    Confirm,
    Merge,
    Cancel,
    Pause,
    Reverify,
    Reject,
    Auto,
    Priority,
}

impl OwnerAction {
    /// The action `name` names, or `None` for anything else (`start` and `reset-budget`
    /// included, so the router answers `404` for them).
    fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "confirm" => OwnerAction::Confirm,
            "merge" => OwnerAction::Merge,
            "cancel" => OwnerAction::Cancel,
            "pause" => OwnerAction::Pause,
            "reverify" => OwnerAction::Reverify,
            "reject" => OwnerAction::Reject,
            "auto" => OwnerAction::Auto,
            "priority" => OwnerAction::Priority,
            _ => return None,
        })
    }
}

fn route(path: &str) -> Route {
    let path = path.strip_prefix('/').unwrap_or(path);
    let parts: Vec<&str> = path.split('/').collect();
    match parts.as_slice() {
        ["v1", "version"] => Route::Version,
        ["v1", "repos"] => Route::Repos,
        ["v1", "events"] => Route::Events,
        ["v1", "repos", name, "items"] => Route::Items(decode(name)),
        ["v1", "repos", name, "items", id] => Route::Item(decode(name), decode(id)),
        ["v1", "repos", name, "items", id, "report"] => Route::Report(decode(name), decode(id)),
        ["v1", "repos", name, "items", id, "verdicts"] => Route::Verdicts(decode(name), decode(id)),
        ["v1", "repos", name, "items", id, "diff"] => Route::Diff(decode(name), decode(id)),
        // A task's worker conversation, before the generic `/items/<id>/<action>` arm: an
        // action named `conversation` is not a write route.
        ["v1", "repos", name, "items", task, "conversation"] => {
            Route::Conversation(decode(name), decode(task))
        }
        ["v1", "repos", name, "items", id, "diff", rest @ ..] if !rest.is_empty() => {
            Route::DiffFile(decode(name), decode(id), decode(&rest.join("/")))
        }
        ["v1", "repos", name, "items", id, action] => match OwnerAction::parse(&decode(action)) {
            Some(action) => Route::Action(decode(name), decode(id), action),
            None => Route::Unknown,
        },
        ["v1", "repos", name, "sessions"] => Route::Sessions(decode(name)),
        ["v1", "repos", name, "sessions", id, "transcript"] => {
            Route::Transcript(decode(name), decode(id))
        }
        ["v1", "repos", name, "sessions", id, "events"] => {
            Route::SessionEvents(decode(name), decode(id))
        }
        ["v1", "repos", name, "sessions", id, "models"] => {
            Route::SessionModels(decode(name), decode(id))
        }
        ["v1", "repos", name, "sessions", id, "prompt"] => {
            Route::SessionPrompt(decode(name), decode(id))
        }
        ["v1", "repos", name, "sessions", id, "cancel"] => {
            Route::SessionCancel(decode(name), decode(id))
        }
        ["v1", "repos", name, "sessions", id, "answer"] => {
            Route::SessionAnswer(decode(name), decode(id))
        }
        ["v1", "repos", name, "sessions", id, "config"] => {
            Route::SessionConfig(decode(name), decode(id))
        }
        ["v1", "repos", name, "sessions", id, "close"] => {
            Route::SessionClose(decode(name), decode(id))
        }
        _ => Route::Unknown,
    }
}

/// Whether `route` takes `method`. `/v1/repos/<name>/sessions` takes both: a `GET` lists the
/// repository's sessions and a `POST` starts one. Every other session route is `GET` except
/// the five that forward a control verb, which are `POST`, like an owner action.
fn accepts(route: &Route, method: &Method) -> bool {
    match route {
        Route::Sessions(_) => method == Method::GET || method == Method::POST,
        Route::Action(..)
        | Route::SessionPrompt(..)
        | Route::SessionCancel(..)
        | Route::SessionAnswer(..)
        | Route::SessionConfig(..)
        | Route::SessionClose(..) => *method == Method::POST,
        Route::Unknown => false,
        _ => *method == Method::GET,
    }
}

/// The most body bytes a route keeps, or `None` when it reads no body. An owner action's body
/// is `{}` or one short field; a session write route carries a prompt's attachments, so it
/// reads up to [`sessions::SESSION_BODY_LIMIT`], the control port's own 16 MiB line limit with
/// the forwarding envelope's headroom.
fn body_limit(route: &Route, method: &Method) -> Option<usize> {
    if method != Method::POST {
        return None;
    }
    match route {
        Route::Action(..) => Some(REQUEST_BODY_LIMIT),
        Route::Sessions(_)
        | Route::SessionPrompt(..)
        | Route::SessionCancel(..)
        | Route::SessionAnswer(..)
        | Route::SessionConfig(..)
        | Route::SessionClose(..) => Some(sessions::SESSION_BODY_LIMIT),
        _ => None,
    }
}

/// Percent-decodes a path segment or a joined path. Invalid escapes and non-UTF-8 bytes are
/// kept as they are, so a path the router does not understand becomes a 404 and never a panic.
fn decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = |byte: u8| (byte as char).to_digit(16);
            if let (Some(high), Some(low)) = (hex(bytes[index + 1]), hex(bytes[index + 2])) {
                out.push((high * 16 + low) as u8);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn handle(
    method: Method,
    path: &str,
    query: Option<&str>,
    body: &[u8],
    registry: &Registry,
    sessions: &Arc<SessionHub>,
) -> Response<ResponseBody> {
    let route = route(path);
    if matches!(route, Route::Unknown) {
        return error_response(StatusCode::NOT_FOUND, "no such route");
    }
    // The write routes: an owner action, or one of the session verbs the hub forwards.
    match &route {
        Route::Action(name, id, action) => {
            return action_route(registry, name, id, *action, body);
        }
        Route::SessionPrompt(name, id) => {
            return session_prompt(registry, sessions, name, id, body);
        }
        Route::SessionCancel(name, id) => {
            return session_cancel(registry, sessions, name, id);
        }
        Route::SessionAnswer(name, id) => {
            return session_answer(registry, sessions, name, id, body);
        }
        Route::SessionConfig(name, id) => {
            return session_config(registry, sessions, name, id, body);
        }
        Route::SessionClose(name, id) => {
            return session_close(registry, sessions, name, id);
        }
        Route::Sessions(name) if method == Method::POST => {
            return session_create(registry, sessions, name, body);
        }
        _ => {}
    }
    if method != Method::GET {
        return error_response(StatusCode::METHOD_NOT_ALLOWED, "method not allowed");
    }
    match route {
        Route::Version => json_response(
            StatusCode::OK,
            json!({
                "api_version": API_VERSION,
                "schema_version": model::SCHEMA_VERSION,
            }),
        ),
        Route::Repos => repos(registry),
        Route::Items(name) => items(registry, &name),
        Route::Item(name, id) => item(registry, &name, &id),
        Route::Report(name, task) => report(registry, &name, &task),
        Route::Verdicts(name, task) => verdicts(registry, &name, &task),
        Route::Diff(name, task) => diff(registry, &name, &task),
        Route::DiffFile(name, task, path) => diff_file(registry, &name, &task, &path),
        Route::Conversation(name, task) => conversation(registry, &name, &task, query),
        Route::Sessions(name) => session_list(registry, sessions, &name),
        Route::Transcript(name, id) => session_transcript(registry, &name, &id),
        Route::SessionEvents(name, id) => session_events(registry, sessions, &name, &id),
        Route::SessionModels(name, id) => session_models(registry, sessions, &name, &id),
        Route::Events => events(registry),
        Route::Action(..)
        | Route::SessionPrompt(..)
        | Route::SessionCancel(..)
        | Route::SessionAnswer(..)
        | Route::SessionConfig(..)
        | Route::SessionClose(..)
        | Route::Unknown => {
            unreachable!("a write or unknown route returned above")
        }
    }
}

/// Whether `header` carries the token the file holds now. The token's `stat` and any re-read
/// run on a blocking thread, like the route work.
async fn authorized(tokens: &Arc<Mutex<TokenState>>, header: Option<String>) -> bool {
    let tokens = Arc::clone(tokens);
    tokio::task::spawn_blocking(move || {
        tokens
            .lock()
            .map(|mut state| state.authorized(header.as_deref()))
            .unwrap_or(false)
    })
    .await
    .unwrap_or(false)
}

/// The most request-body bytes the hub keeps. Every write route's body is `{}` or one short
/// string field; a larger body is refused with `413` instead of being read into memory. Only a
/// write route's body is read: a read request's body is ignored.
pub const REQUEST_BODY_LIMIT: usize = 64 * 1024;

/// One request body, up to [`REQUEST_BODY_LIMIT`].
enum RequestBody {
    Bytes(Bytes),
    TooLarge,
}

/// The whole request body, up to [`REQUEST_BODY_LIMIT`]. An over-limit body is never kept:
/// a declared `Content-Length` over the limit marks it too large before any buffering, and an
/// undeclared body is dropped the moment it passes the limit. The rest is drained only within
/// [`BODY_DRAIN_LIMIT`] and [`BODY_DRAIN_TIMEOUT`], so a client that is still sending can read
/// the `413` without a huge or endless body keeping the task reading. Authorization and routing
/// have already run, so no unauthenticated body and no read route's body is read at all.
async fn request_body(request: hyper::Request<hyper::body::Incoming>, limit: usize) -> RequestBody {
    let mut too_large = declared_over_limit(request.headers(), limit);
    let mut body = request.into_body();
    let mut bytes = Vec::new();
    let mut drained = 0usize;
    // One deadline for the whole drain, never one per frame: a client that sends a frame just
    // under a per-frame timeout must not be able to extend the drain past [`BODY_DRAIN_TIMEOUT`].
    let drain_deadline = tokio::time::Instant::now() + BODY_DRAIN_TIMEOUT;
    loop {
        let next = poll_fn(|cx| Pin::new(&mut body).poll_frame(cx));
        let frame = if too_large {
            let left = drain_deadline.saturating_duration_since(tokio::time::Instant::now());
            if left.is_zero() {
                break;
            }
            match tokio::time::timeout(left, next).await {
                Ok(frame) => frame,
                Err(_) => break,
            }
        } else {
            next.await
        };
        let Some(Ok(frame)) = frame else {
            break;
        };
        let Ok(data) = frame.into_data() else {
            continue;
        };
        if too_large {
            drained = drained.saturating_add(data.len());
            if drained >= BODY_DRAIN_LIMIT {
                break;
            }
            continue;
        }
        if bytes.len().saturating_add(data.len()) > limit {
            bytes = Vec::new();
            too_large = true;
            drained = data.len();
            continue;
        }
        bytes.extend_from_slice(&data);
    }
    if too_large {
        RequestBody::TooLarge
    } else {
        RequestBody::Bytes(Bytes::from(bytes))
    }
}

/// How much of an over-limit body is drained, and for how long, so a client that is still
/// sending reaches the `413` instead of a reset connection. The timeout is one deadline for
/// the whole drain, not a limit per frame.
const BODY_DRAIN_LIMIT: usize = 1024 * 1024;
const BODY_DRAIN_TIMEOUT: Duration = Duration::from_millis(250);

/// Whether a declared `Content-Length` is over `limit`, before any byte is kept.
fn declared_over_limit(headers: &hyper::HeaderMap, limit: usize) -> bool {
    headers
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .is_some_and(|length| length > limit as u64)
}

// ---- the owner's write routes ------------------------------------------------------------

/// What one owner action did: the CLI's stdout text, and the status/`ok` pair the API answers.
struct ActionResult {
    status: StatusCode,
    text: String,
}

impl ActionResult {
    fn success(text: String) -> Self {
        Self {
            status: StatusCode::OK,
            text,
        }
    }

    fn failed(status: StatusCode, text: String) -> Self {
        Self { status, text }
    }
}

/// One `POST /v1/repos/<name>/items/<id>/<action>` (ADR 0048 decision 16).
///
/// A held `state.lock` is answered before the action runs, so the route changes no file when
/// the lock is refused; the wait is the CLI's own [`crate::store::LOCK_WAIT`]. Otherwise the
/// action is the library call the TUI makes, and its outcome is the API's status and body.
fn action_route(
    registry: &Registry,
    name: &str,
    id: &str,
    action: OwnerAction,
    body: &[u8],
) -> Response<ResponseBody> {
    let store = match registry.store_for(name) {
        Ok(store) => store,
        Err(error) => return action_response(&error),
    };
    if let Err(error) = store.check_state_lock() {
        return action_response(&error);
    }
    match run_action(&store, id, action, body) {
        Ok(result) => json_response(
            result.status,
            json!({ "ok": result.status == StatusCode::OK, "text": result.text }),
        ),
        Err(error) => action_response(&error),
    }
}

/// The `{"ok": false, "text": "<message>"}` body of a failed action, at its mapped status.
fn action_response(error: &Error) -> Response<ResponseBody> {
    json_response(
        action_status(error),
        json!({ "ok": false, "text": error.message() }),
    )
}

/// Runs one owner action through the same library function the TUI calls (ADR 0048
/// decisions 11 and 16). `body` is the request's JSON; a field the action takes is read from
/// it, and a missing one is the empty string, which the library refuses the way the CLI does.
fn run_action(
    store: &Store,
    id: &str,
    action: OwnerAction,
    body: &[u8],
) -> Result<ActionResult, Error> {
    match action {
        OwnerAction::Confirm => {
            let confirmed = plan_ops::confirm(store, id)?;
            Ok(ActionResult::success(confirmed.line()))
        }
        OwnerAction::Merge => {
            let merged = review::merge(store, id)?;
            let mut text = merged.line();
            if let Some(warning) = &merged.warning {
                text.push_str(&format!("\nwarning: {warning}"));
            }
            Ok(if merged.pending {
                // The CLI prints the same line and exits `3`: the task stays
                // `review.integrating` and a later merge continues it.
                ActionResult::failed(StatusCode::CONFLICT, text)
            } else {
                ActionResult::success(text)
            })
        }
        OwnerAction::Cancel => {
            let canceled = attempt::cancel_any(store, id)?;
            Ok(ActionResult::success(canceled.lines().join("\n")))
        }
        OwnerAction::Pause => {
            plan_ops::pause(store, id)?;
            Ok(ActionResult::success(format!("{id} auto off")))
        }
        OwnerAction::Reverify => {
            let reverified = review::reverify(store, id)?;
            let text = reverified.lines().join("\n");
            match &reverified.verification {
                Ok(_) => Ok(ActionResult::success(text)),
                // The CLI prints the claim's lines on stdout and the verification's own error
                // on stderr with its exit code; the body carries both, line for line.
                Err(error) => Ok(ActionResult::failed(
                    action_status(error),
                    format!("{text}\n{}", error.message()),
                )),
            }
        }
        OwnerAction::Reject => {
            let note = body_string(body, "note");
            let rejected = review::reject(store, id, &note)?;
            Ok(ActionResult::success(rejected.line()))
        }
        OwnerAction::Auto => {
            let level = body_string(body, "level");
            plan_ops::set_auto(store, id, &level)?;
            Ok(ActionResult::success(format!("{id} auto {level}")))
        }
        OwnerAction::Priority => {
            let priority = body_string(body, "priority");
            // `set_priority` refuses a value outside the CLI's set with exit `3`. That is a
            // validation error, so the route answers `422`; `check_priority` is the same
            // check and message, so the two can never drift.
            if let Err(error) = plan_ops::check_priority(&priority) {
                return Ok(ActionResult::failed(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    error.message(),
                ));
            }
            plan_ops::set_priority(store, id, &priority)?;
            Ok(ActionResult::success(format!("{id} priority {priority}")))
        }
    }
}

/// The string field `field` of the request's JSON object, or the empty string. A body that is
/// not JSON, or a field that is not a string, reads as empty and is refused by the library.
fn body_string(body: &[u8], field: &str) -> String {
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|value| value.get(field).and_then(Value::as_str).map(str::to_owned))
        .unwrap_or_default()
}

/// The status code the API answers for one CLI outcome (ADR 0048 decision 16), one row per
/// outcome: success `200`, an empty reject note `400`, a usage or validation error `422`, a
/// held lock or a refused state change `409`, an unknown item `404`, and anything else `500`.
fn action_status(error: &Error) -> StatusCode {
    if unknown_target(error) {
        return StatusCode::NOT_FOUND;
    }
    match error {
        Error::Usage(message) if message == review::EMPTY_REJECT_NOTE => StatusCode::BAD_REQUEST,
        Error::Usage(_) => StatusCode::UNPROCESSABLE_ENTITY,
        Error::Locked(_) => StatusCode::CONFLICT,
        Error::NotInitialized | Error::ChangedOutsideArc(_) | Error::Refused(_) => {
            StatusCode::CONFLICT
        }
        Error::Failed(_) => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

/// Whether the CLI's message names no registered repository or no stored item: `no repository
/// named`, `no plan or task`, `no plan` or `no task`.
fn unknown_target(error: &Error) -> bool {
    matches!(error, Error::Failed(message)
        if message.starts_with("no repository named `")
            || message.starts_with("no plan or task `")
            || message.starts_with("no plan `")
            || message.starts_with("no task `"))
}

// ---- the read routes ---------------------------------------------------------------------

fn repos(registry: &Registry) -> Response<ResponseBody> {
    match registry.repos() {
        Ok(entries) => {
            let list: Vec<Value> = entries
                .iter()
                .map(|entry| {
                    let store = Store::at(entry.path.clone());
                    let (scheduled, holder) = repo_view(&store);
                    json!({
                        "name": entry.name,
                        "path": entry.path.display().to_string(),
                        "scheduled": scheduled,
                        "lock_holder": holder.map(|pid| pid.to_string()),
                    })
                })
                .collect();
            json_response(StatusCode::OK, Value::Array(list))
        }
        Err(error) => failed(error),
    }
}

/// Whether this hub schedules `store`'s repository, and the pid its lock names. The lock is
/// probed, never inferred from the pid text: that text outlives the lock, so a loop that
/// stopped (repeated error, SIGTERM) leaves the hub's pid in the file while it no longer holds
/// or schedules that repository.
fn repo_view(store: &Store) -> (bool, Option<u32>) {
    if store.runner_lock_free() {
        return (false, None);
    }
    let holder = store.runner_lock_pid();
    (holder == Some(std::process::id()), holder)
}

fn items(registry: &Registry, name: &str) -> Response<ResponseBody> {
    let store = match registry.store_for(name) {
        Ok(store) => store,
        Err(error) => return not_found(error),
    };
    match cli::list_json_text(&store, None, false) {
        Ok(text) => bytes_response(StatusCode::OK, text.into_bytes()),
        Err(error) => failed(error),
    }
}

fn item(registry: &Registry, name: &str, id: &str) -> Response<ResponseBody> {
    let store = match registry.store_for(name) {
        Ok(store) => store,
        Err(error) => return not_found(error),
    };
    match cli::show_json_text(&store, id) {
        Ok(text) => bytes_response(StatusCode::OK, text.into_bytes()),
        Err(error) => not_found(error),
    }
}

fn report(registry: &Registry, name: &str, task: &str) -> Response<ResponseBody> {
    let store = match registry.store_for(name) {
        Ok(store) => store,
        Err(error) => return not_found(error),
    };
    // The id must name a stored task before the run directories are looked at: an unknown id
    // (or a plan's) is the CLI's 404, not a task with no attempt.
    if let Err(error) = store.load_task(task) {
        return not_found(error);
    }
    let attempts = match read::task_attempts(&store, task) {
        Ok(attempts) => attempts,
        Err(error) => return failed(error),
    };
    let Some(newest) = attempts.last() else {
        return no_content();
    };
    // The ADR 0048 decision 7 hand-off reads the same file: the last `Result` line of the
    // newest attempt's `result.jsonl`.
    let result = attempt_ops::run_dir(&store, task, newest.attempt).join("result.jsonl");
    match last_report(&result) {
        Some(text) => json_response(
            StatusCode::OK,
            json!({
                "attempt": format!("{task}-{}", newest.attempt),
                "text": text,
            }),
        ),
        None => no_content(),
    }
}

/// The report line one attempt's `result.jsonl` ends with: the last `Result` record's `text`,
/// trimmed. `None` for no record, no `text`, or an empty one.
fn last_report(path: &Path) -> Option<String> {
    let text = fs::read_to_string(path).ok()?;
    for line in text.lines().rev() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if value.get("type").and_then(Value::as_str) != Some("Result") {
            continue;
        }
        let text = value.get("text")?.as_str()?.trim();
        return (!text.is_empty()).then(|| text.to_owned());
    }
    None
}

fn verdicts(registry: &Registry, name: &str, task: &str) -> Response<ResponseBody> {
    let store = match registry.store_for(name) {
        Ok(store) => store,
        Err(error) => return not_found(error),
    };
    // The id must name a stored task, so an unknown id (or a plan's) is the CLI's 404 rather
    // than an empty list.
    if let Err(error) = store.load_task(task) {
        return not_found(error);
    }
    let attempts = match read::task_attempts(&store, task) {
        Ok(attempts) => attempts,
        Err(error) => return failed(error),
    };
    let Some(newest) = attempts.last() else {
        return json_response(StatusCode::OK, json!([]));
    };
    let dir = attempt_ops::run_dir(&store, task, newest.attempt).join("verdicts");
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return json_response(StatusCode::OK, json!([]));
        }
        Err(error) => {
            return failed(Error::Failed(format!(
                "cannot read {}: {error}",
                dir.display()
            )));
        }
    };
    let mut list = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let verdict = match Verdict::read(&path) {
            Ok(verdict) => verdict,
            Err(error) => {
                return failed(Error::Failed(format!("cannot read a verdict: {error}")));
            }
        };
        list.push(json!({
            "stage": verdict.stage.as_str(),
            "result": if verdict.is_pass() { "pass" } else { "fail" },
            "reason": verdict.reason.clone(),
            "path": path.display().to_string(),
            "verdict": serde_json::to_value(&verdict).unwrap_or(Value::Null),
        }));
    }
    list.sort_by_key(|entry| stage_order(entry.get("stage").and_then(Value::as_str)));
    json_response(StatusCode::OK, Value::Array(list))
}

/// The order a verdict list prints in, the same order `radian arc show` reads its stages.
fn stage_order(stage: Option<&str>) -> usize {
    const ORDER: [Stage; 6] = [
        Stage::Commands,
        Stage::Mutation,
        Stage::Review,
        Stage::Ci,
        Stage::Human,
        Stage::Integration,
    ];
    stage
        .and_then(|name| ORDER.iter().position(|stage| stage.as_str() == name))
        .unwrap_or(ORDER.len())
}

fn diff(registry: &Registry, name: &str, task: &str) -> Response<ResponseBody> {
    let store = match registry.store_for(name) {
        Ok(store) => store,
        Err(error) => return not_found(error),
    };
    // Only `--numstat` is read: the list never holds a file's diff text (a huge changed file
    // costs one line), and the per-file route below reads at most `DIFF_TEXT_LIMIT` bytes.
    match read::task_diff_counts(&store, task) {
        Ok(files) => json_response(
            StatusCode::OK,
            Value::Array(
                files
                    .iter()
                    .map(|file| {
                        json!({
                            "path": file.path,
                            "added": file.added,
                            "removed": file.removed,
                        })
                    })
                    .collect(),
            ),
        ),
        Err(error) => not_found(error),
    }
}

fn diff_file(registry: &Registry, name: &str, task: &str, path: &str) -> Response<ResponseBody> {
    let store = match registry.store_for(name) {
        Ok(store) => store,
        Err(error) => return not_found(error),
    };
    // The requested file only: git's output is read up to the limit and the process stopped
    // there, so a multi-GB change never reaches memory whole.
    let diff = match read::task_diff_file(&store, task, path, DIFF_TEXT_LIMIT) {
        Ok(Some(diff)) => diff,
        Ok(None) => {
            return error_response(
                StatusCode::NOT_FOUND,
                &format!("no file `{path}` in the task's diff"),
            );
        }
        Err(error) => return not_found(error),
    };
    let (text, cut) = cap(&diff.text, DIFF_TEXT_LIMIT);
    json_response(
        StatusCode::OK,
        json!({
            "path": path,
            "truncated": diff.truncated || cut,
            "text": text,
        }),
    )
}

/// `text` cut at `limit` bytes, never inside a character, and whether it was cut.
fn cap(text: &str, limit: usize) -> (&str, bool) {
    if text.len() <= limit {
        return (text, false);
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (&text[..end], true)
}

// ---- the session routes ------------------------------------------------------------------

/// The repository `name` names, the way `Registry::store_for` resolves it (so the refusal a
/// session route answers with is the CLI's own message).
fn repo_path(registry: &Registry, name: &str) -> Result<PathBuf, Error> {
    registry
        .repos()?
        .into_iter()
        .find(|entry| entry.name == name)
        .map(|entry| entry.path)
        .ok_or_else(|| Error::Failed(format!("no repository named `{name}` is registered")))
}

/// The status and body one [`SessionError`] answers with.
fn session_failure(error: SessionError) -> Response<ResponseBody> {
    let status = match error {
        SessionError::Unknown(_) => StatusCode::NOT_FOUND,
        SessionError::NotLive(_) | SessionError::Late(_) => StatusCode::CONFLICT,
        SessionError::Bad(_) => StatusCode::BAD_REQUEST,
        SessionError::Invalid(_) => StatusCode::UNPROCESSABLE_ENTITY,
        SessionError::Port(_) => StatusCode::BAD_GATEWAY,
        SessionError::Failed(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    error_response(status, error.message())
}

/// `GET /v1/repos/<name>/sessions`: every session of the repository's workspace, live and
/// ended (ADR 0048 decision 16).
fn session_list(registry: &Registry, sessions: &SessionHub, name: &str) -> Response<ResponseBody> {
    let path = match repo_path(registry, name) {
        Ok(path) => path,
        Err(error) => return not_found(error),
    };
    match sessions::sessions_body(sessions, &path) {
        Ok(body) => json_response(StatusCode::OK, body),
        Err(error) => session_failure(error),
    }
}

/// `POST /v1/repos/<name>/sessions`: start a hub-held process for a new session, send it the
/// body's first message, and answer `{"session": "<id>"}` (ADR 0048 §16). The body is
/// validated before anything starts.
fn session_create(
    registry: &Registry,
    sessions: &SessionHub,
    name: &str,
    body: &[u8],
) -> Response<ResponseBody> {
    let path = match repo_path(registry, name) {
        Ok(path) => path,
        Err(error) => return not_found(error),
    };
    match sessions::start_session(sessions, &path, body) {
        Ok(id) => json_response(StatusCode::OK, json!({ "session": id })),
        Err(error) => session_failure(error),
    }
}

/// `POST …/sessions/<id>/prompt`: deliver one message, resuming an ended session first.
fn session_prompt(
    registry: &Registry,
    sessions: &SessionHub,
    name: &str,
    id: &str,
    body: &[u8],
) -> Response<ResponseBody> {
    let path = match repo_path(registry, name) {
        Ok(path) => path,
        Err(error) => return not_found(error),
    };
    match sessions::prompt(sessions, &path, id, body) {
        Ok(answer) => json_response(
            StatusCode::OK,
            json!({
                "session": id,
                "message_id": answer.get("message_id").cloned().unwrap_or(Value::Null),
            }),
        ),
        Err(error) => session_failure(error),
    }
}

/// `POST …/sessions/<id>/cancel`: the control port's `cancel`, with the hub token.
fn session_cancel(
    registry: &Registry,
    sessions: &SessionHub,
    name: &str,
    id: &str,
) -> Response<ResponseBody> {
    let path = match repo_path(registry, name) {
        Ok(path) => path,
        Err(error) => return not_found(error),
    };
    match sessions::cancel(sessions, &path, id) {
        Ok(()) => json_response(StatusCode::OK, json!({ "ok": true })),
        Err(error) => session_failure(error),
    }
}

/// `POST …/sessions/<id>/answer`: the control port's `answer`. A late answer is `409` and
/// changes nothing; an unknown request id is `404`.
fn session_answer(
    registry: &Registry,
    sessions: &SessionHub,
    name: &str,
    id: &str,
    body: &[u8],
) -> Response<ResponseBody> {
    let path = match repo_path(registry, name) {
        Ok(path) => path,
        Err(error) => return not_found(error),
    };
    match sessions::answer(sessions, &path, id, body) {
        Ok(()) => json_response(StatusCode::OK, json!({ "ok": true })),
        Err(error) => session_failure(error),
    }
}

/// `POST …/sessions/<id>/config`: the control port's `config`, applied at the next turn
/// boundary (ADR 0004).
fn session_config(
    registry: &Registry,
    sessions: &SessionHub,
    name: &str,
    id: &str,
    body: &[u8],
) -> Response<ResponseBody> {
    let path = match repo_path(registry, name) {
        Ok(path) => path,
        Err(error) => return not_found(error),
    };
    match sessions::config(sessions, &path, id, body) {
        Ok(()) => json_response(StatusCode::OK, json!({ "ok": true })),
        Err(error) => session_failure(error),
    }
}

/// `POST …/sessions/<id>/close`: the control port's `close` (ADR 0043), which ends the process
/// after answering. A hub-held process leaves its journal for the next prompt; a TUI-hosted one
/// refuses with its own text, carried as `502`.
fn session_close(
    registry: &Registry,
    sessions: &SessionHub,
    name: &str,
    id: &str,
) -> Response<ResponseBody> {
    let path = match repo_path(registry, name) {
        Ok(path) => path,
        Err(error) => return not_found(error),
    };
    match sessions::close(sessions, &path, id) {
        Ok(()) => json_response(StatusCode::OK, json!({ "ok": true })),
        Err(error) => session_failure(error),
    }
}

/// `GET /v1/repos/<name>/sessions/<id>/transcript`: the journal folded into the shared entry,
/// the same entries `radian attach` shows.
fn session_transcript(registry: &Registry, name: &str, id: &str) -> Response<ResponseBody> {
    let path = match repo_path(registry, name) {
        Ok(path) => path,
        Err(error) => return not_found(error),
    };
    match sessions::transcript_body(&path, id) {
        Ok(body) => json_response(StatusCode::OK, body),
        Err(error) => session_failure(error),
    }
}

/// `GET /v1/repos/<name>/sessions/<id>/models`: the control port's own `models` answer, so the
/// phone offers the list `/model` offers in that session.
fn session_models(
    registry: &Registry,
    sessions: &SessionHub,
    name: &str,
    id: &str,
) -> Response<ResponseBody> {
    let path = match repo_path(registry, name) {
        Ok(path) => path,
        Err(error) => return not_found(error),
    };
    // The session must be this repository's before its port is asked: another workspace's
    // session is not reachable through this repository's name.
    if let Err(error) = sessions::journal_for(&path, id) {
        return session_failure(error);
    }
    match sessions::models(sessions, id) {
        Ok(body) => json_response(StatusCode::OK, body),
        Err(error) => session_failure(error),
    }
}

/// `GET /v1/repos/<name>/sessions/<id>/events`: the session's event stream, fed by the hub's
/// own long-lived subscription. It opens with one `pending` frame holding the queue's current
/// state, then follows live changes. Like `/v1/events` it never replays entries: a client that
/// reconnects first re-reads the transcript.
fn session_events(
    registry: &Registry,
    sessions: &Arc<SessionHub>,
    name: &str,
    id: &str,
) -> Response<ResponseBody> {
    let path = match repo_path(registry, name) {
        Ok(path) => path,
        Err(error) => return not_found(error),
    };
    if let Err(error) = sessions::journal_for(&path, id) {
        return session_failure(error);
    }
    let Some(source) = sessions.channel(id) else {
        return session_failure(SessionError::NotLive(format!(
            "session {id:?} is not live: no process hosts it"
        )));
    };
    // Subscribe before the snapshot read, so a live frame raised during the read is buffered in
    // this receiver and can be flushed after the snapshot rather than lost or sent first.
    let mut events = source.subscribe();
    let (sender, receiver) = mpsc::channel(EVENT_QUEUE);
    // The stream's producer is an async task: it moves frames from the subscription's fan-out
    // to the connection and writes a keep-alive while nothing happens. It does no blocking IO,
    // so the runtime's threads are free for other requests.
    let hub = Arc::clone(sessions);
    let session = id.to_owned();
    tokio::spawn(async move {
        // The first frame is the queue's current state, so a client that subscribes while an
        // approval or question waits learns the head without a transcript re-read. The control
        // port's `pending` verb is blocking socket work, so it runs on a blocking task.
        let mut read = tokio::task::spawn_blocking(move || sessions::pending(&hub, &session));
        // The subscription is drained into this buffer while the read is in flight. Left
        // unread, the fan-out's 64-frame ring would drop the oldest changes of a busy session
        // before the snapshot could be sent, and the client would never see them.
        let (drained, mut buffered) = drain_snapshot(&mut events, &mut read).await;
        let snapshot = match drained {
            SnapshotDrain::Read(Ok(Ok(snapshot))) => snapshot,
            // The port went away or refused after the stream opened: end it exactly as a
            // session whose process goes away ends it.
            SnapshotDrain::Read(Ok(Err(_)) | Err(_)) => return,
            // The session was forgotten while the read was in flight: the stream ends too.
            SnapshotDrain::Closed => return,
            // The drain fell behind the fan-out's ring, so it never copied some live frames
            // of the read's window and they are unrecoverable. Ending the stream is how the
            // client is told: it reconnects and re-reads the transcript, the stream's rule,
            // rather than following a stream with a silent hole in it.
            SnapshotDrain::Lagged => return,
        };
        if sender
            .send(Ok(Frame::data(sessions::pending_frame(&snapshot))))
            .await
            .is_err()
        {
            return;
        }
        let mut ticker = tokio::time::interval(event_keep_alive_interval());
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // `interval` fires once at once; a keep-alive at the start is not what the stream
        // means, so the first tick is dropped.
        ticker.tick().await;
        loop {
            // The frames raised while the snapshot was read follow it, in the order the
            // session emitted them, before anything newer.
            if let Some(frame) = buffered.pop_front() {
                if sender.send(Ok(Frame::data(frame))).await.is_err() {
                    return;
                }
                continue;
            }
            tokio::select! {
                event = events.recv() => match event {
                    Ok(frame) => {
                        if sender.send(Ok(Frame::data(frame))).await.is_err() {
                            return;
                        }
                    }
                    // The fan-out's ring overflowed: this task fell more than 64 frames
                    // behind (a slow client blocks the send below) and the missed frames are
                    // unrecoverable. Ending the stream is how the client is told, so it
                    // reconnects and re-reads the transcript instead of following a stream
                    // with a silent hole in it.
                    Err(broadcast::error::RecvError::Lagged(_)) => return,
                    // The session was forgotten: the stream ends.
                    Err(broadcast::error::RecvError::Closed) => return,
                },
                _ = ticker.tick() => {
                    if sender
                        .send(Ok(Frame::data(Bytes::from_static(b": keep-alive\n\n"))))
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
            }
        }
    });
    Response::builder()
        .status(StatusCode::OK)
        .header(CONTENT_TYPE, "text/event-stream")
        .header(CACHE_CONTROL, "no-cache")
        .body(ResponseBody::Stream(receiver))
        .unwrap_or_else(|_| error_response(StatusCode::INTERNAL_SERVER_ERROR, "no body"))
}

/// What the drain found while the session event stream's snapshot read was in flight.
enum SnapshotDrain<T> {
    /// The blocking read answered.
    Read(T),
    /// The session was forgotten while the read was in flight.
    Closed,
    /// The drain fell more than the fan-out's ring behind, so live frames of the read's window
    /// were dropped before it copied them. They cannot be recovered.
    Lagged,
}

/// Drains the session's live fan-out into a buffer until the snapshot `read` answers, the
/// session closes, or the drain falls behind the fan-out's ring.
///
/// The drain outruns the fan-out only while this task is polled. The subscription thread runs
/// on its own and can emit more than the ring's capacity between a `recv` that returns
/// `Pending` and this task's next poll, so a `Lagged` answer is reported, never skipped: the
/// caller ends the stream, and the client re-reads the transcript instead of following a stream
/// with a silent hole in it. A lag that happened before the read answered is reported too: the
/// receiver's state is checked before the read's answer is accepted, because the two can become
/// ready in the same poll and no branch order may let the snapshot hide the lost frames.
async fn drain_snapshot<T, F>(
    events: &mut broadcast::Receiver<Bytes>,
    read: &mut F,
) -> (SnapshotDrain<T>, VecDeque<Bytes>)
where
    F: std::future::Future<Output = T> + Unpin,
{
    let mut buffered = VecDeque::new();
    loop {
        tokio::select! {
            // The read is polled first, so a read that answers in the same turn in which the
            // fan-out has a `Lagged` answer ready cannot be hidden behind the order in which
            // `select!` polls ready branches. The receiver is still checked below before the
            // answer is accepted: `select!` alone cannot decide whether lost frames are
            // reported, because an unbiased `select!` picks either ready branch.
            biased;
            result = &mut *read => {
                // `Lagged` is answered before the frames the ring still holds, so one probe
                // of the receiver tells whether frames of the read's window were already
                // dropped. The probe is not a wait: the read has answered, so every frame
                // it copied or that the ring still holds follows the snapshot as an
                // ordinary live frame.
                match events.try_recv() {
                    Ok(frame) => buffered.push_back(frame),
                    Err(broadcast::error::TryRecvError::Lagged(_)) => {
                        return (SnapshotDrain::Lagged, buffered);
                    }
                    Err(broadcast::error::TryRecvError::Closed) => {
                        return (SnapshotDrain::Closed, buffered);
                    }
                    Err(broadcast::error::TryRecvError::Empty) => {}
                }
                return (SnapshotDrain::Read(result), buffered);
            }
            event = events.recv() => match event {
                Ok(frame) => buffered.push_back(frame),
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    return (SnapshotDrain::Lagged, buffered);
                }
                Err(broadcast::error::RecvError::Closed) => {
                    return (SnapshotDrain::Closed, buffered);
                }
            },
        }
    }
}

/// `GET /v1/repos/<name>/items/<task>/conversation`: one attempt's `result.jsonl` folded with
/// the ACP fold, the entries the arc TUI's conversation page shows. `?attempt=<n>` picks an
/// attempt by number; without it the newest is read.
fn conversation(
    registry: &Registry,
    name: &str,
    task: &str,
    query: Option<&str>,
) -> Response<ResponseBody> {
    let store = match registry.store_for(name) {
        Ok(store) => store,
        Err(error) => return not_found(error),
    };
    if let Err(error) = store.load_task(task) {
        return not_found(error);
    }
    let attempt = match attempt_parameter(query) {
        Ok(attempt) => attempt,
        Err(message) => return error_response(StatusCode::BAD_REQUEST, &message),
    };
    match sessions::conversation_body(&store, task, attempt) {
        Ok(body) => json_response(StatusCode::OK, body),
        Err(error) => session_failure(error),
    }
}

/// The `attempt` query parameter: `None` when the query does not carry one, the number it names
/// otherwise. A value that is not a number is refused, never guessed.
fn attempt_parameter(query: Option<&str>) -> Result<Option<u32>, String> {
    let Some(query) = query else {
        return Ok(None);
    };
    for pair in query.split('&') {
        let Some((key, value)) = pair.split_once('=') else {
            continue;
        };
        if key != "attempt" {
            continue;
        }
        return value
            .parse::<u32>()
            .map(Some)
            .map_err(|_| format!("`attempt` must be a number, not {value:?}"));
    }
    Ok(None)
}

// ---- the event stream --------------------------------------------------------------------

/// `GET /v1/events`: one `: open` comment line before the first poll, then one
/// `event: transition` per state change since the previous read, and a `: keep-alive` comment
/// while nothing changes. The open comment makes the first body byte arrive at once, so a
/// client that waits for it (`URLSession.bytes(for:)` does) does not wait for the first
/// keep-alive; every SSE client ignores a comment. The stream is not durable: it starts with
/// the current lists as its baseline, so a client that reconnects re-reads them instead of
/// asking for a replay.
///
/// The read happens on a thread of its own, never on the runtime's async threads, and stops
/// when the response body is dropped (the client went away) or the process ends. The
/// registry is re-read every interval, so a repository registered while the stream is open
/// is picked up with a baseline of its own.
fn events(registry: &Registry) -> Response<ResponseBody> {
    let (sender, receiver) = mpsc::channel(EVENT_QUEUE);
    let registry = registry.clone();
    let _ = std::thread::Builder::new()
        .name("hub-events".to_owned())
        .spawn(move || stream_events(registry, sender));
    Response::builder()
        .status(StatusCode::OK)
        .header(CONTENT_TYPE, "text/event-stream")
        .header(CACHE_CONTROL, "no-cache")
        .body(ResponseBody::Stream(receiver))
        .unwrap_or_else(|_| error_response(StatusCode::INTERNAL_SERVER_ERROR, "no body"))
}

/// One transition frame: `event: transition` and its one-line JSON object.
fn event_frame(repo: &str, transition: &Transition) -> Bytes {
    let data = match transition {
        Transition::Task { id, task, previous } => json!({
            "repo": repo,
            "kind": "task",
            "id": id,
            "field": "state",
            "previous": previous,
            "value": task.full,
        }),
        Transition::Plan { id, event } => json!({
            "repo": repo,
            "kind": "plan",
            "id": id,
            "field": event.field,
            "previous": event.previous,
            "value": event.value,
        }),
    };
    Bytes::from(format!("event: transition\ndata: {data}\n\n"))
}

/// The event stream's producer: write the `: open` comment at once, then poll every
/// [`event_poll_interval`], write every transition, and write a keep-alive comment after
/// [`event_keep_alive_interval`] with nothing sent. A closed receiver is the client going
/// away, so the thread ends there.
fn stream_events(registry: Registry, sender: mpsc::Sender<Result<Frame<Bytes>, Infallible>>) {
    // The first body bytes are one comment line, sent before the first poll. A client that
    // treats the first byte as the stream's open sees it at once instead of at the first
    // keep-alive, and every SSE client ignores a comment. A client that went away between the
    // headers and this line closes the receiver, so the send fails and the thread ends.
    if sender
        .blocking_send(Ok(Frame::data(Bytes::from_static(b": open\n\n"))))
        .is_err()
    {
        return;
    }
    let poll = event_poll_interval();
    let keep_alive = event_keep_alive_interval();
    let mut poller = Poller::new();
    let mut next_poll = Instant::now();
    let mut last_sent = Instant::now();
    loop {
        if sender.is_closed() {
            return;
        }
        let now = Instant::now();
        if now >= next_poll {
            next_poll = now + poll;
            for (repo, transition) in read_transitions(&registry, &mut poller) {
                if sender
                    .blocking_send(Ok(Frame::data(event_frame(&repo, &transition))))
                    .is_err()
                {
                    return;
                }
                last_sent = Instant::now();
            }
        }
        if last_sent.elapsed() >= keep_alive {
            if sender
                .blocking_send(Ok(Frame::data(Bytes::from_static(b": keep-alive\n\n"))))
                .is_err()
            {
                return;
            }
            last_sent = Instant::now();
        }
        let now = Instant::now();
        let wake = next_poll.min(last_sent + keep_alive);
        if wake > now {
            std::thread::sleep(wake - now);
        } else {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

/// One round of the event stream's read: every store the registry names right now. A registry
/// that cannot be read or a store that cannot be opened is left out this round; the next
/// round tries again.
fn read_transitions(registry: &Registry, poller: &mut Poller) -> Vec<(String, Transition)> {
    let Ok(entries) = registry.repos() else {
        return Vec::new();
    };
    let repos: Vec<Repo> = entries
        .into_iter()
        .map(|entry| Repo {
            name: entry.name,
            store: Store::at(entry.path),
        })
        .collect();
    poller.poll(&repos)
}

/// The event stream's read interval. Test-only: `RADIAN_HUB_EVENTS_POLL_SECS` shortens the
/// channel's five-second cadence so a test does not wait a whole interval per transition; no
/// user sets it.
fn event_poll_interval() -> Duration {
    test_interval(
        std::env::var("RADIAN_HUB_EVENTS_POLL_SECS").ok(),
        CHANNEL_INTERVAL_SECS,
    )
}

/// The keep-alive gap. Test-only: `RADIAN_HUB_EVENTS_KEEP_ALIVE_SECS` shortens the fifteen
/// seconds so a test can see a keep-alive without waiting; no user sets it.
fn event_keep_alive_interval() -> Duration {
    test_interval(
        std::env::var("RADIAN_HUB_EVENTS_KEEP_ALIVE_SECS").ok(),
        KEEP_ALIVE_SECS,
    )
}

/// `value` as seconds, when it holds a positive integer; `default` otherwise. The two readers
/// above are the only callers, and both variables exist only for tests.
fn test_interval(value: Option<String>, default: u64) -> Duration {
    let secs = value
        .and_then(|text| text.parse::<u64>().ok())
        .filter(|secs| *secs > 0)
        .unwrap_or(default);
    Duration::from_secs(secs)
}

// ---- responses ---------------------------------------------------------------------------

/// How many event frames may wait for the connection to take them. A client that stops reading
/// blocks this producer on a full queue; a client that goes away closes the receiver.
const EVENT_QUEUE: usize = 64;

/// A response body: one byte string (a JSON route, or nothing for a `204`), or the frames of
/// an event stream.
enum ResponseBody {
    Once(Option<Bytes>),
    Stream(mpsc::Receiver<Result<Frame<Bytes>, Infallible>>),
}

impl Body for ResponseBody {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        match self.get_mut() {
            ResponseBody::Once(bytes) => {
                Poll::Ready(bytes.take().map(|bytes| Ok(Frame::data(bytes))))
            }
            ResponseBody::Stream(receiver) => receiver.poll_recv(cx),
        }
    }

    fn is_end_stream(&self) -> bool {
        match self {
            ResponseBody::Once(bytes) => bytes.is_none(),
            ResponseBody::Stream(_) => false,
        }
    }

    fn size_hint(&self) -> SizeHint {
        let mut hint = SizeHint::new();
        if let ResponseBody::Once(Some(bytes)) = self {
            hint.set_exact(bytes.len() as u64);
        }
        hint
    }
}

fn response(status: StatusCode, body: Option<Vec<u8>>) -> Response<ResponseBody> {
    let mut builder = Response::builder()
        .status(status)
        .header(CONNECTION, "close");
    match body {
        Some(bytes) => {
            builder = builder
                .header(CONTENT_TYPE, "application/json")
                .header(CONTENT_LENGTH, bytes.len().to_string());
            builder
                .body(ResponseBody::Once(Some(Bytes::from(bytes))))
                .unwrap_or_else(|_| Response::new(ResponseBody::Once(None)))
        }
        None => builder
            .body(ResponseBody::Once(None))
            .unwrap_or_else(|_| Response::new(ResponseBody::Once(None))),
    }
}

fn bytes_response(status: StatusCode, bytes: Vec<u8>) -> Response<ResponseBody> {
    response(status, Some(bytes))
}

fn json_response(status: StatusCode, value: Value) -> Response<ResponseBody> {
    response(status, Some(serde_json::to_vec(&value).unwrap_or_default()))
}

fn unauthorized_response() -> Response<ResponseBody> {
    let mut response = json_response(StatusCode::UNAUTHORIZED, json!({}));
    response
        .headers_mut()
        .insert(WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
    response
}

fn error_response(status: StatusCode, message: &str) -> Response<ResponseBody> {
    json_response(status, json!({ "error": message }))
}

fn not_found(error: Error) -> Response<ResponseBody> {
    error_response(StatusCode::NOT_FOUND, &error.message())
}

fn failed(error: Error) -> Response<ResponseBody> {
    error_response(StatusCode::INTERNAL_SERVER_ERROR, &error.message())
}

fn no_content() -> Response<ResponseBody> {
    response(StatusCode::NO_CONTENT, None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    /// The session event stream's producer is spawned from a route, and a route runs on a
    /// blocking thread. `tokio::spawn` must be usable there, or the stream would never produce a
    /// frame.
    #[test]
    fn a_route_can_spawn_on_the_runtime_from_a_blocking_task() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("a runtime");
        let spawned = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = std::sync::Arc::clone(&spawned);
        runtime.block_on(async move {
            tokio::task::spawn_blocking(move || {
                tokio::spawn(async move {
                    flag.store(true, std::sync::atomic::Ordering::SeqCst);
                });
            })
            .await
            .expect("the blocking route finished");
            tokio::time::sleep(Duration::from_millis(50)).await;
        });
        assert!(
            spawned.load(std::sync::atomic::Ordering::SeqCst),
            "the spawned stream task ran"
        );
    }

    /// The drain copies every live frame the ring still holds, in emission order, before the
    /// snapshot read's answer.
    #[test]
    fn a_drain_within_the_ring_buffers_every_frame_in_order() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("a runtime");
        runtime.block_on(async {
            let (sender, mut events) = broadcast::channel(64);
            for n in 0..5 {
                sender
                    .send(Bytes::from(format!("frame {n}")))
                    .expect("the drain's subscription is open");
            }
            // The read answers only after the frames above had a chance to be drained.
            let mut read = Box::pin(async {
                tokio::time::sleep(Duration::from_millis(20)).await;
                "snapshot"
            });
            let (drained, buffered) = drain_snapshot(&mut events, &mut read).await;
            assert!(matches!(drained, SnapshotDrain::Read("snapshot")));
            let frames: Vec<String> = buffered
                .into_iter()
                .map(|frame| String::from_utf8_lossy(&frame).into_owned())
                .collect();
            assert_eq!(
                frames,
                ["frame 0", "frame 1", "frame 2", "frame 3", "frame 4"],
                "every buffered frame reaches the client, in emission order"
            );
        });
    }

    /// The trigger: the subscription thread emits more frames than the fan-out's ring holds
    /// before this task is polled again. The drain must report the lag, so the caller ends the
    /// stream and the client re-reads, rather than returning the frames it still holds as if
    /// nothing were missing.
    #[test]
    fn a_drain_that_falls_behind_the_ring_reports_the_lag() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("a runtime");
        runtime.block_on(async {
            let (sender, mut events) = broadcast::channel(2);
            for n in 0..5 {
                sender
                    .send(Bytes::from(format!("frame {n}")))
                    .expect("the drain's subscription is open");
            }
            // The read answers only after the drain had a chance to copy what the ring still
            // holds: on the old skip-and-continue behavior the drain returns here, with the
            // dropped frames silently missing from its buffer.
            let mut read = Box::pin(async {
                tokio::time::sleep(Duration::from_millis(20)).await;
                "snapshot"
            });
            let (drained, _buffered) = drain_snapshot(&mut events, &mut read).await;
            assert!(
                matches!(drained, SnapshotDrain::Lagged),
                "a drain that fell behind the ring reports it instead of skipping frames"
            );
        });
    }

    /// The trigger: the read answers in the same poll in which the fan-out already has a
    /// `Lagged` answer ready, because the subscription thread emitted more frames than the ring
    /// holds while this task was not polled. The read used to win that poll and the stream went
    /// on with the snapshot and the ring's retained tail; the lag must be reported instead.
    ///
    /// `select!` picks a ready branch at random, so one round proves nothing: every round has
    /// both branches ready, and the old behavior needs only one round that chose the read.
    #[test]
    fn a_finished_read_does_not_hide_a_lag_that_already_happened() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("a runtime");
        runtime.block_on(async {
            for round in 0..64 {
                let (sender, mut events) = broadcast::channel(2);
                for n in 0..5 {
                    sender
                        .send(Bytes::from(format!("frame {n}")))
                        .expect("the drain's subscription is open");
                }
                // The read is ready before the drain's first poll, so both branches are ready
                // at once and the ring is already three frames past its capacity.
                let mut read = Box::pin(std::future::ready("snapshot"));
                let (drained, _buffered) = drain_snapshot(&mut events, &mut read).await;
                assert!(
                    matches!(drained, SnapshotDrain::Lagged),
                    "round {round}: a read that answers after the ring fell behind reports the lag"
                );
            }
        });
    }

    /// A relative `--config` names the file the hub read, not a file beside the child's
    /// repository: `spawn_held` starts the child in the repository (ADR 0048 §16).
    #[test]
    fn a_relative_configuration_path_is_joined_with_the_hubs_directory() {
        assert_eq!(
            absolute_config(PathBuf::from("alt.toml"), Path::new("/srv/hub")),
            PathBuf::from("/srv/hub/alt.toml")
        );
        assert_eq!(
            absolute_config(PathBuf::from("/etc/radian.toml"), Path::new("/srv/hub")),
            PathBuf::from("/etc/radian.toml"),
            "an absolute path is passed through, symlinks and all"
        );
    }

    #[test]
    fn the_bind_address_is_the_configured_one() {
        let address = bind_address(IpAddr::V4(Ipv4Addr::LOCALHOST), 7341);
        assert_eq!(address.ip(), IpAddr::V4(Ipv4Addr::LOCALHOST));
        assert_eq!(address.port(), 7341);
        let any = bind_address(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 7341);
        assert_eq!(any.to_string(), "0.0.0.0:7341");
        let v6 = bind_address(IpAddr::V6(std::net::Ipv6Addr::LOCALHOST), 7341);
        assert_eq!(v6.to_string(), "[::1]:7341");
    }

    #[test]
    fn a_non_loopback_bind_has_one_warning_and_loopback_has_none() {
        assert!(bind_warning(IpAddr::V4(Ipv4Addr::LOCALHOST)).is_none());
        assert!(bind_warning(IpAddr::V6(std::net::Ipv6Addr::LOCALHOST)).is_none());
        let warning = bind_warning(IpAddr::V4(Ipv4Addr::UNSPECIFIED)).expect("0.0.0.0 warns");
        assert!(warning.contains("unencrypted"), "{warning}");
        assert!(warning.contains("tailscale serve"), "{warning}");
        let v6 = bind_warning(IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED)).expect(":: warns");
        assert!(v6.contains("unencrypted"), "{v6}");
    }

    /// A fixed interface list, in interface order: loopback, link-local, a down interface, then
    /// two eligible addresses. The chooser is tested on it so no test depends on this machine.
    fn candidate(address: [u8; 4], up: bool) -> LanCandidate {
        LanCandidate {
            address: Ipv4Addr::from(address),
            up,
        }
    }

    fn lan_candidates() -> Vec<LanCandidate> {
        vec![
            candidate([127, 0, 0, 1], true),
            candidate([169, 254, 3, 4], true),
            candidate([10, 0, 0, 5], false),
            candidate([192, 0, 2, 10], true),
            candidate([192, 0, 2, 11], true),
        ]
    }

    #[test]
    fn the_lan_chooser_skips_loopback_link_local_and_down_interfaces() {
        assert_eq!(
            lan_address(&lan_candidates()),
            Some(Ipv4Addr::new(192, 0, 2, 10))
        );
        assert_eq!(lan_address(&[]), None, "an empty list has no address");
        assert_eq!(
            lan_address(&lan_candidates()[..3]),
            None,
            "loopback, link-local and a down interface are all skipped"
        );
    }

    #[test]
    fn a_url_flag_sets_the_pairing_url_and_drops_a_trailing_slash() {
        let chosen = pairing(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            7341,
            Some("https://mac.example.ts.net/"),
            &[],
        )
        .expect("an absolute https URL with a host");
        assert_eq!(chosen.url, "https://mac.example.ts.net");
        assert!(!chosen.fallback);
        assert_eq!(
            payload(&chosen.url, "abc"),
            "radian://hub?url=https%3A%2F%2Fmac.example.ts.net&token=abc"
        );
        assert_eq!(
            chosen.others,
            vec!["http://127.0.0.1:7341".to_owned()],
            "a URL that is not loopback lists the other addresses"
        );
    }

    #[test]
    fn a_url_flag_keeps_a_slash_that_ends_a_query_or_fragment() {
        for url in [
            "https://mac.example.ts.net/?next=/",
            "https://mac.example.ts.net/#a/",
            "https://mac.example.ts.net/hub/?x=/",
            "https://mac.example.ts.net/hub/#/",
        ] {
            let chosen = pairing(IpAddr::V4(Ipv4Addr::LOCALHOST), 7341, Some(url), &[]).expect(url);
            assert_eq!(chosen.url, url, "a query or fragment value keeps its slash");
        }
        let path = pairing(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            7341,
            Some("https://mac.example.ts.net/hub/"),
            &[],
        )
        .expect("an absolute https URL with a host");
        assert_eq!(path.url, "https://mac.example.ts.net/hub");
    }

    #[test]
    fn a_url_flag_that_is_not_an_absolute_http_url_is_a_usage_error() {
        for url in [
            "ftp://x",
            "mac.local",
            "",
            "http://",
            "http:///hub",
            "http://:7341",
            "http://host:",
            "https://host:70000",
            "http://[not-an-ip]/",
            "https://example.test/%zz",
        ] {
            let error =
                pairing(IpAddr::V4(Ipv4Addr::LOCALHOST), 7341, Some(url), &[]).expect_err(url);
            assert_eq!(error.exit(), crate::store::Exit::Usage, "{url}");
            assert!(
                error.message().contains("--url"),
                "{url}: {}",
                error.message()
            );
        }
        for url in [
            "http://127.0.0.1:7341",
            "HTTPS://mac.example.ts.net",
            "http://[::1]:7341",
            "https://mac.example.ts.net/hub?x=1",
            "https://mac.example.ts.net/a%20b",
        ] {
            assert!(
                pairing(IpAddr::V4(Ipv4Addr::LOCALHOST), 7341, Some(url), &[]).is_ok(),
                "{url} is an absolute http or https URL with a host"
            );
        }
    }

    #[test]
    fn a_loopback_bind_keeps_the_loopback_url() {
        let chosen = pairing(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            7341,
            None,
            &lan_candidates(),
        )
        .unwrap();
        assert_eq!(chosen.url, "http://127.0.0.1:7341");
        assert!(chosen.others.is_empty(), "{chosen:?}");
        assert!(!chosen.fallback);
        // Any loopback bind, IPv6 included, prints the loopback URL a client can reach.
        let v6 = pairing(IpAddr::V6(std::net::Ipv6Addr::LOCALHOST), 7341, None, &[]).unwrap();
        assert_eq!(v6.url, "http://127.0.0.1:7341");
        assert!(v6.others.is_empty(), "{v6:?}");
        assert!(!v6.fallback);
    }

    #[test]
    fn a_specific_bind_prints_that_address() {
        let chosen = pairing(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 10)), 7341, None, &[]).unwrap();
        assert_eq!(chosen.url, "http://192.0.2.10:7341");
        assert!(!chosen.fallback);
    }

    #[test]
    fn an_unspecified_bind_prints_the_first_lan_address() {
        for bind in [
            IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED),
        ] {
            let chosen = pairing(bind, 7341, None, &lan_candidates()).unwrap();
            assert_eq!(chosen.url, "http://192.0.2.10:7341", "{bind}");
            assert!(!chosen.fallback);
            assert_eq!(
                chosen.others,
                vec![
                    "http://127.0.0.1:7341".to_owned(),
                    "http://192.0.2.11:7341".to_owned()
                ],
                "{bind}"
            );
        }
    }

    #[test]
    fn an_unspecified_bind_without_a_lan_address_falls_back_to_loopback() {
        let chosen = pairing(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 7341, None, &[]).unwrap();
        assert_eq!(chosen.url, "http://127.0.0.1:7341");
        assert!(chosen.fallback, "the command prints one line saying so");
        assert!(chosen.others.is_empty());
    }

    #[test]
    fn a_path_is_decoded_and_an_invalid_escape_is_kept() {
        assert_eq!(decode("src%2Fmain.rs"), "src/main.rs");
        assert_eq!(decode("a%2"), "a%2");
        assert_eq!(decode("plain.txt"), "plain.txt");
    }

    #[test]
    fn the_diff_cap_is_256_kib_and_never_splits_a_character() {
        assert_eq!(DIFF_TEXT_LIMIT, 262_144);
        let text = "aé";
        let (cut, truncated) = cap(text, 2);
        assert!(truncated);
        assert_eq!(cut, "a", "two bytes would split the character after it");
        let (whole, truncated) = cap("abc", 3);
        assert!(!truncated);
        assert_eq!(whole, "abc");
    }

    #[test]
    fn the_action_route_takes_the_eight_owner_actions_and_not_start_or_reset_budget() {
        for name in [
            "confirm", "merge", "cancel", "pause", "reverify", "reject", "auto", "priority",
        ] {
            assert!(
                OwnerAction::parse(name).is_some(),
                "`{name}` is an owner action"
            );
        }
        for name in ["start", "reset-budget", "explode", ""] {
            assert!(
                OwnerAction::parse(name).is_none(),
                "`{name}` is not a write route"
            );
        }
    }

    /// The mapping table, one assertion per outcome (ADR 0048 decision 16). A mutating change
    /// to one row fails here before the integration suite sees it.
    #[test]
    fn every_owner_action_outcome_maps_to_its_status() {
        assert_eq!(
            action_status(&Error::Usage(review::EMPTY_REJECT_NOTE.to_owned())),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            action_status(&Error::Usage("`sometimes` is not an auto level".to_owned())),
            StatusCode::UNPROCESSABLE_ENTITY
        );
        assert_eq!(
            action_status(&Error::Locked(crate::store::STATE_LOCK)),
            StatusCode::CONFLICT
        );
        assert_eq!(
            action_status(&Error::Refused("task is doing.running".to_owned())),
            StatusCode::CONFLICT
        );
        assert_eq!(
            action_status(&Error::ChangedOutsideArc("TK0001-x".to_owned())),
            StatusCode::CONFLICT
        );
        assert_eq!(action_status(&Error::NotInitialized), StatusCode::CONFLICT);
        assert_eq!(
            action_status(&Error::Failed("no plan or task `TK9999-x`".to_owned())),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            action_status(&Error::Failed(
                "no repository named `nope` is registered".to_owned()
            )),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            action_status(&Error::Failed("no task `TK9999-x`".to_owned())),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            action_status(&Error::Failed("no plan `PL9999-x`".to_owned())),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            action_status(&Error::Failed("the push failed".to_owned())),
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    /// The pid text left in `runner.lock` outlives the lock. A stale pid is not a holder: the
    /// repository is neither scheduled nor reported with a `lock_holder`.
    #[test]
    fn a_stale_runner_lock_pid_is_not_reported_as_scheduled() {
        let dir = crate::test_support::TempDir::new("hub-stale-lock");
        let arc = dir.path().join(".radian").join("arc");
        std::fs::create_dir_all(&arc).expect("the arc directory is creatable");
        std::fs::write(
            arc.join(crate::store::RUNNER_LOCK),
            std::process::id().to_string(),
        )
        .expect("the stale lock file writes");
        let store = Store::at(dir.path());
        assert!(store.runner_lock_free(), "no process holds the lock");
        assert_eq!(
            repo_view(&store),
            (false, None),
            "a stale pid is not a holder"
        );
    }
}
