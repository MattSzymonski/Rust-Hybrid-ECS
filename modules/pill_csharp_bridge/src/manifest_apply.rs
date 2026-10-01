//! The one apply pipeline every manifest kind runs through.
//!
//! # Responsibilities
//!
//! - Resolves each arriving declaration against the live binding table:
//!   settled, added, renamed through an alias, or reshaped.
//! - Applies the resolution in the one order the reload protocol allows, and
//!   journals an undo per applied entry so a later refusal unwinds.
//! - Retires the entries the manifest stopped naming, last, after nothing that
//!   could fail is left to run.
//!
//! # Design
//!
//! A managed manifest carries more than one kind of entry - components today,
//! resources beside them, script components later - and every kind needs the
//! same six stages in the same order. Only four things genuinely differ: how an
//! entry is identified, what registering one does, what reshaping one does, and
//! what retiring one does. Those arrive as a [`ManifestSubject`], a plain
//! struct of function pointers in the style of `ColumnOps` and
//! `StorageFactory`; everything else lives here, once.
//!
//! ## The ordering is the protocol
//!
//! - **Aliases resolve before vanished entries are collected**, so an entry
//!   whose alias reaches a still-live predecessor turns that predecessor from a
//!   disappearance into a rename.
//! - **Every entry is resolved before any of it is applied**, so the refusals
//!   validation cannot foresee leave the world and the table as they were.
//! - **Retirement runs last**, because it drops bytes and cannot be undone, so
//!   nothing that could fail may run after it.
//!
//! ## What rollback can and cannot promise
//!
//! The table is all-or-nothing by construction: the pipeline works on a copy
//! and the caller publishes whatever comes back. Engine state is as
//! all-or-nothing as the subject's actions allow, and that is where the kinds
//! genuinely part company. A component's reshape is a column relayout whose
//! inverse plan puts the rows back, so it journals an undo and a refusal
//! restores everything. A resource's reshape moves its stored bytes into a
//! possibly narrower shape, and reversing that would have to invent the bytes
//! it dropped - so it journals no undo, and the pipeline leaves that entry
//! applied with its new binding in the table. Leaving it is the point: the
//! table must never describe a layout the engine no longer holds, or the view
//! callback's size check is reading one shape through another. Undo what can be
//! undone, leave what cannot, and keep the table describing the engine.

// Standard library
use std::collections::{HashMap, HashSet};

// External crates
use pill_core::error::CSharpError;
use pill_core::telemetry::telemetry_target;
use pill_core::{error, info};
use pill_engine::Engine;

// Current crate
use super::components::StableComponentId;

// =============================================================================
// Types
// =============================================================================

/// The live table one manifest kind keeps, keyed by stable identity.
pub(super) type BindingTable<Binding> = HashMap<StableComponentId, Binding>;

/// What an action reports: the binding it produced, and how to reverse it.
///
/// `None` in the second slot means the action's effect on the engine cannot be
/// reversed, which is a statement about the kind rather than about this call -
/// see the rollback note in this module's docs.
pub(super) type Applied<Binding, Undo> = Result<(Binding, Option<Undo>), CSharpError>;

/// Resolve one alias to the live table entry it names, if any.
type ResolveAlias<Binding> =
    fn(&Engine, &BindingTable<Binding>, &str) -> Result<Option<StableComponentId>, CSharpError>;

/// Register a declaration the table has no entry for.
type Register<Declaration, Binding, Undo> =
    fn(&mut Engine, StableComponentId, &Declaration) -> Applied<Binding, Undo>;

/// Migrate a live entry's stored data into the arriving shape.
type Reshape<Declaration, Binding, Undo> =
    fn(&mut Engine, StableComponentId, &Binding, &Declaration) -> Applied<Binding, Undo>;

/// Register a successor and move a predecessor's data onto it.
type Rename<Declaration, Binding, Undo> = fn(
    &mut Engine,
    StableComponentId,
    &RenamePredecessor<'_, Binding>,
    &Declaration,
) -> Applied<Binding, Undo>;

/// The live entry an arriving declaration's alias reached.
///
/// Passed whole because applying a rename retires the predecessor: by the time
/// the data has moved, its registration is gone and this is the only
/// description of what was there.
pub(super) struct RenamePredecessor<'binding, Binding> {
    /// The table key the predecessor is bound under.
    pub(super) identity: StableComponentId,
    /// The alias that reached it, which is the name it was registered under.
    pub(super) alias: &'binding str,
    /// Its live binding.
    pub(super) binding: &'binding Binding,
}

/// What an arriving declaration means for a live binding.
///
/// The one decision the pipeline cannot make for itself: whether the stored
/// bytes are still valid under the arriving shape is a question about layouts,
/// and only the kind knows how its layouts compare.
pub(super) enum Settlement {
    /// The live binding already describes the arriving declaration.
    Unchanged,
    /// The shape moved, so the stored bytes have to migrate.
    Reshaped,
}

/// What one manifest kind contributes to the shared pipeline.
///
/// A plain struct of function pointers rather than a trait, matching the
/// `ColumnOps` and `StorageFactory` style this codebase already uses for
/// per-kind behaviour tables. Every action takes the stable identity the
/// pipeline resolved, so no action has to re-derive it.
pub(super) struct ManifestSubject<Declaration, Binding, Undo> {
    /// What this kind is called in log lines - "component", "resource".
    pub(super) kind: &'static str,
    /// The stable identity an arriving declaration claims.
    pub(super) identity: fn(&Declaration) -> StableComponentId,
    /// The declared name, for reports and refusals.
    pub(super) name: fn(&Declaration) -> &str,
    /// The names this declaration says it used to be known by.
    pub(super) aliases: fn(&Declaration) -> &[String],
    /// The name a live binding was registered under, for the retirement report.
    pub(super) binding_name: fn(&Engine, &Binding) -> String,
    /// Whether a live entry is one this pipeline owns and may retire.
    ///
    /// A table can hold entries this manifest does not govern - the component
    /// table also holds native and module-native bindings - and those are never
    /// retired for having gone unmentioned.
    pub(super) is_governed: fn(&Binding) -> bool,
    /// Resolve one alias to the live table entry it names, if any.
    pub(super) resolve_alias: ResolveAlias<Binding>,
    /// Refuse a declaration the table has no entry for and that cannot be
    /// registered as it stands.
    pub(super) check_addable: fn(&Declaration) -> Result<(), CSharpError>,
    /// Decide what an arriving declaration means for a live binding.
    pub(super) settle: fn(&Engine, &Binding, &Declaration) -> Result<Settlement, CSharpError>,
    /// Register a declaration the table has no entry for.
    pub(super) register: Register<Declaration, Binding, Undo>,
    /// Migrate a live entry's stored bytes into the arriving shape.
    pub(super) reshape: Reshape<Declaration, Binding, Undo>,
    /// Register the successor and move the predecessor's data onto it.
    pub(super) rename: Rename<Declaration, Binding, Undo>,
    /// Retire every entry the manifest stopped naming, in one call.
    ///
    /// Runs last and reports how much it touched; it cannot fail, because
    /// nothing after it could react to a failure.
    pub(super) retire: fn(&mut Engine, &[Binding]) -> usize,
    /// Undo one applied entry during a rollback, best effort.
    pub(super) undo: fn(&mut Engine, Undo),
}

/// What one applied manifest changed.
///
/// One shape for every kind: the pipeline fills it and each caller folds it
/// into whatever report its own callers read.
#[derive(Debug, Default)]
pub(super) struct ManifestOutcome {
    /// Entries the arriving generation declares for the first time.
    pub(super) added: Vec<String>,
    /// Entries whose layout changed and whose stored data was migrated.
    pub(super) migrated: Vec<String>,
    /// Entries renamed from an earlier declaration, as `old name -> new name`.
    pub(super) renamed: Vec<String>,
    /// Entries retired because the manifest stopped naming them.
    pub(super) retired: Vec<String>,
}

/// One arriving declaration, resolved against the live table.
enum Resolution<'declaration, Declaration> {
    /// The table already agrees with the declaration.
    Settled,
    /// The table has no entry for it.
    Add(&'declaration Declaration),
    /// An alias reached a live predecessor, whose data moves onto this entry.
    Rename {
        declaration: &'declaration Declaration,
        predecessor: StableComponentId,
        alias: String,
    },
    /// The shape moved, so the stored data has to migrate.
    Reshape(&'declaration Declaration),
}

/// One applied entry, recorded so a later refusal can be undone.
///
/// The table half is the pipeline's: it remembers exactly which entries the
/// step wrote so a rollback can put the map back without the subject knowing a
/// table exists. The engine half is the subject's, and is absent for a step
/// whose effect cannot be reversed.
struct JournalEntry<Binding, Undo> {
    /// The identity the step wrote, and what the table held there before.
    wrote: (StableComponentId, Option<Binding>),
    /// A predecessor the step removed, and the binding it held.
    removed: Option<(StableComponentId, Binding)>,
    /// How to reverse the step's effect on the engine, when it can be.
    undo: Option<Undo>,
}

// =============================================================================
// The pipeline
// =============================================================================

/// Apply one kind's declarations to the live world and its binding table.
///
/// Returns the outcome and the table to publish. The table comes back on the
/// failing path too, because a refusal still has a correct table to publish -
/// everything the rollback put back, plus the entries whose engine effect could
/// not be reversed and must therefore keep describing what the engine holds.
///
/// # Errors
///
/// Returns a [`CSharpError`] when an alias claims more than one predecessor,
/// when a declaration cannot be registered as it stands, or when the engine
/// refuses a registration, a migration or a rename.
pub(super) fn apply_manifest<Declaration, Binding, Undo>(
    subject: &ManifestSubject<Declaration, Binding, Undo>,
    engine: &mut Engine,
    previous: BindingTable<Binding>,
    declarations: &[Declaration],
) -> (Result<ManifestOutcome, CSharpError>, BindingTable<Binding>)
where
    Binding: Clone,
{
    // Step 1: Resolve aliases, before anything else looks at what vanished.
    // An entry whose alias reaches a live predecessor is a rename, and its
    // predecessor is not a disappearance.
    let renames = match resolve_renames(subject, engine, &previous, declarations) {
        Ok(renames) => renames,
        Err(error) => return (Err(error), previous),
    };

    // Step 2: Collect the entries the manifest stopped naming, to be retired
    // after everything else has applied. Collected here because an alias in the
    // arriving manifest can claim one of them as a rename - its data would
    // move, not drop - and retired last because a retirement cannot be
    // journalled: nothing may run after it that could fail.
    let arriving: HashSet<StableComponentId> = declarations
        .iter()
        .map(|declaration| (subject.identity)(declaration))
        .collect();
    let renamed_sources: HashSet<StableComponentId> =
        renames.values().map(|(source, _)| *source).collect();
    let vanished: Vec<StableComponentId> = previous
        .iter()
        .filter(|(identity, binding)| {
            !arriving.contains(*identity)
                && !renamed_sources.contains(*identity)
                && (subject.is_governed)(binding)
        })
        .map(|(identity, _)| *identity)
        .collect();

    // Step 3: Resolve every entry before the first mutation. The reads below
    // are read-only, so a refusal here leaves the world and the table alone.
    let resolutions = match resolve_entries(subject, engine, &previous, declarations, &renames) {
        Ok(resolutions) => resolutions,
        Err(error) => return (Err(error), previous),
    };

    // Step 4: Execute, journalling one entry per applied resolution.
    let mut table = previous;
    let mut outcome = ManifestOutcome::default();
    let mut journal: Vec<JournalEntry<Binding, Undo>> = Vec::with_capacity(resolutions.len());
    for (identity, resolution) in resolutions {
        let applied = apply_one(
            subject,
            engine,
            &mut table,
            identity,
            resolution,
            &mut outcome,
        );
        match applied {
            Ok(Some(entry)) => journal.push(entry),
            Ok(None) => {}
            Err(error) => {
                roll_back(subject, engine, &mut table, journal);
                return (Err(error), table);
            }
        }
    }

    // Step 5: Retire the declarations the manifest stopped naming, now that
    // every add and migration has applied. Nothing may fail after this.
    if !vanished.is_empty() {
        let retired: Vec<Binding> = vanished
            .iter()
            .filter_map(|identity| table.get(identity).cloned())
            .collect();
        // The names are read before the retirement, not after: retiring an
        // entry takes its registration with it, and the report needs the name
        // it was registered under.
        for binding in &retired {
            outcome
                .retired
                .push((subject.binding_name)(engine, binding));
        }
        let touched = (subject.retire)(engine, &retired);
        for identity in &vanished {
            table.remove(identity);
        }
        info!(
            target: telemetry_target::HOT_RELOAD,
            kind = subject.kind,
            entries = retired.len(),
            touched,
            "retired managed declarations the manifest stopped naming"
        );
    }

    (Ok(outcome), table)
}

/// Resolve every declaration's aliases to the live entries they used to name.
///
/// One hop, against live entries only: an alias names something still
/// registered (otherwise there is no data to carry), and the entry it lands on
/// becomes a rename instead of an add. An alias that resolves to nothing is
/// left alone - it names nothing this path owns, and a later registration under
/// it is an ordinary add.
///
/// # Errors
///
/// Returns a [`CSharpError`] when the name is ambiguous, or when one entry
/// collects predecessors through more than one alias: two old registrations
/// cannot both be its past, and picking one would silently drop the other's
/// data.
fn resolve_renames<Declaration, Binding, Undo>(
    subject: &ManifestSubject<Declaration, Binding, Undo>,
    engine: &Engine,
    previous: &BindingTable<Binding>,
    declarations: &[Declaration],
) -> Result<HashMap<StableComponentId, (StableComponentId, String)>, CSharpError> {
    let mut renames: HashMap<StableComponentId, (StableComponentId, String)> = HashMap::new();
    for declaration in declarations {
        let successor = (subject.identity)(declaration);
        for alias in (subject.aliases)(declaration) {
            let Some(predecessor) = (subject.resolve_alias)(engine, previous, alias)? else {
                continue;
            };
            if renames.contains_key(&successor) {
                return Err(CSharpError::ManifestInvalid {
                    message: format!(
                        "managed {} {} declares aliases for more than one previous registration",
                        subject.kind,
                        (subject.name)(declaration),
                    ),
                });
            }
            renames.insert(successor, (predecessor, alias.clone()));
        }
    }
    Ok(renames)
}

/// Resolve every declaration against the live table, refusing before applying.
///
/// # Errors
///
/// Returns a [`CSharpError`] when a declaration cannot be registered as it
/// stands, when a rename's successor name is already bound, or when the
/// subject refuses the arriving shape against the live one.
#[allow(clippy::type_complexity)]
fn resolve_entries<'declaration, Declaration, Binding, Undo>(
    subject: &ManifestSubject<Declaration, Binding, Undo>,
    engine: &Engine,
    previous: &BindingTable<Binding>,
    declarations: &'declaration [Declaration],
    renames: &HashMap<StableComponentId, (StableComponentId, String)>,
) -> Result<Vec<(StableComponentId, Resolution<'declaration, Declaration>)>, CSharpError> {
    let mut resolved = Vec::with_capacity(declarations.len());
    for declaration in declarations {
        let identity = (subject.identity)(declaration);
        let Some(binding) = previous.get(&identity) else {
            (subject.check_addable)(declaration)?;
            // An alias that reached a predecessor turns this entry into a
            // rename; without one it is an ordinary addition.
            let resolution = match renames.get(&identity) {
                Some((predecessor, alias)) => Resolution::Rename {
                    declaration,
                    predecessor: *predecessor,
                    alias: alias.clone(),
                },
                None => Resolution::Add(declaration),
            };
            resolved.push((identity, resolution));
            continue;
        };

        // A successor's identity is derived from its new name, so a table hit
        // here means the name was already registered while its aliases still
        // name an earlier registration. Refused rather than settled: settling
        // would strand the predecessor's data in a binding nothing tracks.
        if renames.contains_key(&identity) {
            return Err(CSharpError::ManifestInvalid {
                message: format!(
                    "managed {} {} is already registered and also declares an alias for an \
                     earlier registration; retire one of the two names",
                    subject.kind,
                    (subject.name)(declaration),
                ),
            });
        }

        let resolution = match (subject.settle)(engine, binding, declaration)? {
            Settlement::Unchanged => Resolution::Settled,
            Settlement::Reshaped => Resolution::Reshape(declaration),
        };
        resolved.push((identity, resolution));
    }
    Ok(resolved)
}

/// Apply one resolved entry, recording what it wrote.
///
/// # Errors
///
/// Whatever the subject's action reports. Nothing is written to the table
/// unless the action succeeded, so a refusal here leaves the entry untouched
/// and the journal describes only entries that did apply.
fn apply_one<Declaration, Binding, Undo>(
    subject: &ManifestSubject<Declaration, Binding, Undo>,
    engine: &mut Engine,
    table: &mut BindingTable<Binding>,
    identity: StableComponentId,
    resolution: Resolution<'_, Declaration>,
    outcome: &mut ManifestOutcome,
) -> Result<Option<JournalEntry<Binding, Undo>>, CSharpError>
where
    Binding: Clone,
{
    match resolution {
        Resolution::Settled => Ok(None),
        Resolution::Add(declaration) => {
            let (binding, undo) = (subject.register)(engine, identity, declaration)?;
            let wrote = (identity, table.insert(identity, binding));
            outcome.added.push((subject.name)(declaration).to_string());
            Ok(Some(JournalEntry {
                wrote,
                removed: None,
                undo,
            }))
        }
        Resolution::Reshape(declaration) => {
            let existing = table
                .get(&identity)
                .cloned()
                .expect("a reshape is only resolved for an entry the table holds");
            let (binding, undo) = (subject.reshape)(engine, identity, &existing, declaration)?;
            let wrote = (identity, table.insert(identity, binding));
            outcome
                .migrated
                .push((subject.name)(declaration).to_string());
            Ok(Some(JournalEntry {
                wrote,
                removed: None,
                undo,
            }))
        }
        Resolution::Rename {
            declaration,
            predecessor,
            alias,
        } => {
            let predecessor_binding = table
                .get(&predecessor)
                .cloned()
                .expect("the resolver only names predecessors the table holds");
            let (binding, undo) = (subject.rename)(
                engine,
                identity,
                &RenamePredecessor {
                    identity: predecessor,
                    alias: &alias,
                    binding: &predecessor_binding,
                },
                declaration,
            )?;
            let wrote = (identity, table.insert(identity, binding));
            let removed = table
                .remove(&predecessor)
                .map(|removed| (predecessor, removed));
            outcome
                .renamed
                .push(format!("{alias} -> {}", (subject.name)(declaration)));
            Ok(Some(JournalEntry {
                wrote,
                removed,
                undo,
            }))
        }
    }
}

/// Undo every applied entry, newest first.
///
/// Best effort on the engine side by design: a rollback that fails leaves the
/// process mixed, and the failure is logged where it happened rather than
/// raised over the original refusal, which is the error the caller needs.
///
/// The table side is not best effort. An entry whose engine effect was reversed
/// has its table entry put back exactly as it was; an entry whose effect could
/// not be reversed keeps the binding it was given, because the table has to
/// keep describing what the engine actually holds.
fn roll_back<Declaration, Binding, Undo>(
    subject: &ManifestSubject<Declaration, Binding, Undo>,
    engine: &mut Engine,
    table: &mut BindingTable<Binding>,
    journal: Vec<JournalEntry<Binding, Undo>>,
) {
    for entry in journal.into_iter().rev() {
        let Some(undo) = entry.undo else {
            // Nothing to reverse: the step's effect on the engine is permanent,
            // so its binding stays in the table describing that effect.
            continue;
        };
        (subject.undo)(engine, undo);
        let (identity, replaced) = entry.wrote;
        match replaced {
            Some(binding) => table.insert(identity, binding),
            None => table.remove(&identity),
        };
        if let Some((predecessor, binding)) = entry.removed {
            table.insert(predecessor, binding);
        }
    }
    error!(
        target: telemetry_target::HOT_RELOAD,
        kind = subject.kind,
        "rolled back a refused manifest"
    );
}
