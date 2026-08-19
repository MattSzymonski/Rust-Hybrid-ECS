//! World-state capture and restore across an engine runtime swap.
//!
//! # Responsibilities
//!
//! - Serialize entities, persistable components, their exact handles, and
//!   persistable resources into one self-describing document.
//! - Hand that document to the host as a [`CapturedWorldState`] envelope, and
//!   release it again on request.
//! - Rebuild a world from an envelope written by a *different* engine binary,
//!   reporting what was preserved, adapted, or dropped.
//!
//! # Design
//!
//! An engine reload replaces the whole engine binary, so the two sides of a
//! capture/restore pair are independently compiled. Nothing may be carried
//! across as a Rust value: `TypeId`s differ, struct layouts differ, and vtable
//! addresses point into a retired module. The envelope is therefore a JSON
//! document matched by *type name*, which is exactly the format and matching
//! rule the existing project-reload snapshot already uses - engine reload
//! reuses that machinery instead of introducing a second state format.
//!
//! Component payloads are embedded as their original JSON text rather than
//! re-parsed into a value tree, so a capture/restore round trip is byte-exact
//! and no floating-point value can drift through an intermediate
//! representation.
//!
//! ## Ownership
//!
//! [`capture`] allocates an [`OwnedCapturedState`] whose first field is the
//! `#[repr(C)]` header the host sees. The returned pointer therefore addresses
//! both, and [`release`] recovers the full allocation from it. Because the
//! allocation belongs to this module's allocator, the host must release an
//! envelope through the same table that produced it; [`release`] refuses
//! anything whose guard fields do not match rather than freeing memory it does
//! not own.

// Standard library
use std::ffi::{c_char, CString};
use std::time::{SystemTime, UNIX_EPOCH};

// External crates
use pill_core::{info, warn};
use pill_engine::{ComponentSnapshot, Engine, SnapshotEntityId};
use pill_runtime_api::{CapturedWorldState, PILL_RUNTIME_STATE_FORMAT_VERSION};
use serde::{Deserialize, Serialize};

// =============================================================================
// Constants
// =============================================================================

/// Guard value stored beside the ABI header of every envelope this module
/// allocates.
///
/// Releasing an envelope allocated by a different module would free memory
/// with the wrong allocator, so the guard turns a host-side ownership mistake
/// into a refused release rather than heap corruption.
const OWNED_STATE_MAGIC: u64 = 0x5049_4C4C_5354_4131;

// =============================================================================
// Serialized document
// =============================================================================

/// One persistable component captured from one entity.
#[derive(Debug, Serialize, Deserialize)]
struct ComponentRecord {
    /// Fully-qualified Rust type name the component was registered under.
    type_name: String,
    /// Original JSON text produced by the component's serializer.
    payload: String,
}

/// One entity and the persistable components it carried.
#[derive(Debug, Serialize, Deserialize)]
struct EntityRecord {
    /// Slot identifier of the captured handle.
    id: u64,
    /// Generation counter of the captured handle.
    generation: u32,
    /// Every persistable component on the entity.
    components: Vec<ComponentRecord>,
}

/// One persistable resource captured from the world.
#[derive(Debug, Serialize, Deserialize)]
struct ResourceRecord {
    /// Fully-qualified Rust type name the resource was registered under.
    type_name: String,
    /// Original JSON text produced by the resource's serializer.
    payload: String,
}

/// Schema fingerprint of one persistable component type at capture time.
#[derive(Debug, Serialize, Deserialize)]
struct ManifestRecord {
    /// Fully-qualified Rust type name.
    type_name: String,
    /// Schema hash derived from the type's default JSON shape and size.
    schema_hash: u64,
}

/// The complete captured world, as written into an envelope payload.
#[derive(Debug, Serialize, Deserialize)]
struct WorldStateDocument {
    /// Revision of this document layout.
    format_version: u32,
    /// Persistable component schemas the capturing engine had registered.
    manifest: Vec<ManifestRecord>,
    /// Every entity that carried at least one persistable component.
    entities: Vec<EntityRecord>,
    /// Every registered persistable resource that had an instance.
    resources: Vec<ResourceRecord>,
}

// =============================================================================
// OwnedCapturedState
// =============================================================================

/// The allocation behind one [`CapturedWorldState`] pointer.
///
/// `#[repr(C)]` with `header` first means the pointer the host receives
/// addresses the header and the allocation interchangeably, so the envelope
/// needs no separate registry to be released.
#[repr(C)]
struct OwnedCapturedState {
    /// The `#[repr(C)]` view the host reads; must stay the first field.
    header: CapturedWorldState,
    /// Guard proving this allocation came from this module.
    magic: u64,
    /// Serialized document the header's `payload` pointer addresses.
    payload: Vec<u8>,
    /// Human summary the header's `summary_utf8` pointer addresses.
    summary: CString,
}

// =============================================================================
// Types
// =============================================================================

/// Outcome of restoring a captured world into a fresh engine.
#[derive(Debug, Default)]
pub(crate) struct RestoreReport {
    /// Entities rebuilt from the envelope.
    pub(crate) restored_entity_count: usize,
    /// Persistable resources decoded and re-inserted.
    pub(crate) restored_resource_count: usize,
    /// Component types whose schema hash changed since capture.
    pub(crate) migrated_type_names: Vec<String>,
    /// Component types the current project no longer registers.
    pub(crate) dropped_type_names: Vec<String>,
    /// Component types registered now that the capture did not know about.
    pub(crate) added_type_names: Vec<String>,
}

// =============================================================================
// Free Functions
// =============================================================================

/// Serialize the engine's world into a new host-owned envelope.
///
/// # Errors
///
/// Returns a message when the document cannot be encoded, which leaves the
/// caller free to keep running the current generation instead of swapping.
pub(crate) fn capture(engine: &Engine) -> Result<*mut CapturedWorldState, String> {
    // Step 1: Reuse the engine's existing component snapshot, which already
    // walks every archetype and applies each registered serializer.
    let world = engine.world();
    let snapshot = world.snapshot_components();
    let manifest: Vec<ManifestRecord> = world
        .persist_type_manifest()
        .into_iter()
        .map(|entry| ManifestRecord {
            type_name: entry.type_name,
            schema_hash: entry.schema_hash,
        })
        .collect();
    let resources = world.snapshot_resources();

    // Step 2: Fold the snapshot, its captured handles, and the resources into
    // one document. Payload bytes come from `serde_json`, so they are always
    // valid UTF-8; a component that somehow is not is dropped with a warning
    // rather than corrupting the whole envelope.
    let entities = build_entity_records(&snapshot);
    let resources: Vec<ResourceRecord> = resources
        .into_iter()
        .filter_map(|(type_name, bytes)| match String::from_utf8(bytes) {
            Ok(payload) => Some(ResourceRecord { type_name, payload }),
            Err(_) => {
                warn!(
                    target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                    type_name = %type_name,
                    "captured resource payload is not valid UTF-8; dropping it from the envelope"
                );
                None
            }
        })
        .collect();

    let document = WorldStateDocument {
        format_version: PILL_RUNTIME_STATE_FORMAT_VERSION,
        manifest,
        entities,
        resources,
    };

    let summary = format!(
        "{} entities, {} persistable component types, {} resources",
        document.entities.len(),
        document.manifest.len(),
        document.resources.len(),
    );
    let payload = serde_json::to_vec(&document)
        .map_err(|error| format!("failed to encode the captured world: {error}"))?;

    // Step 3: Publish the document through a `#[repr(C)]` header whose
    // pointers address this same allocation.
    Ok(into_envelope(payload, summary))
}

/// Build one entity record per snapshot entry, pairing it with its handle.
///
/// A snapshot whose identity list does not line up with its entries is treated
/// as carrying no identities at all, matching the engine's own restore rule:
/// pairing components with the wrong handle would be far worse than allocating
/// fresh ones.
fn build_entity_records(snapshot: &ComponentSnapshot) -> Vec<EntityRecord> {
    let identities_are_usable = snapshot.entity_ids.len() == snapshot.entries.len();
    if !identities_are_usable && !snapshot.entity_ids.is_empty() {
        warn!(
            target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
            captured_ids = snapshot.entity_ids.len(),
            entries = snapshot.entries.len(),
            "captured entity identities do not match the snapshot entries; capturing without identities"
        );
    }

    snapshot
        .entries
        .iter()
        .enumerate()
        .map(|(entry_index, components)| {
            let identity = if identities_are_usable {
                snapshot.entity_ids[entry_index]
            } else {
                // A zeroed identity is never written into the envelope: the
                // whole list is discarded above when it cannot be paired, so
                // restore falls back to fresh handles.
                SnapshotEntityId {
                    id: 0,
                    generation: 0,
                }
            };
            EntityRecord {
                id: identity.id,
                generation: identity.generation,
                components: components
                    .iter()
                    .filter_map(|(type_name, bytes)| {
                        match std::str::from_utf8(bytes) {
                            Ok(payload) => Some(ComponentRecord {
                                type_name: type_name.clone(),
                                payload: payload.to_string(),
                            }),
                            Err(_) => {
                                warn!(
                                    target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                                    type_name = %type_name,
                                    "captured component payload is not valid UTF-8; dropping it from the envelope"
                                );
                                None
                            }
                        }
                    })
                    .collect(),
            }
        })
        .filter(|record| !record.components.is_empty())
        .collect()
}

/// Wrap a serialized document and its summary in a host-facing envelope.
fn into_envelope(payload: Vec<u8>, summary: String) -> *mut CapturedWorldState {
    // Interior NULs cannot occur in a formatted count summary, but fall back
    // rather than panic so a future summary change stays safe.
    let summary = CString::new(summary).unwrap_or_else(|_| CString::new("captured world").unwrap());
    let captured_at_nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos() as u64)
        .unwrap_or_default();

    let mut owned = Box::new(OwnedCapturedState {
        header: CapturedWorldState {
            struct_size: std::mem::size_of::<CapturedWorldState>() as u32,
            format_version: PILL_RUNTIME_STATE_FORMAT_VERSION,
            captured_at_nanos,
            payload: std::ptr::null(),
            payload_len: 0,
            summary_utf8: std::ptr::null(),
        },
        magic: OWNED_STATE_MAGIC,
        payload,
        summary,
    });

    // The header's pointers must address the boxed allocation's own buffers,
    // so they are filled after the box exists and never before.
    owned.header.payload = owned.payload.as_ptr();
    owned.header.payload_len = owned.payload.len() as u64;
    owned.header.summary_utf8 = owned.summary.as_ptr();

    Box::into_raw(owned) as *mut CapturedWorldState
}

/// Release an envelope this module allocated.
///
/// # Safety
///
/// `state` must be a pointer returned by [`capture`] *in this module* and not
/// yet released. Passing an envelope from another runtime generation would
/// free memory with the wrong allocator; the guard fields are checked first so
/// such a call leaks rather than corrupts.
pub(crate) unsafe fn release(state: *mut CapturedWorldState) {
    if state.is_null() {
        return;
    }

    // SAFETY: The caller guarantees `state` came from `capture` in this
    // module, where `OwnedCapturedState` is `#[repr(C)]` with `header` first,
    // so the pointer addresses the whole allocation.
    let owned = state as *mut OwnedCapturedState;
    // SAFETY: Same as above; the guard fields are read before the allocation
    // is reclaimed so a foreign pointer is rejected instead of freed.
    let (magic, struct_size) = unsafe { ((*owned).magic, (*owned).header.struct_size) };
    if magic != OWNED_STATE_MAGIC
        || struct_size as usize != std::mem::size_of::<CapturedWorldState>()
    {
        warn!(
            target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
            "refusing to release a captured world envelope this runtime did not allocate"
        );
        return;
    }

    // SAFETY: The guards above confirm this allocation came from `capture` in
    // this module, and the contract guarantees it is released exactly once.
    drop(unsafe { Box::from_raw(owned) });
}

/// Rebuild the engine's world from a captured envelope.
///
/// The replacement engine has already loaded the project and run
/// `project_init`, so every persistable component and resource type is
/// registered before anything is decoded.
///
/// # Errors
///
/// Returns a message when the envelope has a foreign layout, an unsupported
/// format version, or a payload that cannot be parsed. The caller keeps the
/// freshly initialized world in that case rather than a partially restored one.
///
/// # Safety
///
/// `state` must point to a live [`CapturedWorldState`] whose payload stays
/// valid for the duration of the call.
pub(crate) unsafe fn restore(
    engine: &mut Engine,
    state: *const CapturedWorldState,
) -> Result<RestoreReport, String> {
    if state.is_null() {
        return Err(String::from("restore_world_state received a null envelope"));
    }

    // SAFETY: The caller guarantees a non-null `state` points to a live
    // envelope for the duration of this call.
    let state = unsafe { &*state };
    if !state.has_expected_layout() {
        return Err(format!(
            "captured world envelope layout mismatch: runtime expects {} bytes, envelope reports {} bytes",
            std::mem::size_of::<CapturedWorldState>(),
            state.struct_size,
        ));
    }
    if state.format_version != PILL_RUNTIME_STATE_FORMAT_VERSION {
        return Err(format!(
            "captured world format version mismatch: runtime expects {}, envelope reports {}",
            PILL_RUNTIME_STATE_FORMAT_VERSION, state.format_version,
        ));
    }

    // SAFETY: The caller guarantees the payload stays valid for this call, and
    // the envelope was written by a matching contract build.
    let payload = unsafe { state.payload_bytes() };
    let document: WorldStateDocument = serde_json::from_slice(payload)
        .map_err(|error| format!("failed to decode the captured world: {error}"))?;
    if document.format_version != PILL_RUNTIME_STATE_FORMAT_VERSION {
        return Err(format!(
            "captured world document version mismatch: runtime expects {}, document reports {}",
            PILL_RUNTIME_STATE_FORMAT_VERSION, document.format_version,
        ));
    }

    Ok(apply_document(engine, document))
}

/// Compare schemas, rebuild entities, and re-insert resources.
///
/// The manifest comparison decides whether this is a fast-path restore - every
/// captured schema hash still matches, so payloads decode field-for-field - or
/// a migrating restore, where at least one type changed shape and its payload
/// is adapted by merging it over the new type's default JSON. That adaptation
/// happens inside the engine's own deserializers, which is why the migrating
/// path needs no separate column rewrite: the captured world is rebuilt from
/// JSON rather than from the retired generation's archetype memory.
fn apply_document(engine: &mut Engine, document: WorldStateDocument) -> RestoreReport {
    let mut report = RestoreReport::default();

    // Step 1: Diff the captured schemas against the ones the replacement
    // engine and project just registered.
    let current_manifest = engine.world().persist_type_manifest();
    let current_hashes: std::collections::HashMap<&str, u64> = current_manifest
        .iter()
        .map(|entry| (entry.type_name.as_str(), entry.schema_hash))
        .collect();

    for captured in &document.manifest {
        match current_hashes.get(captured.type_name.as_str()) {
            Some(&current_hash) if current_hash == captured.schema_hash => {}
            Some(&current_hash) => {
                info!(
                    target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                    type_name = %captured.type_name,
                    captured_schema = captured.schema_hash,
                    current_schema = current_hash,
                    "persistable component schema changed across the engine swap; adapting on restore"
                );
                report.migrated_type_names.push(captured.type_name.clone());
            }
            None => {
                warn!(
                    target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                    type_name = %captured.type_name,
                    "persistable component type is no longer registered; its captured data is dropped"
                );
                report.dropped_type_names.push(captured.type_name.clone());
            }
        }
    }

    let captured_names: std::collections::HashSet<&str> = document
        .manifest
        .iter()
        .map(|entry| entry.type_name.as_str())
        .collect();
    for entry in &current_manifest {
        if !captured_names.contains(entry.type_name.as_str()) {
            report.added_type_names.push(entry.type_name.clone());
        }
    }

    if report.migrated_type_names.is_empty() && report.dropped_type_names.is_empty() {
        info!(
            target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
            types = document.manifest.len(),
            "captured schema unchanged for all persistable component types - fast path restore"
        );
    }

    // Step 2: Rebuild the component snapshot the engine restore path consumes,
    // including the captured handles so entity identities survive the swap.
    //
    // A capture with no entities is skipped rather than applied. Restoring
    // replaces the world, so applying an empty capture would destroy whatever
    // the replacement generation's `project_init` and startup methods just
    // built. That is exactly the situation a project whose components are not
    // persistable is in - a managed C# project registers dynamic components,
    // which never enter a snapshot - and wiping its world would contradict the
    // rule that non-persistable state is recreated by initialization rather
    // than lost.
    if document.entities.is_empty() {
        report.restored_entity_count = engine.world().entity_count();
        info!(
            target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
            entities = report.restored_entity_count,
            "the captured world held no persistable entities; keeping the freshly initialized world"
        );
    } else {
        let mut snapshot = ComponentSnapshot {
            entries: Vec::with_capacity(document.entities.len()),
            entity_ids: Vec::with_capacity(document.entities.len()),
        };
        for entity in document.entities {
            snapshot.entity_ids.push(SnapshotEntityId {
                id: entity.id,
                generation: entity.generation,
            });
            snapshot.entries.push(
                entity
                    .components
                    .into_iter()
                    .map(|component| (component.type_name, component.payload.into_bytes()))
                    .collect(),
            );
        }
        engine.world_mut().restore_from_snapshot(&snapshot);
        report.restored_entity_count = engine.world().entity_count();
    }

    // Step 3: Re-insert captured resources over the instances `project_init`
    // created, so simulation state such as elapsed time survives the swap.
    let resources: Vec<(String, Vec<u8>)> = document
        .resources
        .into_iter()
        .map(|resource| (resource.type_name, resource.payload.into_bytes()))
        .collect();
    let resource_report = engine.world_mut().restore_resources(&resources);
    report.restored_resource_count = resource_report.restored_count;

    report
}

/// Payload size of an envelope, in bytes.
///
/// # Safety
///
/// `state` must either be null or point to a live [`CapturedWorldState`].
pub(crate) unsafe fn byte_len(state: *const CapturedWorldState) -> u64 {
    if state.is_null() {
        return 0;
    }
    // SAFETY: The caller guarantees a non-null `state` points to a live
    // envelope for the duration of this call.
    unsafe { (*state).payload_len }
}

/// Borrow an envelope's human summary.
///
/// # Safety
///
/// `state` must either be null or point to a live [`CapturedWorldState`] whose
/// summary buffer stays valid for the returned pointer's use.
pub(crate) unsafe fn describe(state: *const CapturedWorldState) -> *const c_char {
    if state.is_null() {
        return std::ptr::null();
    }
    // SAFETY: The caller guarantees a non-null `state` points to a live
    // envelope for the duration of this call.
    unsafe { (*state).summary_utf8 }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// An envelope's header points at its own payload and summary buffers.
    #[test]
    fn envelope_header_addresses_its_own_buffers() {
        let raw = into_envelope(
            b"{\"format_version\":1}".to_vec(),
            String::from("2 entities"),
        );
        // SAFETY: `into_envelope` just produced this envelope in this module.
        let header = unsafe { &*raw };

        assert!(header.has_expected_layout());
        assert_eq!(header.format_version, PILL_RUNTIME_STATE_FORMAT_VERSION);
        // SAFETY: The header addresses the allocation's own payload buffer.
        assert_eq!(unsafe { header.payload_bytes() }, b"{\"format_version\":1}");
        // SAFETY: The summary is a live `CString` inside the same allocation.
        let summary = unsafe { std::ffi::CStr::from_ptr(header.summary_utf8) };
        assert_eq!(summary.to_str().unwrap(), "2 entities");

        // SAFETY: The envelope came from this module and is released once.
        unsafe { release(raw) };
    }

    /// Diagnostics read an envelope without taking ownership of it.
    #[test]
    fn envelope_diagnostics_report_payload_and_summary() {
        let raw = into_envelope(vec![b'{', b'}'], String::from("empty world"));
        // SAFETY: `raw` is a live envelope produced by this module.
        assert_eq!(unsafe { byte_len(raw) }, 2);
        // SAFETY: Same as above; the summary pointer stays valid until release.
        let summary = unsafe { std::ffi::CStr::from_ptr(describe(raw)) };
        assert_eq!(summary.to_str().unwrap(), "empty world");
        // SAFETY: The envelope came from this module and is released once.
        unsafe { release(raw) };
    }

    /// Null envelopes are inert for every diagnostic and for release.
    #[test]
    fn null_envelopes_are_inert() {
        // SAFETY: A null pointer is explicitly permitted by each contract.
        unsafe {
            assert_eq!(byte_len(std::ptr::null()), 0);
            assert!(describe(std::ptr::null()).is_null());
            release(std::ptr::null_mut());
        }
    }

    /// A document round-trips through the payload encoding unchanged.
    #[test]
    fn world_document_round_trips_through_json() {
        let document = WorldStateDocument {
            format_version: PILL_RUNTIME_STATE_FORMAT_VERSION,
            manifest: vec![ManifestRecord {
                type_name: String::from("project::Position"),
                schema_hash: 42,
            }],
            entities: vec![EntityRecord {
                id: 7,
                generation: 3,
                components: vec![ComponentRecord {
                    type_name: String::from("project::Position"),
                    payload: String::from("{\"x\":1.5,\"y\":-2.25}"),
                }],
            }],
            resources: vec![ResourceRecord {
                type_name: String::from("project::Score"),
                payload: String::from("{\"points\":11}"),
            }],
        };

        let encoded = serde_json::to_vec(&document).unwrap();
        let decoded: WorldStateDocument = serde_json::from_slice(&encoded).unwrap();

        assert_eq!(decoded.entities.len(), 1);
        assert_eq!(decoded.entities[0].id, 7);
        assert_eq!(decoded.entities[0].generation, 3);
        // The payload survives as its exact original text, so no floating
        // point value is re-encoded on the way through the envelope.
        assert_eq!(
            decoded.entities[0].components[0].payload,
            "{\"x\":1.5,\"y\":-2.25}"
        );
        assert_eq!(decoded.resources[0].type_name, "project::Score");
    }

    /// A snapshot whose identity list is truncated captures no identities.
    #[test]
    fn mismatched_identity_lists_are_discarded() {
        let snapshot = ComponentSnapshot {
            entries: vec![
                vec![(String::from("project::Position"), b"{}".to_vec())],
                vec![(String::from("project::Position"), b"{}".to_vec())],
            ],
            entity_ids: vec![SnapshotEntityId {
                id: 4,
                generation: 1,
            }],
        };

        let records = build_entity_records(&snapshot);
        assert_eq!(records.len(), 2);
        assert!(records.iter().all(|record| record.id == 0));
    }

    /// An empty capture keeps the world the replacement generation just built.
    ///
    /// This is the managed-project case: a C# project registers dynamic
    /// components, which never enter a snapshot, so its capture is empty while
    /// its startup methods have already rebuilt the world. Applying the empty
    /// capture would destroy exactly that.
    #[test]
    fn an_empty_capture_never_destroys_a_freshly_initialized_world() {
        use pill_engine::Component;
        use trait_type_map::impl_trait_accessible;

        #[derive(Debug, Clone, Default)]
        struct DynamicMarker {
            value: u32,
        }
        impl Component for DynamicMarker {}
        impl_trait_accessible!(dyn Component; DynamicMarker);

        let mut engine = Engine::new();
        engine.world_mut().register_component::<DynamicMarker>();
        for value in 0..3 {
            engine
                .world_mut()
                .create_entity()
                .with(DynamicMarker { value })
                .build()
                .expect("the fixture entity builds");
        }
        assert_eq!(engine.world().entity_count(), 3);

        let empty = WorldStateDocument {
            format_version: PILL_RUNTIME_STATE_FORMAT_VERSION,
            manifest: Vec::new(),
            entities: Vec::new(),
            resources: Vec::new(),
        };
        let report = apply_document(&mut engine, empty);

        assert_eq!(engine.world().entity_count(), 3);
        assert_eq!(report.restored_entity_count, 3);
    }

    /// A capture holding entities does replace the world it is applied to.
    #[test]
    fn a_non_empty_capture_replaces_the_world() {
        use pill_engine::Component;
        use serde::{Deserialize, Serialize};
        use trait_type_map::impl_trait_accessible;

        #[derive(Debug, Clone, Default, Serialize, Deserialize)]
        struct Persisted {
            value: u32,
        }
        impl Component for Persisted {}
        impl_trait_accessible!(dyn Component; Persisted);

        let mut engine = Engine::new();
        engine
            .world_mut()
            .register_persistable_component::<Persisted>();
        // The replacement generation seeds two entities of its own, exactly as
        // an idempotent `project_init` would.
        for value in 0..2 {
            engine
                .world_mut()
                .create_entity()
                .with(Persisted { value })
                .build()
                .expect("the fixture entity builds");
        }

        let captured = WorldStateDocument {
            format_version: PILL_RUNTIME_STATE_FORMAT_VERSION,
            manifest: Vec::new(),
            entities: vec![EntityRecord {
                id: 41,
                generation: 2,
                components: vec![ComponentRecord {
                    type_name: std::any::type_name::<Persisted>().to_string(),
                    payload: String::from("{\"value\":99}"),
                }],
            }],
            resources: Vec::new(),
        };
        let report = apply_document(&mut engine, captured);

        assert_eq!(report.restored_entity_count, 1);
        assert_eq!(engine.world().entity_count(), 1);
    }

    /// Entities whose components all failed to encode are left out entirely.
    #[test]
    fn entities_without_encodable_components_are_omitted() {
        let snapshot = ComponentSnapshot {
            entries: vec![vec![(String::from("project::Broken"), vec![0xFF, 0xFE])]],
            entity_ids: vec![SnapshotEntityId {
                id: 1,
                generation: 0,
            }],
        };

        assert!(build_entity_records(&snapshot).is_empty());
    }
}
