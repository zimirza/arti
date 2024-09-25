//! Read-only C Tor service key store implementation
//!
//! See [`CTorServiceKeystore`] for more details.

use crate::keystore::ctor::err::{CTorKeystoreError, MalformedServiceKeyError};
use crate::keystore::ctor::CTorKeystore;
use crate::keystore::fs_utils::{checked_op, FilesystemAction, FilesystemError};
use crate::keystore::{EncodableKey, ErasedKey, KeySpecifier, Keystore, KeystoreId};
use crate::{CTorPath, CTorServicePath, KeyPath, Result};

use fs_mistrust::Mistrust;
use tor_basic_utils::PathExt as _;
use tor_error::internal;
use tor_key_forge::KeyType;
use tor_llcrypto::pk::ed25519;
use tor_persist::hsnickname::HsNickname;

use std::path::{Path, PathBuf};
use std::result::Result as StdResult;
use std::sync::Arc;

/// A read-only C Tor service keystore.
///
/// This keystore provides read-only access to the hidden service keys
/// rooted at a given `HiddenServiceDirectory` directory
/// (see `HiddenServiceDirectory` in `tor(1)`).
///
/// This keystore can be used to read the `HiddenServiceDirectory/private_key`
/// and `HiddenServiceDirectory/public_key` C Tor keys, specified by
/// [`CTorServicePath::PrivateKey`] (with [`KeyType::Ed25519ExpandedKeypair`])
/// and [`CTorServicePath::PublicKey`] (with [`KeyType::Ed25519PublicKey`]),
/// respectively. Any other files stored in `HiddenServiceDirectory` will be ignored.
///
/// The only supported [`Keystore`] operations are [`contains`](Keystore::contains),
/// [`get`](Keystore::get), and [`list`](Keystore::list). All other keystore operations
/// will return an error.
///
/// This keystore implementation uses the [`CTorPath`] of the requested [`KeySpecifier`]
/// and the [`KeyType`] to identify the appropriate key.
/// If the requested `CTorPath` is not [`Service`](CTorPath::Service),
/// or if the [`HsNickname`] specified in the `CTorPath` does not match the nickname of this store,
/// the key will be declared not found.
/// If the requested `CTorPath` is [`Service`](CTorPath::Service),
/// but the `KeyType` and [`CTorServicePath`] are mismatched,
/// an error is returned.
pub struct CTorServiceKeystore {
    /// The underlying keystore
    keystore: CTorKeystore,
    /// The nickname of the service this keystore is meant for
    nickname: HsNickname,
}

impl CTorServiceKeystore {
    /// Create a new `CTorServiceKeystore`
    /// rooted at the specified `keystore_dir` directory.
    ///
    /// This function returns an error if `keystore_dir` is not a directory,
    /// or if it does not conform to the requirements of the specified `Mistrust`.
    pub fn from_path_and_mistrust(
        keystore_dir: impl AsRef<Path>,
        mistrust: &Mistrust,
        id: KeystoreId,
        nickname: HsNickname,
    ) -> Result<Self> {
        let keystore = CTorKeystore::from_path_and_mistrust(keystore_dir, mistrust, id)?;

        Ok(Self { keystore, nickname })
    }
}

/// Extract the key path (relative to the keystore root) from the specified result `res`,
/// or return an error.
///
/// If `res` is `None`, return `ret`.
macro_rules! rel_path_if_supported {
    ($self:expr, $spec:expr, $ret:expr, $key_type:expr) => {{
        // If the key specifier doesn't have a CTorPath,
        // we can't possibly handle this key.
        let Some(ctor_path) = $spec.ctor_path() else {
            return $ret;
        };

        // This keystore only deals with service keys...
        let CTorPath::Service { path, nickname } = ctor_path else {
            return $ret;
        };

        // ...more specifically, it has the service keys of a *particular* service
        // (identified by nickname).
        if nickname != $self.nickname {
            return $ret;
        };

        let relpath = $self.keystore.rel_path(PathBuf::from(path.to_string()));
        match ($key_type, &path) {
            (KeyType::Ed25519ExpandedKeypair, CTorServicePath::PrivateKey)
            | (KeyType::Ed25519PublicKey, CTorServicePath::PublicKey) => Ok(()),
            _ => Err(CTorKeystoreError::InvalidKeyType {
                key_type: $key_type.clone(),
                key: format!("key {}", relpath.rel_path_unchecked().display_lossy()),
            }),
        }?;

        relpath
    }};
}

impl Keystore for CTorServiceKeystore {
    fn id(&self) -> &KeystoreId {
        &self.keystore.id
    }

    fn contains(&self, key_spec: &dyn KeySpecifier, key_type: &KeyType) -> Result<bool> {
        let path = rel_path_if_supported!(self, key_spec, Ok(false), key_type);

        let meta = match checked_op!(metadata, path) {
            Ok(meta) => meta,
            Err(fs_mistrust::Error::NotFound(_)) => return Ok(false),
            Err(e) => {
                return Err(FilesystemError::FsMistrust {
                    action: FilesystemAction::Read,
                    path: path.rel_path_unchecked().into(),
                    err: e.into(),
                })
                .map_err(|e| CTorKeystoreError::Filesystem(e).into());
            }
        };

        // The path exists, now check that it's actually a file and not a directory or symlink.
        if meta.is_file() {
            Ok(true)
        } else {
            Err(
                CTorKeystoreError::Filesystem(FilesystemError::NotARegularFile(
                    path.rel_path_unchecked().into(),
                ))
                .into(),
            )
        }
    }

    fn get(&self, key_spec: &dyn KeySpecifier, key_type: &KeyType) -> Result<Option<ErasedKey>> {
        let path = rel_path_if_supported!(self, key_spec, Ok(None), key_type);

        let key = match checked_op!(read, path) {
            Err(fs_mistrust::Error::NotFound(_)) => return Ok(None),
            res => res
                .map_err(|err| FilesystemError::FsMistrust {
                    action: FilesystemAction::Read,
                    path: path.rel_path_unchecked().into(),
                    err: err.into(),
                })
                .map_err(CTorKeystoreError::Filesystem)?,
        };

        let parse_err = |err: MalformedServiceKeyError| CTorKeystoreError::MalformedKey {
            path: path.rel_path_unchecked().into(),
            err: err.into(),
        };

        let parsed_key: ErasedKey = match key_type {
            KeyType::Ed25519ExpandedKeypair => parse_ed25519_keypair(&key)
                .map_err(parse_err)
                .map(Box::new)?,
            KeyType::Ed25519PublicKey => parse_ed25519_public(&key)
                .map_err(parse_err)
                .map(Box::new)?,
            _ => {
                return Err(
                    internal!("key type was not validated by rel_path_if_supported?!").into(),
                );
            }
        };

        Ok(Some(parsed_key))
    }

    fn insert(
        &self,
        _key: &dyn EncodableKey,
        _key_spec: &dyn KeySpecifier,
        _key_type: &KeyType,
    ) -> Result<()> {
        Err(CTorKeystoreError::NotSupported { action: "insert" }.into())
    }

    fn remove(&self, _key_spec: &dyn KeySpecifier, _key_type: &KeyType) -> Result<Option<()>> {
        Err(CTorKeystoreError::NotSupported { action: "remove" }.into())
    }

    fn list(&self) -> Result<Vec<(KeyPath, KeyType)>> {
        use crate::CTorServicePath::*;
        use itertools::Itertools;

        // This keystore can contain at most 2 keys (the public and private
        // keys of the service)
        let all_keys = [
            (
                CTorPath::Service {
                    nickname: self.nickname.clone(),
                    path: PublicKey,
                },
                KeyType::Ed25519PublicKey,
            ),
            (
                CTorPath::Service {
                    nickname: self.nickname.clone(),
                    path: PrivateKey,
                },
                KeyType::Ed25519ExpandedKeypair,
            ),
        ];

        all_keys
            .into_iter()
            .map(|(path, key_type)| {
                self.contains(&path, &key_type)
                    .map(|res: bool| (path, key_type, res))
            })
            .filter_map_ok(|(path, key_type, res)| res.then_some((path.into(), key_type)))
            .collect()
    }
}

/// Helper for parsing C Tor's ed25519 key format.
macro_rules! parse_ed25519 {
    ($key:expr, $parse_fn:expr, $tag:expr, $key_len:expr) => {{
        let expected_len = $tag.len() + $key_len;

        if $key.len() != expected_len {
            return Err(MalformedServiceKeyError::InvalidKeyLen {
                len: $key.len(),
                expected_len,
            });
        }

        let (tag, key) = $key.split_at($tag.len());

        if tag != $tag {
            return Err(MalformedServiceKeyError::InvalidTag {
                tag: tag.to_vec(),
                expected_tag: $tag.into(),
            });
        }

        ($parse_fn)(key)
    }};
}

/// Helper for parsing C Tor's ed25519 public key format.
fn parse_ed25519_public(key: &[u8]) -> StdResult<ed25519::PublicKey, MalformedServiceKeyError> {
    /// The tag C Tor ed25519 public keys are expected to begin with.
    const PUBKEY_TAG: &[u8] = b"== ed25519v1-public: type0 ==\0\0\0";
    /// The size of an ed25519 public key.
    const PUBKEY_LEN: usize = 32;

    parse_ed25519!(
        key,
        |key| ed25519::PublicKey::try_from(key)
            .map_err(|e| MalformedServiceKeyError::from(Arc::new(e))),
        PUBKEY_TAG,
        PUBKEY_LEN
    )
}

/// Helper for parsing C Tor's ed25519 keypair format.
fn parse_ed25519_keypair(
    key: &[u8],
) -> StdResult<ed25519::ExpandedKeypair, MalformedServiceKeyError> {
    /// The tag C Tor ed25519 keypairs are expected to begin with.
    const KEYPAIR_TAG: &[u8] = b"== ed25519v1-secret: type0 ==\0\0\0";
    /// The size of an ed25519 keypair.
    const KEYPAIR_LEN: usize = 64;

    parse_ed25519!(
        key,
        |key: &[u8]| {
            let key: [u8; 64] = key
                .try_into()
                .map_err(|_| internal!("bad length on expanded ed25519 secret key "))?;
            ed25519::ExpandedKeypair::from_secret_key_bytes(key)
                .ok_or(MalformedServiceKeyError::Ed25519Keypair)
        },
        KEYPAIR_TAG,
        KEYPAIR_LEN
    )
}

#[cfg(test)]
mod tests {
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

    // TODO
}
