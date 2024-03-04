#![cfg_attr(docsrs, feature(doc_auto_cfg, doc_cfg))]
#![doc = include_str!("../README.md")]
// @@ begin lint list maintained by maint/add_warning @@
#![cfg_attr(not(ci_arti_stable), allow(renamed_and_removed_lints))]
#![cfg_attr(not(ci_arti_nightly), allow(unknown_lints))]
#![warn(missing_docs)]
#![warn(noop_method_call)]
#![warn(unreachable_pub)]
#![warn(clippy::all)]
#![deny(clippy::await_holding_lock)]
#![deny(clippy::cargo_common_metadata)]
#![deny(clippy::cast_lossless)]
#![deny(clippy::checked_conversions)]
#![warn(clippy::cognitive_complexity)]
#![deny(clippy::debug_assert_with_mut_call)]
#![deny(clippy::exhaustive_enums)]
#![deny(clippy::exhaustive_structs)]
#![deny(clippy::expl_impl_clone_on_copy)]
#![deny(clippy::fallible_impl_from)]
#![deny(clippy::implicit_clone)]
#![deny(clippy::large_stack_arrays)]
#![warn(clippy::manual_ok_or)]
#![deny(clippy::missing_docs_in_private_items)]
#![warn(clippy::needless_borrow)]
#![warn(clippy::needless_pass_by_value)]
#![warn(clippy::option_option)]
#![deny(clippy::print_stderr)]
#![deny(clippy::print_stdout)]
#![warn(clippy::rc_buffer)]
#![deny(clippy::ref_option_ref)]
#![warn(clippy::semicolon_if_nothing_returned)]
#![warn(clippy::trait_duplication_in_bounds)]
#![deny(clippy::unchecked_duration_subtraction)]
#![deny(clippy::unnecessary_wraps)]
#![warn(clippy::unseparated_literal_suffix)]
#![deny(clippy::unwrap_used)]
#![allow(clippy::let_unit_value)] // This can reasonably be done for explicitness
#![allow(clippy::uninlined_format_args)]
#![allow(clippy::significant_drop_in_scrutinee)] // arti/-/merge_requests/588/#note_2812945
#![allow(clippy::result_large_err)] // temporary workaround for arti#587
#![allow(clippy::needless_raw_string_hashes)] // complained-about code is fine, often best
//! <!-- @@ end lint list maintained by maint/add_warning @@ -->

use std::fmt::{self, Display};
use std::str::FromStr;

use derive_adhoc::Adhoc;
use serde::{Deserialize, Serialize};
use serde::{Deserializer, Serializer};
use thiserror::Error;

use tor_basic_utils::impl_debug_hex;
use tor_keymgr::KeySpecifierComponentViaDisplayFromStr;

#[macro_use] // SerdeStringOrTransparent
mod time_store;

mod anon_level;
pub mod config;
mod err;
mod helpers;
mod ipt_establish;
mod ipt_mgr;
mod ipt_set;
mod keys;
mod netdir;
mod nickname;
mod publish;
mod rend_handshake;
mod replay;
mod req;
pub mod status;
mod svc;
mod timeout_track;

// rustdoc doctests can't use crate-public APIs, so are broken if provided for private items.
// So we export the whole module again under this name.
// Supports the Example in timeout_track.rs's module-level docs.
//
// Any out-of-crate user needs to write this ludicrous name in their code,
// so we don't need to put any warnings in the docs for the individual items.)
//
// (`#[doc(hidden)] pub mod timeout_track;` would work for the test but it would
// completely suppress the actual documentation, which is not what we want.)
#[doc(hidden)]
pub mod timeout_track_for_doctests_unstable_no_semver_guarantees {
    pub use crate::timeout_track::*;
}
#[doc(hidden)]
pub mod time_store_for_doctests_unstable_no_semver_guarantees {
    pub use crate::time_store::*;
}

pub use anon_level::Anonymity;
pub use config::OnionServiceConfig;
pub use err::{ClientError, EstablishSessionError, FatalError, IntroRequestError, StartupError};
pub use ipt_mgr::IptError;
pub use keys::{
    BlindIdKeypairSpecifier, BlindIdPublicKeySpecifier, DescSigningKeypairSpecifier,
    HsIdKeypairSpecifier, HsIdPublicKeySpecifier,
};
pub use nickname::{HsNickname, InvalidNickname};
pub use req::{RendRequest, StreamRequest};
pub use crate::netdir::NetdirProviderShutdown;
pub use publish::UploadError as DescUploadError;
pub use svc::{OnionService, RunningOnionService};

use err::IptStoreError;

/// Persistent local identifier for an introduction point
///
/// Changes when the IPT relay changes, or the IPT key material changes.
/// (Different for different `.onion` services, obviously)
///
/// Is a randomly-generated byte string, currently 32 long.
#[derive(Clone, Copy, Eq, PartialEq, Hash, Ord, PartialOrd, Adhoc)]
#[derive_adhoc(SerdeStringOrTransparent)]
pub(crate) struct IptLocalId([u8; 32]);

impl_debug_hex!(IptLocalId.0);

impl Display for IptLocalId {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        for v in self.0 {
            write!(f, "{v:02x}")?;
        }
        Ok(())
    }
}

/// Invalid [`IptLocalId`] - for example bad string representation
#[derive(Debug, Error, Clone, Eq, PartialEq)]
#[error("invalid IptLocalId")]
#[non_exhaustive]
pub(crate) struct InvalidIptLocalId {}

impl FromStr for IptLocalId {
    type Err = InvalidIptLocalId;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut b = [0; 32];
        hex::decode_to_slice(s, &mut b).map_err(|_: hex::FromHexError| InvalidIptLocalId {})?;
        Ok(IptLocalId(b))
    }
}

impl KeySpecifierComponentViaDisplayFromStr for IptLocalId {}

impl IptLocalId {
    /// Return a fixed dummy `IptLocalId`, for testing etc.
    ///
    /// The id is made by repeating `which` 32 times.
    #[cfg(test)]
    pub(crate) fn dummy(which: u8) -> Self {
        IptLocalId([which; 32]) // I can't think of a good way not to specify 32 again here
    }
}

pub use helpers::handle_rend_requests;

#[cfg(test)]
pub(crate) mod test {
    // @@ begin test lint list maintained by maint/add_warning @@
    #![allow(clippy::bool_assert_comparison)]
    #![allow(clippy::clone_on_copy)]
    #![allow(clippy::dbg_macro)]
    #![allow(clippy::print_stderr)]
    #![allow(clippy::print_stdout)]
    #![allow(clippy::single_char_pattern)]
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::unchecked_duration_subtraction)]
    #![allow(clippy::useless_vec)]
    #![allow(clippy::needless_pass_by_value)]
    //! <!-- @@ end test lint list maintained by maint/add_warning @@ -->
    use super::*;
    use itertools::{chain, Itertools};

    #[derive(Serialize, Deserialize, Eq, PartialEq, Debug)]
    struct IptLidTest {
        lid: IptLocalId,
    }

    #[test]
    fn lid_serde() {
        let t = IptLidTest {
            lid: IptLocalId::dummy(7),
        };
        let json = serde_json::to_string(&t).unwrap();
        assert_eq!(
            json,
            // This also tests <IptLocalId as Display> since that's how we serialise it
            r#"{"lid":"0707070707070707070707070707070707070707070707070707070707070707"}"#,
        );
        let u: IptLidTest = serde_json::from_str(&json).unwrap();
        assert_eq!(t, u);

        let mpack = rmp_serde::to_vec_named(&t).unwrap();
        assert_eq!(
            mpack,
            chain!(&[129, 163], b"lid", &[220, 0, 32], &[0x07; 32],)
                .cloned()
                .collect_vec()
        );
        let u: IptLidTest = rmp_serde::from_slice(&mpack).unwrap();
        assert_eq!(t, u);
    }
}
