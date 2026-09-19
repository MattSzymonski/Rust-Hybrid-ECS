//! The two-call protocol every managed payload crosses the boundary through.
//!
//! # Responsibilities
//!
//! - Runs the ask-the-length, allocate, ask-for-a-copy handshake once, for
//!   every payload kind: manifests, system names, failure messages, compiler
//!   diagnostics.
//! - Bounds the length managed code reports and reserves fallibly, so neither
//!   a buggy nor a hostile assembly can drive the host into an abort.
//!
//! # Design
//!
//! Managed code cannot hand the host an owned buffer, so every payload crosses
//! in two calls: one reports how many bytes there are, the host allocates, and
//! the second fills what the host allocated. That protocol is a property of the
//! boundary rather than of any one payload, so it lives here once and each
//! caller supplies only what differs - the pair of exported functions and the
//! upper bound that payload kind accepts.
//!
//! Two things the protocol gets right are easy to get wrong when written out by
//! hand, which is why they are here and not at the call sites. The length is
//! checked against a bound **before** anything is allocated, so a corrupt
//! length is a reported error rather than a multi-gigabyte request. And the
//! reservation is fallible: `Vec::try_reserve_exact` returns an error where
//! `vec![0; n]` aborts the process, and aborting is not an acceptable answer to
//! a number that came from outside.
//!
//! Failure is returned rather than reported, because the callers disagree about
//! what a failure means - one fails a reload with a typed error, one logs and
//! keeps the manifest it already applied, one falls back to a neutral string -
//! and that disagreement is legitimate.

// External crates
use pill_core::error::CSharpError;

// =============================================================================
// Types
// =============================================================================

/// Why one managed payload could not be fetched.
///
/// Carries the numbers the caller needs to report, so a caller that logs and a
/// caller that returns a typed error both have what they need without asking
/// the managed side again.
#[cfg_attr(not(feature = "hot_reload"), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ManagedBufferError {
    /// The managed side reported a length outside `1..=limit`.
    LengthOutOfRange {
        /// The length it reported.
        length: u32,
        /// The largest length this payload kind accepts.
        limit: u32,
    },
    /// The host could not reserve a buffer that long.
    AllocationFailed {
        /// The length the reservation was for.
        length: u32,
    },
    /// The managed side declined to fill the buffer the host offered.
    CopyFailed,
}

// =============================================================================
// Free Functions
// =============================================================================

/// Fetch one managed payload through the two-call protocol.
///
/// `length` and `copy` are the pair of exported functions for this payload
/// kind, already bound to whatever index or receiver they need; `limit` is the
/// largest length this kind accepts. The returned buffer is exactly `length`
/// bytes, all of them written by managed code.
///
/// Closures rather than function pointers, because the exports are
/// `extern "system"` and that ABI does not implement the `Fn` traits - and
/// because several payload kinds are addressed by an index the caller has to
/// close over anyway.
///
/// # Errors
///
/// Returns [`ManagedBufferError::LengthOutOfRange`] when the reported length is
/// zero or above `limit`, [`ManagedBufferError::AllocationFailed`] when a
/// buffer that long cannot be reserved, and
/// [`ManagedBufferError::CopyFailed`] when managed code declines to fill it.
/// Nothing is allocated in the first case, and the partially filled buffer is
/// discarded in the last.
#[cfg_attr(not(feature = "hot_reload"), allow(dead_code))]
pub(super) fn fetch_managed_buffer(
    length: impl FnOnce() -> u32,
    copy: impl FnOnce(*mut u8, u32) -> u8,
    limit: u32,
) -> Result<Vec<u8>, ManagedBufferError> {
    // Step 1: Bound the length before anything is allocated from it. Zero is
    // rejected with the oversized values: a payload the managed side says is
    // empty is the absence of a payload, not a payload of no bytes, and every
    // caller that can meet one answers it differently from a failed fetch.
    let length = length();
    if length == 0 || length > limit {
        return Err(ManagedBufferError::LengthOutOfRange { length, limit });
    }

    // Step 2: Reserve explicitly so an allocation failure surfaces as a regular
    // error instead of aborting the host process.
    let mut buffer = Vec::new();
    buffer
        .try_reserve_exact(length as usize)
        .map_err(|_| ManagedBufferError::AllocationFailed { length })?;
    buffer.resize(length as usize, 0);

    // Step 3: Hand the buffer over. The managed contract rejects any caller
    // buffer smaller than the payload, so a successful copy guarantees a
    // complete one; a refusal leaves the zeroed buffer to be dropped.
    if copy(buffer.as_mut_ptr(), length) == 0 {
        return Err(ManagedBufferError::CopyFailed);
    }
    Ok(buffer)
}

/// Report a failed manifest fetch in the vocabulary the reload path uses.
///
/// The startup and AOT paths both fail the whole load on any of these, so they
/// share one mapping rather than each inventing its own.
#[cfg_attr(not(feature = "hot_reload"), allow(dead_code))]
pub(super) fn manifest_fetch_error(error: ManagedBufferError) -> CSharpError {
    match error {
        ManagedBufferError::LengthOutOfRange { length, limit } => {
            CSharpError::ManifestLengthOutOfRange { length, limit }
        }
        ManagedBufferError::AllocationFailed { .. } => CSharpError::ManifestAllocationFailed,
        ManagedBufferError::CopyFailed => CSharpError::ManifestCopyFailed,
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// A payload of the reported length arrives whole.
    #[test]
    fn a_reported_payload_is_fetched_in_full() {
        let payload = b"manifest";
        let fetched = fetch_managed_buffer(
            || payload.len() as u32,
            |pointer, length| {
                // SAFETY: the helper allocated `length` bytes at `pointer` and
                // reports exactly the length the closure above returned, which
                // is this payload's length.
                unsafe {
                    std::ptr::copy_nonoverlapping(payload.as_ptr(), pointer, length as usize)
                };
                1
            },
            1024,
        )
        .expect("a bounded length and a successful copy");
        assert_eq!(fetched, payload);
    }

    /// A length above the bound is refused before anything is allocated.
    ///
    /// This is the protection the hand-written copies applied at only some of
    /// the call sites: an unbounded length reaches `Vec` as an allocation
    /// request, and an allocation request that large aborts the host.
    #[test]
    fn an_oversized_length_is_refused_without_allocating() {
        let error = fetch_managed_buffer(
            || u32::MAX,
            |_, _| panic!("the copy must not be reached for a refused length"),
            64,
        )
        .expect_err("a length above the bound is refused");
        assert_eq!(
            error,
            ManagedBufferError::LengthOutOfRange {
                length: u32::MAX,
                limit: 64,
            }
        );
    }

    /// A reported length of zero is the absence of a payload, not an empty one.
    #[test]
    fn a_zero_length_is_refused() {
        let error = fetch_managed_buffer(
            || 0,
            |_, _| panic!("the copy must not be reached for a refused length"),
            64,
        )
        .expect_err("zero is outside the accepted range");
        assert_eq!(
            error,
            ManagedBufferError::LengthOutOfRange {
                length: 0,
                limit: 64,
            }
        );
    }

    /// A refused copy is a failure, not a buffer of zeroes.
    #[test]
    fn a_refused_copy_discards_the_buffer() {
        let error = fetch_managed_buffer(|| 8, |_, _| 0, 64)
            .expect_err("the managed side declined to fill the buffer");
        assert_eq!(error, ManagedBufferError::CopyFailed);
    }

    /// Each failure maps to the manifest error the reload path reports.
    #[test]
    fn manifest_failures_map_to_their_typed_errors() {
        assert!(matches!(
            manifest_fetch_error(ManagedBufferError::LengthOutOfRange {
                length: 9,
                limit: 4
            }),
            CSharpError::ManifestLengthOutOfRange {
                length: 9,
                limit: 4
            }
        ));
        assert!(matches!(
            manifest_fetch_error(ManagedBufferError::AllocationFailed { length: 9 }),
            CSharpError::ManifestAllocationFailed
        ));
        assert!(matches!(
            manifest_fetch_error(ManagedBufferError::CopyFailed),
            CSharpError::ManifestCopyFailed
        ));
    }
}
