//! High-level APIs for an RPC session
//!
//! A "session" is created when a user authenticates on an RPC connection.  It
//! is the root for all other RPC capabilities.

use arti_client::{
    rpc::{ClientConnectionResult, ClientConnectionTarget},
    TorClient,
};
use async_trait::async_trait;
use derive_deftly::Deftly;
use std::{net::IpAddr, sync::Arc};
use tor_rtcompat::Runtime;

use tor_rpcbase::{self as rpc, static_rpc_invoke_fn, templates::*, ObjectArcExt as _};

/// An authenticated RPC session: a capability through which most other RPC functionality is available
///
/// This relates to [`Connection`](crate::Connection) as follows:
///
///  * A `Connection` exists prior to authentication;
///    whereas an `RpcSession` comes into being as a result of authentication.
///
///  * The `RpcSession` is principally owned by the `Connection`'s object table.
///
///  * Typically, after authentication, there is one `RpcSession` for the `Connection`.
///    But a client may authenticate more than once; each time produces a new `RpcSession`.
#[derive(Deftly)]
#[derive_deftly(Object)]
#[deftly(
    rpc(expose_outside_of_session),
    rpc(downcastable_to = "ClientConnectionTarget")
)]
pub struct RpcSession {
    /// An inner TorClient object that we use to implement remaining
    /// functionality.
    #[allow(unused)]
    client: Arc<dyn Client>,
}

/// Type-erased `TorClient``, as used within an RpcSession.
trait Client: rpc::Object {
    /// Return a new isolated TorClient.
    fn isolated_client(&self) -> Arc<dyn rpc::Object>;

    /// Upcast `self` to an rpc::Object.
    fn upcast_arc(self: Arc<Self>) -> Arc<dyn rpc::Object>;
}

impl<R: Runtime> Client for TorClient<R> {
    fn isolated_client(&self) -> Arc<dyn rpc::Object> {
        Arc::new(TorClient::isolated_client(self))
    }

    fn upcast_arc(self: Arc<Self>) -> Arc<dyn rpc::Object> {
        self
    }
}

impl RpcSession {
    /// Create a new session object containing a single client object.
    pub fn new_with_client<R: Runtime>(client: Arc<arti_client::TorClient<R>>) -> Arc<Self> {
        Arc::new(Self { client })
    }

    /// Return a view of the client associated with this session, as an `Arc<dyn
    /// ClientConnectionTarget>.`
    fn client_as_conn_target(&self) -> Arc<dyn ClientConnectionTarget> {
        self.client
            .clone()
            .upcast_arc()
            .cast_to_arc_trait()
            .ok()
            .expect("Somehow we had a client that was not a ClientConnectionTarget?")
    }
}

/// RPC method to release a single strong reference.
#[derive(Debug, serde::Deserialize, Deftly)]
#[derive_deftly(DynMethod)]
#[deftly(rpc(method_name = "rpc:release"))]
struct RpcRelease {
    /// The object to release. Must be a strong reference.
    ///
    /// TODO RPC: Releasing a weak reference is perilous and hard-to-define
    /// based on how we have implemented our object ids.  If you tell the objmap
    /// to "release" a single name for a weak reference, you are releasing every
    /// name for that weak reference, which may have surprising results.
    ///
    /// This might be a sign of a design problem.
    obj: rpc::ObjectId,
}
/// RPC method to release a single strong reference, creating a weak reference
/// in its place.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct RpcDowngrade {
    /// The object to downgrade
    obj: rpc::ObjectId,
}

impl rpc::Method for RpcRelease {
    type Output = rpc::Nil;
    type Update = rpc::NoUpdates;
    type Error = rpc::RpcError;
}

/// Implementation for calling "release" on a Session.
async fn rpc_release(
    _obj: Arc<RpcSession>,
    method: Box<RpcRelease>,
    ctx: Arc<dyn rpc::Context>,
) -> Result<rpc::Nil, rpc::RpcError> {
    ctx.release_owned(&method.obj)?;
    Ok(rpc::Nil::default())
}

/// A simple temporary method to echo a reply.
#[derive(Debug, serde::Deserialize, serde::Serialize, Deftly)]
#[derive_deftly(DynMethod)]
#[deftly(rpc(method_name = "arti:x_echo"))]
struct Echo {
    /// A message to echo.
    msg: String,
}

impl rpc::Method for Echo {
    type Output = Echo;
    type Update = rpc::NoUpdates;
    type Error = rpc::RpcError;
}

/// Implementation for calling "echo" on a Session.
///
/// TODO RPC: Remove this. It shouldn't exist.
async fn echo_on_session(
    _obj: Arc<RpcSession>,
    method: Box<Echo>,
    _ctx: Arc<dyn rpc::Context>,
) -> Result<Echo, rpc::RpcError> {
    Ok(*method)
}

/// An RPC method to return the default client for a session.
#[derive(Debug, serde::Deserialize, serde::Serialize, Deftly)]
#[derive_deftly(DynMethod)]
#[deftly(rpc(method_name = "arti:get_client"))]
struct GetClient {}

impl rpc::Method for GetClient {
    type Output = rpc::SingletonId;
    type Update = rpc::NoUpdates;
    type Error = rpc::RpcError;
}

/// Implement GetClient on an RpcSession.
async fn get_client_on_session(
    session: Arc<RpcSession>,
    _method: Box<GetClient>,
    ctx: Arc<dyn rpc::Context>,
) -> Result<rpc::SingletonId, rpc::RpcError> {
    Ok(rpc::SingletonId::from(
        // TODO RPC: This relies (somewhat) on deduplication properties for register_owned.
        ctx.register_owned(session.client.clone().upcast_arc()),
    ))
}

/// Implement IsolatedClient on an RpcSession.
async fn isolated_client_on_session(
    session: Arc<RpcSession>,
    _method: Box<arti_client::rpc::IsolatedClient>,
    ctx: Arc<dyn rpc::Context>,
) -> Result<rpc::SingletonId, rpc::RpcError> {
    let new_client = session.client.isolated_client();
    Ok(rpc::SingletonId::from(ctx.register_owned(new_client)))
}

static_rpc_invoke_fn! {
    rpc_release;
    echo_on_session;
    get_client_on_session;
    isolated_client_on_session;
}

#[async_trait]
impl ClientConnectionTarget for RpcSession {
    async fn connect_with_prefs(
        &self,
        target: &arti_client::TorAddr,
        prefs: &arti_client::StreamPrefs,
    ) -> ClientConnectionResult<arti_client::DataStream> {
        self.client_as_conn_target()
            .connect_with_prefs(target, prefs)
            .await
    }

    async fn resolve_with_prefs(
        &self,
        hostname: &str,
        prefs: &arti_client::StreamPrefs,
    ) -> ClientConnectionResult<Vec<IpAddr>> {
        self.client_as_conn_target()
            .resolve_with_prefs(hostname, prefs)
            .await
    }

    async fn resolve_ptr_with_prefs(
        &self,
        addr: IpAddr,
        prefs: &arti_client::StreamPrefs,
    ) -> ClientConnectionResult<Vec<String>> {
        self.client_as_conn_target()
            .resolve_ptr_with_prefs(addr, prefs)
            .await
    }
}
