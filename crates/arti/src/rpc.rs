//! Experimental RPC support.

use anyhow::Result;
use arti_rpcserver::RpcMgr;
use derive_builder::Builder;
use futures::task::SpawnExt;
use listener::{RpcListenerMap, RpcListenerMapBuilder};
use serde::{Deserialize, Serialize};
use session::ArtiRpcSession;
use std::{path::Path, sync::Arc};
use tor_config::{define_list_builder_helper, impl_standard_builder, ConfigBuildError};
use tor_config_path::CfgPath;

use arti_client::TorClient;
use tor_rtcompat::Runtime;

pub(crate) mod conntarget;
pub(crate) mod listener;
mod proxyinfo;
mod session;

pub(crate) use session::{RpcStateSender, RpcVisibleArtiState};

cfg_if::cfg_if! {
    if #[cfg(all(feature="tokio", not(target_os="windows")))] {
        use tokio_crate::net::UnixListener ;
        use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
    } else if #[cfg(all(feature="async-std", not(target_os="windows")))] {
        use async_std::os::unix::net::UnixListener;
    } else if #[cfg(target_os="windows")] {
        compile_error!("Sorry, no windows support for RPC yet.");
        // TODO RPC: Tokio has a named pipe API; AsyncStd should let us construct
        // one via FromRawHandle.
    } else {
        compile_error!("You need to have tokio or async-std.");
    }
}

/// Configuration for Arti's RPC subsystem.
///
/// You cannot change this section on a running Arti client.
#[derive(Debug, Clone, Builder, Eq, PartialEq)]
#[builder(build_fn(error = "ConfigBuildError"))]
#[builder(derive(Debug, Serialize, Deserialize))]
#[builder_struct_attr(non_exhaustive)]
#[non_exhaustive]
pub struct RpcConfig {
    /// Location to listen for incoming RPC connections.
    #[builder(default = "default_rpc_path()")]
    pub(crate) rpc_listen: Option<CfgPath>,

    /// A set of named locations in which to find connect files.
    #[builder(sub_builder)]
    #[builder_field_attr(serde(default))]
    listen: RpcListenerMap,

    /// A list of default connect points to bind if none are found under `listen`.
    #[builder(sub_builder)]
    #[builder_field_attr(serde(default))]
    listen_default: ListenDefaults,
}
impl_standard_builder! { RpcConfig }

/// Type alias to enable sub_builder to work.
type ListenDefaults = Vec<String>;

define_list_builder_helper! {
    pub struct ListenDefaultsBuilder {
        values: [String],
    }
    built: Vec<String> = values;
    default = listen_defaults_defaults();
    item_build: |item| Ok(item.clone());
}

/// Return default values for `RpcConfig.listen_default`
fn listen_defaults_defaults() -> Vec<String> {
    vec![tor_rpc_connect::USER_DEFAULT_CONNECT_POINT.to_string()]
}

/// Return the default value for our configuration path.
#[allow(clippy::unnecessary_wraps)]
fn default_rpc_path() -> Option<CfgPath> {
    let s = if cfg!(target_os = "windows") {
        r"\\.\pipe\arti\SOCKET"
    } else {
        "~/.local/run/arti/SOCKET"
    };
    Some(CfgPath::new(s.to_string()))
}

/// Run an RPC listener task to accept incoming connections at the Unix
/// socket address of `path`.
pub(crate) fn launch_rpc_listener<R: Runtime>(
    runtime: &R,
    path: impl AsRef<Path>,
    client: TorClient<R>,
    rpc_state: Arc<RpcVisibleArtiState>,
) -> Result<Arc<RpcMgr>> {
    // TODO RPC: there should be an error return instead.

    // TODO RPC: Maybe the UnixListener functionality belongs in tor-rtcompat?
    // But I certainly don't want to make breaking changes there if we can help
    // it.
    let listener = UnixListener::bind(path)?;
    let rpc_mgr = RpcMgr::new(move |auth| ArtiRpcSession::new(auth, &client, &rpc_state))?;
    // Register methods. Needed since TorClient is generic.
    //
    // TODO: If we accumulate a large number of generics like this, we should do this elsewhere.
    rpc_mgr.register_rpc_methods(TorClient::<R>::rpc_methods());
    rpc_mgr.register_rpc_methods(arti_rpcserver::rpc_methods::<R>());

    let rt_clone = runtime.clone();
    let rpc_mgr_clone = rpc_mgr.clone();

    // TODO: Using spawn in this way makes it hard to report whether we
    // succeeded or not. This is something we should fix when we refactor
    // our service-launching code.
    runtime.spawn(async {
        let result = run_rpc_listener(rt_clone, listener, rpc_mgr_clone).await;
        if let Err(e) = result {
            tracing::warn!("RPC manager quit with an error: {}", e);
        }
    })?;
    Ok(rpc_mgr)
}

/// Backend function to implement an RPC listener: runs in a loop.
async fn run_rpc_listener<R: Runtime>(
    runtime: R,
    listener: UnixListener,
    rpc_mgr: Arc<RpcMgr>,
) -> Result<()> {
    loop {
        let (stream, _addr) = listener.accept().await?;
        // TODO RPC: Perhaps we should have rpcmgr hold the client reference?
        let connection = rpc_mgr.new_connection();
        let (input, output) = stream.into_split();

        #[cfg(feature = "tokio")]
        let (input, output) = (input.compat(), output.compat_write());

        runtime.spawn(async {
            let result = connection.run(input, output).await;
            if let Err(e) = result {
                tracing::warn!("RPC session ended with an error: {}", e);
            }
        })?;
    }
}

#[cfg(test)]
mod test {
    // @@ begin test lint list maintained by maint/add_warning @@
    #![allow(clippy::bool_assert_comparison)]
    #![allow(clippy::clone_on_copy)]
    #![allow(clippy::dbg_macro)]
    #![allow(clippy::mixed_attributes_style)]
    #![allow(clippy::print_stderr)]
    #![allow(clippy::print_stdout)]
    #![allow(clippy::single_char_pattern)]
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::unchecked_duration_subtraction)]
    #![allow(clippy::useless_vec)]
    #![allow(clippy::needless_pass_by_value)]
    //! <!-- @@ end test lint list maintained by maint/add_warning @@ -->

    #[test]
    fn rpc_method_names() {
        // We run this from a nice high level module, to ensure that as many method names as
        // possible will be in-scope.
        let problems = tor_rpcbase::check_method_names([]);

        for (m, err) in &problems {
            eprintln!("Bad method name {m:?}: {err}");
        }
        assert!(problems.is_empty());
    }
}
