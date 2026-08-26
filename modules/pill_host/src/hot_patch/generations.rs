//! A patched function's history, and the routes back through it.
//!
//! # Responsibilities
//!
//! - Record every generation of every function this session replaced.
//! - Report that history to the console and to tooling.
//! - Reinstall an earlier generation, or the artifact's own original code.
//! - Forget prologue records a reload has invalidated.
//!
//! # Design
//!
//! Rollback is not one operation. How a generation was *delivered* decides how
//! it is undone: a slot is re-pointed by a single store, while a prologue patch
//! overwrote the artifact's own bytes and can only be undone by writing bytes
//! back. That distinction is recorded per generation rather than inferred from
//! the function's kind, because an annotated and an un-annotated plain function
//! are the same kind and are undone completely differently.
//!
//! A reload invalidates every recorded prologue address at once: the addresses
//! point into an image the graveyard will unmap, and a module's records include
//! addresses inside the project image, so they cannot be cleared selectively.
//! Dropping the whole set is what keeps a rollback from writing into a retired
//! image.

// Standard library
use std::time::Instant;

// External crates
use pill_core::info;

// Current crate
use super::{
    install_everywhere, reset_everywhere, source, HotPatchSession, LoadedPatch, PrologueRestore,
};
use crate::native_library::NativeLibrary;
use pill_engine::Engine;

/// One loaded patch library, kept mapped for the process lifetime.
///
/// Never unloaded, unlike the module graveyard: a slot holds an address inside
/// this image and nothing re-homes those addresses. That is also what makes
/// rollback a single pointer store rather than a rebuild.
pub(crate) struct Generation {
    /// Qualified name of the function this generation replaced.
    pub(super) function: String,
    /// One-based position in this function's history. Generation zero is the
    /// original code the running artifact was built with, which has no entry
    /// here because it needs no library.
    pub(super) number: u32,
    /// Address to install to make this generation active again.
    pub(super) address: usize,
    /// How the replacement is delivered, which decides what rollback does.
    pub(super) kind: source::HotFunctionKind,
    /// Signature identity a system's slot checks before accepting the address.
    pub(super) signature_hash: u64,
    /// The name the delivering route looks this function up by, which is not
    /// always [`Self::function`] - see `slot_lookup_name`.
    pub(super) lookup_name: String,
    /// Signature text a plain function's slot checks before accepting it.
    pub(super) signature: String,
    /// Bytes overwritten in each artifact, when this generation was delivered
    /// by prologue patching. Empty for a slot-delivered generation.
    pub(super) prologue_restores: Vec<PrologueRestore>,
    /// Whether [`Self::prologue_restores`] was emptied by a reload rather than
    /// never having been filled.
    ///
    /// The two are indistinguishable once the list is empty, and they need
    /// opposite answers: a slot-delivered generation is rolled back through its
    /// slot, while one whose addresses a reload invalidated cannot be rolled
    /// back at all. Without this the second case took the first case's route and
    /// failed claiming the crate was not loaded, which is not what went wrong.
    pub(super) prologue_history_dropped: bool,
    /// When this generation went live, for `patch_generations` listings.
    pub(super) installed_at: Instant,
}

impl Generation {
    /// Whether this generation was delivered by overwriting code rather than by
    /// installing into a slot.
    ///
    /// The distinction is recorded rather than inferred from the function's
    /// kind, because an un-annotated plain function and an annotated one are the
    /// same kind but are undone in completely different ways.
    fn overwrote_prologues(&self) -> bool {
        !self.prologue_restores.is_empty()
    }
}

/// One entry in a function's patch history, for hosts and tooling.
#[derive(Debug, Clone)]
pub struct PatchGeneration {
    /// Qualified name of the patched function.
    pub function: String,
    /// One-based position in the history; zero is the original code.
    pub number: u32,
    /// Address currently associated with this generation.
    pub address: usize,
    /// Seconds since this generation was installed.
    pub age_seconds: f64,
}

impl HotPatchSession {
    /// The generation currently running for one function.
    ///
    /// Zero means the original code the artifact was built with, which is also
    /// the answer when nothing has ever been patched.
    pub(super) fn active_generation(&self, qualified: &str) -> u32 {
        self.active_generations.get(qualified).copied().unwrap_or(0)
    }

    /// Every generation recorded for every function this session has patched.
    pub(crate) fn generations(&self) -> Vec<PatchGeneration> {
        self.generations
            .iter()
            .map(|generation| PatchGeneration {
                function: generation.function.clone(),
                number: generation.number,
                address: generation.address,
                age_seconds: generation.installed_at.elapsed().as_secs_f64(),
            })
            .collect()
    }

    /// Forget every prologue patch, because the images they refer to are gone.
    ///
    /// A reload replaces an artifact's image, so the addresses recorded when a
    /// prologue was overwritten no longer name that function - and the freshly
    /// loaded copy is unpatched, so the redirect is lost anyway. Writing saved
    /// bytes back to a stale address would corrupt whatever now occupies it, so
    /// the record is dropped rather than kept.
    ///
    /// Slot-delivered generations are untouched: a slot is re-created by the
    /// new artifact and re-installed through the registry, not by address.
    pub(crate) fn forget_prologue_patches(&mut self) {
        let mut functions: Vec<String> = Vec::new();
        for generation in &mut self.generations {
            if generation.overwrote_prologues() {
                generation.prologue_restores.clear();
                generation.prologue_history_dropped = true;
                if !functions.contains(&generation.function) {
                    functions.push(generation.function.clone());
                }
            }
        }
        if functions.is_empty() {
            return;
        }

        // The history no longer describes anything: the new image was compiled
        // from the current sources, so it already behaves like the newest
        // generation, and none of the recorded addresses point into it.
        for function in &functions {
            self.active_generations.remove(function);
        }

        // Said out loud rather than logged, because a developer who just rolled
        // back would otherwise watch the rollback silently undo itself.
        println!(
            "{} reload rebuilt from source; live-patch history reset for {}",
            crate::console::bold_cyan("[hot]"),
            functions.join(", ")
        );
        info!(
            target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
            functions = functions.join(", ").as_str(),
            "prologue patch history dropped: the reloaded image supersedes it"
        );
    }

    /// Whether this session has ever patched `qualified`.
    pub(crate) fn knows_function(&self, qualified: &str) -> bool {
        self.active_generations.contains_key(qualified)
    }

    /// Reinstall an earlier generation of one function.
    ///
    /// Generation zero is the original code the running artifact was built
    /// with; one and up are patches, in the order they were applied. This is a
    /// single pointer store per artifact - no rebuild, no reload, and nothing
    /// is unloaded, which is the concrete payoff of dispatching through a slot
    /// rather than overwriting a function's prologue.
    ///
    /// # Errors
    ///
    /// Returns a message when the function was never patched by this session,
    /// when the generation does not exist, or when an artifact refuses the
    /// address. On any error the currently running implementation is kept.
    pub(crate) fn rollback(
        &mut self,
        engine: &mut Engine,
        targets: &[(&str, &NativeLibrary)],
        patches: &[LoadedPatch],
        qualified: &str,
        number: u32,
    ) -> Result<(), String> {
        if !self.knows_function(qualified) {
            return Err(format!(
                "`{qualified}` has not been patched in this session, so it has \
                 no history to roll back to"
            ));
        }

        // How the generation was delivered decides how to undo it, and that is
        // not the same question as what kind of function it is: an un-annotated
        // function has no slot to install into, so its generations were written
        // over the code itself.
        let delivered_by_prologue = self
            .generations
            .iter()
            .any(|candidate| candidate.function == qualified && candidate.overwrote_prologues());

        if number == 0 {
            if delivered_by_prologue {
                self.restore_prologue_baseline(qualified)?;
            } else {
                self.restore_baseline(engine, targets, patches, qualified)?;
            }
        } else {
            let generation = self
                .generations
                .iter()
                .find(|candidate| candidate.function == qualified && candidate.number == number)
                .ok_or_else(|| format!("`{qualified}` has no generation {number}"))?;

            // Its addresses pointed into an image a reload has since replaced,
            // so there is nothing left to re-aim. Saying so is the point: the
            // slot route below would refuse too, but for the wrong reason.
            if generation.prologue_history_dropped {
                return Err(format!(
                    "generation {number} of `{qualified}` was written into the \
                     artifact's own code, and a reload has replaced that code \
                     since; it can no longer be rolled back to. The reloaded \
                     artifact already carries the current source"
                ));
            }

            if generation.overwrote_prologues() {
                // Re-aim every copy at this generation's body. The bytes each
                // call returns are deliberately discarded: they are the jump its
                // predecessor wrote, not the artifact's own code, and overwriting
                // the saved originals with them would lose the only way back to
                // generation zero.
                for restore in &generation.prologue_restores {
                    // SAFETY: the address came from this artifact's inventory
                    // when the generation was installed, and `patch_prologue`
                    // re-validates the function's extent before writing. No
                    // system is executing: rollback runs between frames.
                    unsafe {
                        pill_engine::hot_patch::patch_prologue(restore.address, generation.address)
                    }
                    .map_err(|detail| format!("{}: {detail}", restore.artifact))?;
                }
            } else {
                match generation.kind {
                    source::HotFunctionKind::System => engine
                        .hot_patch(qualified, generation.address, generation.signature_hash)
                        .map_err(|error| error.to_string())?,
                    // The fan-out count is only reported for a fresh patch;
                    // a rollback re-installs the same set and has nothing new
                    // to say about it.
                    source::HotFunctionKind::PlainFunction => {
                        install_everywhere(
                            targets,
                            patches,
                            &generation.lookup_name,
                            generation.address,
                            &generation.signature,
                        )?;
                    }
                }
            }
        }

        self.active_generations
            .insert(qualified.to_string(), number);
        info!(
            target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
            function = qualified,
            generation = number,
            "patch generation rolled back"
        );
        Ok(())
    }

    /// Put a prologue-patched function back to the code its artifact was built
    /// with.
    ///
    /// The bytes to write are the ones the FIRST generation saved. Every later
    /// generation saved the jump its predecessor had written, so restoring from
    /// the newest would reinstate a patch rather than remove one.
    fn restore_prologue_baseline(&self, qualified: &str) -> Result<(), String> {
        let first = self
            .generations
            .iter()
            .filter(|candidate| candidate.function == qualified && candidate.overwrote_prologues())
            .min_by_key(|candidate| candidate.number)
            .ok_or_else(|| {
                format!(
                    "`{qualified}` has no saved prologue bytes; the artifact was reloaded \
                     after it was patched, which already restored its own code"
                )
            })?;

        for restore in &first.prologue_restores {
            // SAFETY: the bytes and the address were recorded together when this
            // generation was installed, and `restore_prologue` re-reads the
            // function's extent before writing so a replaced image is refused
            // rather than overwritten. No system is executing.
            unsafe { pill_engine::hot_patch::restore_prologue(restore.address, &restore.original) }
                .map_err(|detail| format!("{}: {detail}", restore.artifact))?;
        }
        Ok(())
    }

    /// Return one function to the code its artifact was built with.
    ///
    /// A system has one baseline address, held by the engine's slot. A plain
    /// function has one per artifact, so each artifact is asked to empty its
    /// own slot instead of being handed an address that belongs to another.
    fn restore_baseline(
        &self,
        engine: &mut Engine,
        targets: &[(&str, &NativeLibrary)],
        patches: &[LoadedPatch],
        qualified: &str,
    ) -> Result<(), String> {
        // The route's own lookup name, not the canonical one: a slot for a
        // method is registered without its type, so asking under the canonical
        // name would miss it.
        let (kind, lookup_name) = self
            .generations
            .iter()
            .find(|candidate| candidate.function == qualified)
            .map(|generation| (generation.kind, generation.lookup_name.clone()))
            .ok_or_else(|| format!("`{qualified}` has no recorded generation"))?;

        match kind {
            source::HotFunctionKind::System => {
                let (address, signature_hash) = engine
                    .hot_patch_baseline(qualified)
                    .ok_or_else(|| format!("`{qualified}` records no baseline implementation"))?;
                engine
                    .hot_patch(qualified, address, signature_hash)
                    .map_err(|error| error.to_string())
            }
            source::HotFunctionKind::PlainFunction => {
                reset_everywhere(targets, patches, &lookup_name)
            }
        }
    }
}
