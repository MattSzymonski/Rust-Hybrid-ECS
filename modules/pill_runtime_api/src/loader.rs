//! Host-side helpers for loading and validating a runtime dynamic library.
//!
//! # Responsibilities
//!
//! - Open a runtime dynamic library and resolve its exported API accessor.
//! - Validate the returned table before the host makes any call through it.
//! - Keep the module mapped for exactly as long as the table is reachable.
//!
//! # Design
//!
//! The loaded module is kept behind [`LoadedRuntimeModule`], which owns both
//! the `libloading` handle and the raw table pointer it produced. Nothing here
//! decides *when* a module may be unloaded: the host's reload transaction owns
//! that policy and keeps retired generations mapped in a graveyard, because
//! captured state envelopes and drop glue can still point into their code.
//!
//! This module is gated behind the `loader` feature so the runtime dylib never
//! links `libloading` through the contract crate.

// Standard library
use std::fmt;
use std::path::{Path, PathBuf};

// External crates
use libloading::{Library, Symbol};

// Current crate
use crate::vtable::{
    validate_api_table, ApiValidationError, PillRuntimeApiV1, PILL_RUNTIME_API_SYMBOL,
};

// =============================================================================
// Types
// =============================================================================

/// Why a runtime dynamic library could not be opened and validated.
#[derive(Debug)]
pub enum RuntimeModuleError {
    /// The module could not be mapped into the process.
    LoadFailed {
        /// Path that was opened.
        path: PathBuf,
        /// Underlying loader failure.
        source: libloading::Error,
    },
    /// The module does not export the API accessor symbol.
    MissingExport {
        /// Path that was opened.
        path: PathBuf,
        /// Underlying symbol-resolution failure.
        source: libloading::Error,
    },
    /// The exported table failed a contract guard.
    InvalidTable {
        /// Path that was opened.
        path: PathBuf,
        /// The guard that rejected the table.
        source: ApiValidationError,
    },
}

impl fmt::Display for RuntimeModuleError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LoadFailed { path, source } => {
                write!(formatter, "failed to load {}: {source}", path.display())
            }
            Self::MissingExport { path, source } => write!(
                formatter,
                "{} does not export the runtime API accessor: {source}",
                path.display()
            ),
            Self::InvalidTable { path, source } => {
                write!(formatter, "{} was rejected: {source}", path.display())
            }
        }
    }
}

impl std::error::Error for RuntimeModuleError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::LoadFailed { source, .. } | Self::MissingExport { source, .. } => Some(source),
            Self::InvalidTable { source, .. } => Some(source),
        }
    }
}

/// One mapped runtime dynamic library and its validated API table.
///
/// Dropping this unmaps the module, which is only safe once nothing can still
/// reach its code: no live handle, no captured state envelope it allocated,
/// and no drop glue reachable from the engine.
pub struct LoadedRuntimeModule {
    /// Keeps the module mapped for as long as `table` is dereferenced.
    library: Library,
    /// Pointer to the `'static` table the module exports.
    table: *const PillRuntimeApiV1,
    /// Path the module was opened from, retained for diagnostics.
    path: PathBuf,
}

impl LoadedRuntimeModule {
    /// Open a runtime dynamic library and validate its exported table.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeModuleError`] when the module cannot be mapped, does
    /// not export [`PILL_RUNTIME_API_SYMBOL`], or exports a table that fails a
    /// contract guard.
    ///
    /// # Safety
    ///
    /// `path` must name a runtime dynamic library built from this workspace.
    /// Mapping a module runs its initializers, and the resolved accessor is
    /// called immediately, so an arbitrary or corrupted file is not merely
    /// rejected - it can execute code.
    pub unsafe fn load(path: &Path) -> Result<Self, RuntimeModuleError> {
        // Step 1: Map the module. This runs its initializers, which is why the
        // caller must vouch for the file's provenance.
        // SAFETY: The caller guarantees `path` names a runtime dynamic library
        // produced by this workspace's build.
        let library =
            unsafe { Library::new(path) }.map_err(|source| RuntimeModuleError::LoadFailed {
                path: path.to_path_buf(),
                source,
            })?;

        // Step 2: Resolve the single exported accessor.
        // SAFETY: `PILL_RUNTIME_API_SYMBOL` is declared by the contract with
        // exactly this signature, and the borrow ends before `library` moves.
        let accessor: Symbol<extern "C" fn() -> *const PillRuntimeApiV1> = unsafe {
            library.get(PILL_RUNTIME_API_SYMBOL)
        }
        .map_err(|source| RuntimeModuleError::MissingExport {
            path: path.to_path_buf(),
            source,
        })?;
        let table = accessor();

        // Step 3: Refuse the module unless its table matches this contract.
        // SAFETY: `table` came from the accessor the contract defines; it is
        // either null, which validation rejects, or the module's `'static`
        // table, which stays valid while `library` is mapped.
        unsafe { validate_api_table(table) }.map_err(|source| {
            RuntimeModuleError::InvalidTable {
                path: path.to_path_buf(),
                source,
            }
        })?;

        Ok(Self {
            library,
            table,
            path: path.to_path_buf(),
        })
    }

    /// Borrow the validated API table.
    pub fn table(&self) -> &PillRuntimeApiV1 {
        // SAFETY: `load` validated that `table` is non-null and points at the
        // module's `'static` table, and `library` keeps that module mapped for
        // the lifetime of `self`.
        unsafe { &*self.table }
    }

    /// Path this module was opened from.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Release the module handle without unmapping anything else.
    ///
    /// Used by the host's graveyard when a retired generation finally ages out.
    pub fn into_library(self) -> Library {
        self.library
    }
}

impl fmt::Debug for LoadedRuntimeModule {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LoadedRuntimeModule")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}
