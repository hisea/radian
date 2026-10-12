use std::collections::HashMap;
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

/// Routes progress notifications to the call that carries the matching token. Senders are
/// unbounded so the transport loop never blocks on a slow call; entries are removed by the
/// call itself when it settles, so unknown or late tokens simply find no route here.
pub(crate) type ProgressRoutes = Arc<
    Mutex<HashMap<ProgressToken, tokio::sync::mpsc::UnboundedSender<ProgressNotificationParam>>>,
>;

/// The MCP client side. Declares no sampling, roots or elicitation capability and answers any
/// such request at once with method-not-found, so a server can never block waiting on us.
///
/// rmcp's defaults differ (empty roots, declined elicitation), which a server could read as
/// real answers.
#[derive(Debug, Clone)]
pub(crate) struct Client {
    routes: ProgressRoutes,
}

impl Client {
    pub(crate) fn new(routes: ProgressRoutes) -> Self {
        Self { routes }
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
        let routes = self.routes.clone();
        async move {
            let sender = routes
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&params.progress_token)
                .cloned();
            if let Some(sender) = sender {
                // A failure means the call already settled and dropped its receiver.
                let _ = sender.send(params);
            }
        }
    }

    fn get_info(&self) -> ClientConfig {
        ClientConfig::new(
            ClientCapabilities::default(),
            Implementation::new("agent-runtime", env!("CARGO_PKG_VERSION")),
        )
    }
}
