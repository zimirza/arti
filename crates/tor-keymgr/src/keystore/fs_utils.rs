//! Module providing support for handling paths relative to a [`CheckedDir`].
//!
//! The underlying relative path of a [`RelKeyPath`] should not be manipulated directly.
//! Instead, prefer converting it to an absolute path using
//! [`checked_path`](RelKeyPath::checked_path) where possible.
//! You may also use the `checked_op` macro to call [`CheckedDir`] functions on the path.

use std::path::{Path, PathBuf};
use std::result::Result as StdResult;

use fs_mistrust::CheckedDir;
use tor_key_forge::KeyType;

use crate::keystore::arti::err::{ArtiNativeKeystoreError, FilesystemAction};
use crate::{ArtiPathUnavailableError, KeySpecifier, Result};

/// The path of a key, relative to a [`CheckedDir`].
///
/// See the [module-level documentation](self) for a general overview.
#[derive(Debug, Clone)]
pub(super) struct RelKeyPath<'a> {
    /// The directory this path is relative to.
    dir: &'a CheckedDir,
    /// The relative path.
    path: PathBuf,
}

impl<'a> RelKeyPath<'a> {
    /// Create a new [`RelKeyPath`] representing an `ArtiPath`.
    ///
    /// Returns an error if `key_spec` does not have an `ArtiPath`.
    pub(super) fn arti(
        dir: &'a CheckedDir,
        key_spec: &dyn KeySpecifier,
        key_type: &KeyType,
    ) -> StdResult<Self, ArtiPathUnavailableError> {
        let arti_path: String = key_spec.arti_path()?.into();
        let mut path = PathBuf::from(arti_path);
        path.set_extension(key_type.arti_extension());
        Ok(Self { dir, path })
    }

    /// Create a new [`RelKeyPath`] from a `CheckedDir` and a relative path.
    pub(super) fn from_parts(dir: &'a CheckedDir, path: PathBuf) -> Self {
        Self { dir, path }
    }

    /// Return the checked absolute path.
    pub(super) fn checked_path(&self) -> Result<PathBuf> {
        let abs_path =
            self.dir
                .join(&self.path)
                .map_err(|err| ArtiNativeKeystoreError::FsMistrust {
                    action: FilesystemAction::Read,
                    path: self.path.clone(),
                    err: err.into(),
                })?;

        Ok(abs_path)
    }

    /// Return this as an unchecked relative path.
    pub(super) fn rel_path_unchecked(&self) -> &Path {
        &self.path
    }

    /// Return the [`CheckedDir`] of this `RelKeyPath`.
    pub(super) fn checked_dir(&self) -> &CheckedDir {
        self.dir
    }
}

pub(crate) use internal::checked_op;

/// Private module for reexporting the `checked_op` macro.
mod internal {
    /// Run operation `op` on a [`RelKeyPath`](super::RelKeyPath).
    ///
    /// `op` is an identifier that represents a [`CheckedDir`](fs_mistrust::CheckedDir) function.
    macro_rules! checked_op {
        ($op:ident, $relpath:expr $(, $arg:expr)* ) => {{
            $relpath.checked_dir().$op($relpath.rel_path_unchecked(),  $($arg,)* )
        }}
    }

    pub(crate) use checked_op;
}
