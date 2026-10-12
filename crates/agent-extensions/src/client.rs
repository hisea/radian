use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::sync::{Arc, Mutex};

#[allow(deprecated)]
use rmcp::model::{
    ClientCapabilities, ClientConfig, CreateMessageRequestMethod, CreateMessageRequestParams,
    CreateMessageResult, ElicitRequestParams, ElicitResult, ElicitationCreateRequestMethod,
    Implementation, ListRootsRequestMethod, ListRootsResult, ProgressNotificationParam,
    ProgressToken,
};
use rmcp::service::{NotificationContext, RequestContext, RoleClient};
use rmcp::{ClientHandler, ErrorData};

/// Cap for notifications without a live route. Bounded so a chatty or hostile server cannot
/// grow host memory; the oldest are dropped first.
const ORPHAN_LIMIT: usize = 256;

/// Routes progress notifications to the call that carries the matching token.
///
/// rmcp mints the token only while sending the request, and the request is on the wire by the
/// time `send_cancellable_request` resolves, so a notification can arrive before its call has
/// registered a route. Unrouted notifications therefore wait in a bounded buffer and are
/// replayed, in order, by `register` — under the same lock `deliver` takes, which closes the
/// race without either side ever awaiting.
#[derive(Debug, Default)]
pub(crate) struct ProgressRouter {
    inner: Mutex<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    routes: HashMap<ProgressToken, tokio::sync::mpsc::UnboundedSender<ProgressNotificationParam>>,
    orphans: VecDeque<ProgressNotificationParam>,
}

impl ProgressRouter {
    /// Delivers one notification, from the rmcp service loop. Synchronous by design: the
    /// transport must never wait on a call.
    pub(crate) fn deliver(&self, params: ProgressNotificationParam) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        match inner.routes.get(&params.progress_token) {
            Some(sender) => {
                // A failure means the call already settled and dropped its receiver.
                let _ = sender.send(params);
            }
            None => {
                inner.orphans.push_back(params);
                while inner.orphans.len() > ORPHAN_LIMIT {
                    inner.orphans.pop_front();
                }
            }
        }
    }

    /// Registers a call's channel. Notifications that already arrived for the token are
    /// replayed into it here, under the lock, so nothing sent before registration is missed
    /// and nothing after is duplicated.
    pub(crate) fn register(
        &self,
        token: ProgressToken,
        sender: tokio::sync::mpsc::UnboundedSender<ProgressNotificationParam>,
    ) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let mut i = 0;
        while i < inner.orphans.len() {
            if inner.orphans[i].progress_token == token {
                let orphan = inner.orphans.remove(i).expect("index just checked");
                let _ = sender.send(orphan);
            } else {
                i += 1;
            }
        }
        inner.routes.insert(token, sender);
    }

    /// Removes a settled call's route; whatever arrives for its token afterwards stays in the
    /// orphan buffer until evicted. Tokens are unique, so a later call can never claim it.
    pub(crate) fn unregister(&self, token: &ProgressToken) {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .routes
            .remove(token);
    }

    /// Live routes, including any whose call has not yet cleaned up.
    pub(crate) fn route_count(&self) -> usize {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .routes
            .len()
    }

    #[cfg(test)]
    fn orphan_count(&self) -> usize {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .orphans
            .len()
    }
}

/// The MCP client side. Declares no sampling, roots or elicitation capability and answers any
/// such request at once with method-not-found, so a server can never block waiting on us.
///
/// rmcp's defaults differ (empty roots, declined elicitation), which a server could read as
/// real answers.
#[derive(Debug, Clone)]
pub(crate) struct Client {
    router: Arc<ProgressRouter>,
}

impl Client {
    pub(crate) fn new(router: Arc<ProgressRouter>) -> Self {
        Self { router }
    }
}

// Sampling and roots are deprecated from 2026-07-28 (SEP-2577), but a 2025-11-25 server may
// still send them, so they must still be refused explicitly.
#[allow(deprecated)]
impl ClientHandler for Client {
    async fn create_message(
        &self,
        _params: CreateMessageRequestParams,
        _context: RequestContext<RoleClient>,
    ) -> Result<CreateMessageResult, ErrorData> {
        Err(ErrorData::method_not_found::<CreateMessageRequestMethod>())
    }

    async fn list_roots(
        &self,
        _context: RequestContext<RoleClient>,
    ) -> Result<ListRootsResult, ErrorData> {
        Err(ErrorData::method_not_found::<ListRootsRequestMethod>())
    }

    async fn create_elicitation(
        &self,
        _request: ElicitRequestParams,
        _context: RequestContext<RoleClient>,
    ) -> Result<ElicitResult, ErrorData> {
        Err(ErrorData::method_not_found::<ElicitationCreateRequestMethod>())
    }

    fn on_progress(
        &self,
        params: ProgressNotificationParam,
        _context: NotificationContext<RoleClient>,
    ) -> impl Future<Output = ()> + Send + '_ {
        self.router.deliver(params);
        std::future::ready(())
    }

    fn get_info(&self) -> ClientConfig {
        ClientConfig::new(
            ClientCapabilities::default(),
            Implementation::new("agent-runtime", env!("CARGO_PKG_VERSION")),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::NumberOrString;
    use serde_json::json;

    fn token(n: i64) -> ProgressToken {
        ProgressToken(NumberOrString::Number(n))
    }

    fn notification(n: i64, progress: f64, message: &str) -> ProgressNotificationParam {
        let value = json!({"progressToken": n, "progress": progress, "message": message});
        serde_json::from_value(value).expect("valid notification")
    }

    #[test]
    fn live_routes_receive_their_notifications() {
        let router = ProgressRouter::default();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        router.register(token(1), tx);

        router.deliver(notification(1, 4.0, "mine"));
        router.deliver(notification(2, 5.0, "not mine"));

        assert_eq!(rx.try_recv().unwrap().message.as_deref(), Some("mine"));
        assert!(rx.try_recv().is_err());
        assert_eq!(router.orphan_count(), 1);
    }

    #[test]
    fn orphaned_notifications_replay_in_order_at_registration() {
        let router = ProgressRouter::default();
        router.deliver(notification(1, 1.0, "first"));
        router.deliver(notification(2, 9.0, "other call"));
        router.deliver(notification(1, 2.0, "second"));
        router.deliver(notification(1, 3.0, "third"));

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        router.register(token(1), tx);

        let messages: Vec<_> = [
            rx.try_recv().unwrap(),
            rx.try_recv().unwrap(),
            rx.try_recv().unwrap(),
        ]
        .into_iter()
        .map(|n| n.message.expect("set"))
        .collect();
        assert_eq!(messages, ["first", "second", "third"]);
        assert!(rx.try_recv().is_err());
        // The other call's notification is still buffered, waiting for its own route.
        assert_eq!(router.orphan_count(), 1);
    }

    #[test]
    fn the_orphan_buffer_is_bounded_and_drops_the_oldest() {
        let router = ProgressRouter::default();
        let far_over = ORPHAN_LIMIT + 100;
        for i in 0..far_over as i64 {
            router.deliver(notification(i, 1.0, "orphan"));
        }
        assert_eq!(router.orphan_count(), ORPHAN_LIMIT);

        // The first notifications were evicted; the latest survive.
        let (early, _) = tokio::sync::mpsc::unbounded_channel();
        router.register(token(0), early);
        let (late, mut late_rx) = tokio::sync::mpsc::unbounded_channel();
        router.register(token(far_over as i64 - 1), late);
        assert!(
            late_rx.try_recv().is_ok(),
            "the newest orphan must still be there"
        );
    }

    #[test]
    fn unregister_stops_delivery() {
        let router = ProgressRouter::default();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        router.register(token(1), tx);
        assert_eq!(router.route_count(), 1);

        router.unregister(&token(1));
        router.deliver(notification(1, 1.0, "late"));

        assert!(rx.try_recv().is_err());
        assert_eq!(router.route_count(), 0);
    }
}
