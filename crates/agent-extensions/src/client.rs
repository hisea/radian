#[allow(deprecated)]
use rmcp::model::{
    ClientCapabilities, ClientConfig, CreateMessageRequestMethod, CreateMessageRequestParams,
    CreateMessageResult, ElicitRequestParams, ElicitResult, ElicitationCreateRequestMethod,
    Implementation, ListRootsRequestMethod, ListRootsResult,
};
use rmcp::service::{RequestContext, RoleClient};
use rmcp::{ClientHandler, ErrorData};

/// The MCP client side. Declares no sampling, roots or elicitation capability and answers any
/// such request at once with method-not-found, so a server can never block waiting on us.
///
/// rmcp's defaults differ (empty roots, declined elicitation), which a server could read as
/// real answers.
#[derive(Debug, Clone, Default)]
pub(crate) struct Client;

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

    fn get_info(&self) -> ClientConfig {
        ClientConfig::new(
            ClientCapabilities::default(),
            Implementation::new("agent-runtime", env!("CARGO_PKG_VERSION")),
        )
    }
}
