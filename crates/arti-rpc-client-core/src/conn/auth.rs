//! Authentication for RpcConn.

use std::sync::Arc;

use serde::{de::DeserializeOwned, Deserialize, Serialize};

use crate::msgs::{request::Request, ObjectId};

use super::{ConnectError, ProtoError, RpcConn};

impl super::SuccessResponse {
    /// Try to decode the "result" field of a SuccessResponse as an instance of `D`.
    //
    // TODO RPC: Move this.  Possibly, make it public.  If we do, it will need a different error type.
    pub(crate) fn deserialize_as<D: DeserializeOwned>(&self) -> Result<D, ProtoError> {
        /// Helper object for decoding the "result" field.
        #[derive(Deserialize)]
        struct Response<R> {
            /// The decoded value.
            result: R,
        }

        let r: Response<D> = serde_json::from_str(self.as_ref())
            .map_err(|e| ProtoError::CouldNotDecode(Arc::new(e)))?;
        Ok(r.result)
    }
}

/// Arguments to an `auth:authenticate` request.
#[derive(Serialize, Debug)]
struct AuthParams<'a> {
    /// The authentication scheme we are using.
    scheme: &'a str,
}
/// Response to an `auth:authenticate` request.
#[derive(Deserialize, Debug)]
struct Authenticated {
    /// A session object that we use to access the rest of Arti's functionality.
    session: ObjectId,
}

impl RpcConn {
    /// Try to negotiate "inherent" authentication, using the provided scheme name.
    ///
    /// (Inherent authentication is available whenever the client proves that they
    /// are authorized through being able to connect to Arti at all.  Examples
    /// include connecting to a unix domain socket, and an in-process Arti implementation.)
    pub(crate) fn authenticate_inherent(
        &self,
        scheme_name: &str,
    ) -> Result<ObjectId, ConnectError> {
        let r: Request<AuthParams> = Request::new(
            ObjectId::connection_id(),
            "auth:authenticate",
            AuthParams {
                scheme: scheme_name,
            },
        );
        let authenticated = self
            .execute(&r.encode()?)?
            .map_err(ConnectError::AuthenticationRejected)?
            .deserialize_as::<Authenticated>()?;

        Ok(authenticated.session)
    }
}
