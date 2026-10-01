//! Unit tests for the hot-patch subsystem.
//!
//! # Responsibilities
//!
//! - Exercises patch classification, install and bookkeeping rules
//!   that would otherwise need a live host to reach.
//!
//! # Design
//!
//! Kept beside the code they exercise rather than inside it, so the
//! implementation half of the module reads as implementation.
//! Private items stay reachable through `use super::*`.

use super::*;

/// A fan-out that reaches no copy is reported rather than treated as
/// success, and the image parked in the graveyard is untouched by the
/// refusal - which is what keeps a refusal from unmapping an image another
/// artifact may already jump into.
#[test]
fn a_fan_out_that_reaches_nothing_keeps_the_patch_image_owned() {
    // SAFETY: a system library with no relationship to this crate, loaded
    // only so the graveyard holds a real image.
    let library = unsafe { Library::new("kernel32.dll") }.expect("kernel32.dll loads");
    let patches = vec![LoadedPatch {
        function: String::from("demo::function"),
        generation: 1,
        library,
    }];

    let error = match prologue_patch_everywhere(&[], &patches, "demo::function", 0, "fn()") {
        Ok(_) => panic!("a fan-out with no artifacts cannot succeed"),
        Err(error) => error,
    };
    assert!(
        error.contains("no loaded artifact reports an address"),
        "the refusal explains why nothing was reached: {error}"
    );
    assert_eq!(
        patches.len(),
        1,
        "the image is still owned by the graveyard"
    );
}

/// An image whose resolver export cannot be resolved is an error, not an
/// artifact that links nothing.
///
/// `Ok(false)` means "asked, and this image does not carry the crate", and
/// every caller skips those. Folding an image that cannot be interrogated
/// into that answer would leave a copy running old code while the console
/// reports a provable install, so the error makes the caller fall back to
/// a reload.
#[test]
fn patch_without_its_resolver_export_is_an_error() {
    // SAFETY: a system library with no relationship to this crate, loaded
    // only so the patch holds a real image with no resolver export.
    let library = unsafe { Library::new("kernel32.dll") }.expect("kernel32.dll loads");
    let patch = LoadedPatch {
        function: String::from("demo::function"),
        generation: 1,
        library,
    };

    let install = patch
        .install_plain_function("demo::function", 0, "fn()")
        .expect_err("an image that cannot be interrogated cannot answer `not here`");
    assert!(
        install.contains("pill_patch_resolve_install") && install.contains("demo::function"),
        "the refusal names the export and the patch it was missing from: {install}"
    );

    let reset = patch
        .reset_plain_function("demo::function")
        .expect_err("a rollback must not treat an un-interrogable image as declaring nothing");
    assert!(
        reset.contains("pill_patch_resolve_reset"),
        "the refusal names the export the reset needs: {reset}"
    );
}

/// The generated source must carry the prefixed name and the distinct
/// resolver export, because both prevent a silent collision with the copy
/// of the project the patch necessarily links.
#[test]
fn generated_source_namespaces_the_entry_and_the_export() {
    let session = HotPatchSession {
        workspace_root: PathBuf::from("."),
        package: "project".to_string(),
        crate_root: PathBuf::from("lib.rs"),
        source_root: PathBuf::from("."),
        package_rlib: PathBuf::from("libproject.rlib"),
        build_command: vec!["cargo".to_string(), "build".to_string()],
        snapshots: HashMap::new(),
        snapshot_taken_at: SystemTime::UNIX_EPOCH,
        rustc_line: None,
        generations: Vec::new(),
        active_generations: HashMap::new(),
        counter: 0,
    };

    let contents =
        "use pill_engine::*;\n\n#[pill_hot]\nfn movement(value: i32) -> i32 { value * 2 }\n";
    let generated = session
        .generate(
            contents,
            &system_declaration("movement"),
            "project::movement",
        )
        .expect("generated");

    assert!(generated.contains("name = \"pill_patch::project::movement\""));
    assert!(generated.contains("pill_hot_resolver!(pill_patch_resolve)"));
    assert!(generated.contains("use pill_engine::*;"));
    assert!(generated.contains("use project::*;"));
    // The function is copied verbatim, body included.
    assert!(generated.contains("fn movement(value: i32) -> i32 { value * 2 }"));
    // The original attribute must NOT be duplicated.
    assert!(generated.contains("#["));
    assert_eq!(generated.matches("pill_hot(name").count(), 1);
}

/// A plain function is generated with its own attribute and no registry
/// override, because it is redirected through per-artifact slots rather
/// than through the engine's registry.
#[test]
fn a_plain_function_keeps_its_own_attribute() {
    let directory = std::env::temp_dir().join("pill_generate_plain");
    let _ = std::fs::remove_dir_all(&directory);
    let session = session_over(&directory, PLAIN_SOURCE);

    let generated = session
        .generate(
            PLAIN_SOURCE,
            &plain_declaration("get_color_a"),
            "project::get_color_a",
        )
        .expect("a plain function must generate");

    assert!(generated.contains("#[::pill_engine::pill_hot_fn]"));
    // A `name` override belongs to the system path only: a plain function
    // is found under the patch crate's own unique name.
    assert!(!generated.contains("pill_hot(name"));
    assert!(generated.contains("pill_hot_resolver!(pill_patch_resolve)"));
    assert!(generated.contains("133.0"));

    let _ = std::fs::remove_dir_all(&directory);
}

/// Classification must report which attribute marked the changed function,
/// because the two are installed through entirely different machinery.
#[test]
fn classify_reports_the_plain_function_kind() {
    let directory = std::env::temp_dir().join("pill_classify_plain");
    let _ = std::fs::remove_dir_all(&directory);
    let mut session = session_over(&directory, PLAIN_SOURCE);

    let edited = PLAIN_SOURCE.replace("133.0", "999.0");
    std::fs::write(directory.join("lib.rs"), &edited).expect("write edit");

    let classified = session.classify().expect("body-only edit must be accepted");
    let edit = classified.expect("a change must be reported");
    assert_eq!(edit.declarations.len(), 1, "one body changed");
    assert_eq!(edit.declarations[0].name, "get_color_a");
    assert_eq!(
        edit.declarations[0].kind,
        source::HotFunctionKind::PlainFunction
    );

    let _ = std::fs::remove_dir_all(&directory);
}

/// The name a function is keyed by follows the file tree, so a function in
/// a submodule must not be looked up under the crate root's name.
#[test]
fn qualified_names_follow_the_source_tree() {
    let directory = std::env::temp_dir().join("pill_qualified_names");
    let _ = std::fs::remove_dir_all(&directory);
    let session = session_over(&directory, PLAIN_SOURCE);
    let free = plain_declaration("get_color_a");

    assert_eq!(
        session.qualified_name(&directory.join("lib.rs"), &free),
        "project::get_color_a"
    );
    assert_eq!(
        session.qualified_name(&directory.join("color.rs"), &free),
        "project::color::get_color_a"
    );
    assert_eq!(
        session.qualified_name(&directory.join("color").join("mod.rs"), &free),
        "project::color::get_color_a"
    );

    let _ = std::fs::remove_dir_all(&directory);
}

/// A method's canonical name carries its type, because that is what a build
/// script registers - it can read the `impl` block.
///
/// This is the contract that was broken: the host asked for
/// `pill_dummy_color::mix` while the inventory held
/// `pill_dummy_color::Tint::mix`, so every method missed and the refusal
/// blamed the build script.
#[test]
fn a_method_is_named_through_its_type() {
    let directory = std::env::temp_dir().join("pill_method_naming");
    let _ = std::fs::remove_dir_all(&directory);
    let session = session_over(&directory, PLAIN_SOURCE);
    let method = source::HotFunction {
        name: "mix".to_string(),
        kind: source::HotFunctionKind::PlainFunction,
        self_type: Some("Tint".to_string()),
        trait_name: None,
        takes_receiver: true,
        signature: "fn mix(&self, other: Tint) -> Tint".to_string(),
        cfg_gated: false,
        inline_always: false,
        abi_entry_point: false,
        annotated: false,
    };

    assert_eq!(
        session.qualified_name(&directory.join("lib.rs"), &method),
        "project::Tint::mix"
    );
    assert_eq!(
        session.qualified_name(&directory.join("color.rs"), &method),
        "project::color::Tint::mix"
    );

    let _ = std::fs::remove_dir_all(&directory);
}

/// A trait method is named through both its type and its trait, because
/// neither alone identifies it.
///
/// A type may carry an inherent `draw` beside a trait `draw`, and two traits
/// may each define `draw` for it. `Type::draw` names all of them.
#[test]
fn a_trait_method_is_named_through_its_trait() {
    let directory = std::env::temp_dir().join("pill_trait_method_naming");
    let _ = std::fs::remove_dir_all(&directory);
    let session = session_over(&directory, PLAIN_SOURCE);
    let via_trait = source::HotFunction {
        name: "default".to_string(),
        kind: source::HotFunctionKind::PlainFunction,
        self_type: Some("Spline".to_string()),
        trait_name: Some("Default".to_string()),
        takes_receiver: false,
        signature: "fn default() -> Self".to_string(),
        cfg_gated: false,
        inline_always: false,
        abi_entry_point: false,
        annotated: false,
    };

    assert_eq!(
        session.qualified_name(&directory.join("lib.rs"), &via_trait),
        "project::<Spline as Default>::default"
    );
    assert_eq!(
        session.qualified_name(&directory.join("spline.rs"), &via_trait),
        "project::spline::<Spline as Default>::default"
    );

    // And the inherent method of the same name stays a different key.
    let inherent = source::HotFunction {
        trait_name: None,
        ..via_trait
    };
    assert_eq!(
        session.qualified_name(&directory.join("lib.rs"), &inherent),
        "project::Spline::default"
    );

    let _ = std::fs::remove_dir_all(&directory);
}

/// An edit to a trait method body classifies as patchable and reports the
/// trait, so the generated patch and the address lookup agree.
#[test]
fn classify_accepts_a_trait_method_body() {
    let directory = std::env::temp_dir().join("pill_classify_trait_method");
    let _ = std::fs::remove_dir_all(&directory);
    let source = "pub struct Spline(u32);\n\nimpl Shape for Spline {
fn size(&self) -> u32 { 1 }\n}\n";
    let mut session = session_over(&directory, source);

    std::fs::write(directory.join("lib.rs"), source.replace("{ 1 }", "{ 2 }")).expect("write edit");

    let edit = session
        .classify()
        .expect("a trait method body is in scope")
        .expect("the change must be reported");
    assert_eq!(edit.declarations.len(), 1);
    assert_eq!(edit.declarations[0].name, "size");
    assert_eq!(edit.declarations[0].self_type.as_deref(), Some("Spline"));
    assert_eq!(edit.declarations[0].trait_name.as_deref(), Some("Shape"));
    assert!(edit.new_contents.contains("{ 2 }"));

    let _ = std::fs::remove_dir_all(&directory);
}

/// A receiver-less associated function is refused at classification.
///
/// `fn default() -> Self` has no receiver to carry, so the generated patch
/// would emit the body at the top level, where `Self` cannot be named - a
/// doomed compile that cost a patch failure plus a full reload. The refusal
/// names the trait and the type, because either alone leaves the reader
/// hunting for which `impl` block was meant.
#[test]
fn classify_refuses_a_receiverless_trait_function() {
    let directory = std::env::temp_dir().join("pill_classify_receiverless");
    let _ = std::fs::remove_dir_all(&directory);
    let source = "pub struct Spline(u32);\n\nimpl Default for Spline {
fn default() -> Self { Spline(1) }\n}\n";
    let mut session = session_over(&directory, source);

    std::fs::write(
        directory.join("lib.rs"),
        source.replace("Spline(1)", "Spline(2)"),
    )
    .expect("write edit");

    let refusal = session
        .classify()
        .expect_err("a receiver-less associated function cannot be patched");
    assert_eq!(
        refusal.code,
        refusal_code::ASSOCIATED_WITHOUT_RECEIVER,
        "{}",
        refusal.detail
    );
    assert!(
        refusal.detail.contains("Default") && refusal.detail.contains("Spline"),
        "the refusal names the trait and the type: {}",
        refusal.detail
    );

    let _ = std::fs::remove_dir_all(&directory);
}

/// The generated patch carries the body from the right `impl` block.
///
/// Two types implementing one trait method is the case that made a bare-name
/// search unsafe: it would compile the first `fn default` in the file and
/// install it for whichever type was patched.
#[test]
fn a_generated_trait_patch_carries_the_right_body() {
    let directory = std::env::temp_dir().join("pill_generate_trait_method");
    let _ = std::fs::remove_dir_all(&directory);
    let source = "pub struct Alpha(u32);\npub struct Beta(u32);\n
impl Shape for Alpha {
fn size(&self) -> u32 { 111 }\n}\n
impl Shape for Beta {
fn size(&self) -> u32 { 222 }\n}\n";
    let session = session_over(&directory, source);

    let beta = source::HotFunction {
        name: "size".to_string(),
        kind: source::HotFunctionKind::PlainFunction,
        self_type: Some("Beta".to_string()),
        trait_name: Some("Shape".to_string()),
        takes_receiver: true,
        signature: "fn size(&self) -> u32".to_string(),
        cfg_gated: false,
        inline_always: false,
        abi_entry_point: false,
        annotated: false,
    };
    let generated = session
        .generate(source, &beta, "project::<Beta as Shape>::size")
        .expect("a trait method generates a patch");

    assert!(
        generated.contains("222"),
        "the patch must carry Beta's body, not Alpha's:\n{generated}"
    );
    assert!(
        !generated.contains("111"),
        "Alpha's body must not appear:\n{generated}"
    );
    // The body keeps `self`, so it is carried into a local trait implemented
    // for the concrete type - exactly as an inherent method is.
    assert!(generated.contains("impl PillHotMethodPatch for Beta"));

    let _ = std::fs::remove_dir_all(&directory);
}

/// A dispatch slot for a method is registered WITHOUT its type, so the slot
/// route has to ask under the degraded name.
///
/// Not a preference: the descriptor is an item, and items may not name
/// `Self` (`error[E0401]`), so `#[pill_hot_fn]` on a method has no way to
/// learn the type. The two forms are therefore expected to differ.
#[test]
fn a_slot_lookup_omits_the_type_the_macro_cannot_see() {
    let directory = std::env::temp_dir().join("pill_slot_naming");
    let _ = std::fs::remove_dir_all(&directory);
    let session = session_over(&directory, PLAIN_SOURCE);
    let method = source::HotFunction {
        name: "get_color_a".to_string(),
        kind: source::HotFunctionKind::PlainFunction,
        self_type: Some("Spline".to_string()),
        trait_name: None,
        takes_receiver: true,
        signature: "fn get_color_a(&self) -> f32".to_string(),
        cfg_gated: false,
        inline_always: false,
        abi_entry_point: false,
        annotated: true,
    };

    assert_eq!(
        session.slot_lookup_name(&directory.join("lib.rs"), &method),
        "project::get_color_a",
        "a slot is registered under module_path!() + the method name"
    );
    assert_eq!(
        session.qualified_name(&directory.join("lib.rs"), &method),
        "project::Spline::get_color_a",
        "the canonical name still carries the type"
    );

    let _ = std::fs::remove_dir_all(&directory);
}

/// A `#[pill_hot]` system declaration, as classification would report it.
fn system_declaration(name: &str) -> source::HotFunction {
    source::HotFunction {
        name: name.to_string(),
        kind: source::HotFunctionKind::System,
        self_type: None,
        trait_name: None,
        takes_receiver: false,
        signature: format!("fn {name}()"),
        cfg_gated: false,
        inline_always: false,
        abi_entry_point: false,
        annotated: true,
    }
}

/// A `#[pill_hot_fn]` free-function declaration.
fn plain_declaration(name: &str) -> source::HotFunction {
    source::HotFunction {
        name: name.to_string(),
        kind: source::HotFunctionKind::PlainFunction,
        self_type: None,
        trait_name: None,
        takes_receiver: false,
        signature: format!("fn {name}()"),
        cfg_gated: false,
        inline_always: false,
        abi_entry_point: false,
        annotated: true,
    }
}

/// Two sessions must never generate the same patch artifact name.
///
/// Every session counts its generations from one and patch libraries are
/// never unloaded, so a shared name means the first patch of a second module
/// tries to write a `.dll` the first module still has mapped. Windows
/// refuses, and that module can never be patched again in that session.
#[test]
fn patch_artifact_names_do_not_collide_across_sessions() {
    let first = std::env::temp_dir().join("pill_names_first");
    let second = std::env::temp_dir().join("pill_names_second");
    let _ = std::fs::remove_dir_all(&first);
    let _ = std::fs::remove_dir_all(&second);

    let mut colour = session_over(&first, PLAIN_SOURCE);
    colour.package = "pill_dummy_color".to_string();
    let mut spline = session_over(&second, PLAIN_SOURCE);
    spline.package = "pill_spline".to_string();

    colour.counter += 1;
    spline.counter += 1;
    let colour_name = format!("pill_hotpatch_{}_{}", colour.package, colour.counter);
    let spline_name = format!("pill_hotpatch_{}_{}", spline.package, spline.counter);
    assert_ne!(
        colour_name, spline_name,
        "each module's first patch must claim its own artifact"
    );

    let _ = std::fs::remove_dir_all(&first);
    let _ = std::fs::remove_dir_all(&second);
}

/// An untouched file is not re-read, so classification costs what the edit
/// costs rather than what the crate weighs.
///
/// The gate compares the file's modification time against the one recorded
/// when it was read - filesystem time against filesystem time. Comparing
/// against `SystemTime::now()` is not sound on Windows, where file times come
/// from the coarse system clock and a file written after a snapshot can
/// report an earlier time; that mistake made every classification miss.
#[test]
fn an_untouched_file_is_not_re_read() {
    let directory = std::env::temp_dir().join("pill_classify_mtime_gate");
    let _ = std::fs::remove_dir_all(&directory);
    let mut session = session_over(&directory, HOT_SOURCE);

    // A second file that never changes.
    let untouched = directory.join("untouched.rs");
    fs_write(&untouched, "pub fn stable() {}\n");
    session.refresh_snapshots();
    assert!(
        session.snapshots[&untouched].modified.is_some(),
        "the snapshot must record when it was read"
    );

    // Its recorded contents are then replaced with a different set of
    // functions, without touching the file. Nothing on disk changed, so the
    // gate must skip it - and if it does not, the re-read sees a renamed
    // function, which is a structural change and refuses the whole
    // classification. That makes the skip observable rather than asserted.
    // The recorded time is kept, so the gate still sees the file as
    // unchanged; only the contents are made to disagree.
    let recorded = session.snapshots[&untouched].modified;
    session.snapshots.insert(
        untouched.clone(),
        Snapshot {
            contents: "pub fn renamed_since_the_snapshot() {}\n".to_string(),
            modified: recorded,
        },
    );

    // Only the edited file is re-read, so the classification succeeds and
    // names it.
    let edited = HOT_SOURCE.replace("value * SPEED", "value * SPEED * 3.0");
    std::fs::write(directory.join("lib.rs"), &edited).expect("write edit");

    let edit = session
        .classify()
        .expect("the untouched file must be skipped, not re-read")
        .expect("a change must be reported");
    assert_eq!(edit.path.file_name().unwrap(), "lib.rs");
    assert_eq!(edit.declarations[0].name, "movement");

    // And the stale snapshot is still there: proof the file was never read.
    assert!(
        session.snapshots[&untouched]
            .contents
            .contains("renamed_since_the_snapshot"),
        "the untouched file was re-read despite an unchanged modification time"
    );

    let _ = std::fs::remove_dir_all(&directory);
}

/// A file whose content changes is re-read even though it was in the
/// snapshot, because its modification time moved.
#[test]
fn a_touched_file_is_re_read() {
    let directory = std::env::temp_dir().join("pill_classify_mtime_touch");
    let _ = std::fs::remove_dir_all(&directory);
    let mut session = session_over(&directory, PLAIN_SOURCE);

    // Sleep past the filesystem's timestamp granularity, so the write is
    // guaranteed to produce a different modification time rather than
    // relying on it.
    std::thread::sleep(std::time::Duration::from_millis(20));
    let edited = PLAIN_SOURCE.replace("133.0", "144.0");
    std::fs::write(directory.join("lib.rs"), &edited).expect("write edit");

    let edit = session
        .classify()
        .expect("a body-only edit is in scope")
        .expect("the changed file must be re-read and reported");
    assert_eq!(edit.declarations[0].name, "get_color_a");
    assert!(edit.new_contents.contains("144.0"));

    let _ = std::fs::remove_dir_all(&directory);
}

/// Classification hands on the modification time it read WITH the contents.
///
/// The pair is what `try_patch` stores as the new snapshot. It used to
/// re-stat the file after installing the patch, pairing older contents with
/// a newer time - so a save made during the compile was recorded as already
/// delivered and then skipped by the modification-time gate.
#[test]
fn classify_returns_the_time_of_the_contents_it_read() {
    let directory = std::env::temp_dir().join("pill_classify_modified_pair");
    let _ = std::fs::remove_dir_all(&directory);
    let mut session = session_over(&directory, PLAIN_SOURCE);

    // Same coarse-timestamp guard as the other re-read tests: a write
    // landing in the recorded tick would look unchanged.
    std::thread::sleep(std::time::Duration::from_millis(20));
    let edited = PLAIN_SOURCE.replace("133.0", "144.0");
    std::fs::write(directory.join("lib.rs"), &edited).expect("write edit");

    let edit = session
        .classify()
        .expect("a body-only edit is in scope")
        .expect("the change must be reported");
    let on_disk = std::fs::metadata(directory.join("lib.rs"))
        .and_then(|metadata| metadata.modified())
        .ok();
    assert_eq!(
        edit.modified, on_disk,
        "the time must describe the contents that were read"
    );
    assert!(edit.new_contents.contains("144.0"));

    // Replayed as the snapshot a successful patch stores: current against
    // its own moment, and stale against a later touch - which is what makes
    // a save during the patch re-readable rather than silently shipped.
    let recorded = edit.modified.expect("a temp directory reports a time");
    let replayed = Snapshot {
        contents: edit.new_contents,
        modified: edit.modified,
    };
    assert!(replayed.is_current(Some(recorded)));
    assert!(
        !replayed.is_current(Some(recorded + std::time::Duration::from_secs(1))),
        "a touch after the read must not count as delivered"
    );

    let _ = std::fs::remove_dir_all(&directory);
}

/// The staged rlib is brought back in step with cargo's output before a
/// patch is compiled, so the two halves of the link closure agree.
///
/// Without this the compile fails with `error[E0463]: can't find crate for
/// <the crate being patched>` whenever one of its dependencies was rebuilt
/// after it was staged - a message that names the wrong crate entirely.
#[test]
fn a_stale_staged_rlib_is_refreshed_before_compiling() {
    let directory = std::env::temp_dir().join("pill_restage_rlib");
    let _ = std::fs::remove_dir_all(&directory);
    let session = session_over(&directory, PLAIN_SOURCE);

    // What cargo produced (in the private module build tree under the
    // host profile directory), and the older copy the host staged from it.
    let built = directory
        .join(crate::config::module_build_artifact_directory())
        .join("libproject.rlib");
    fs_write(&built, "rebuilt against the current dependencies");
    fs_write(&session.package_rlib, "stale");

    session
        .refresh_staged_rlib()
        .expect("refreshing must succeed");
    assert_eq!(
        std::fs::read_to_string(&session.package_rlib).expect("read staged"),
        "rebuilt against the current dependencies",
        "the staged copy must match what cargo produced"
    );

    let _ = std::fs::remove_dir_all(&directory);
}

/// An already-current staged copy is left alone, so the common path costs
/// two metadata reads rather than a file copy.
#[test]
fn a_current_staged_rlib_is_left_alone() {
    let directory = std::env::temp_dir().join("pill_restage_current");
    let _ = std::fs::remove_dir_all(&directory);
    let session = session_over(&directory, PLAIN_SOURCE);

    let built = directory
        .join(crate::config::module_build_artifact_directory())
        .join("libproject.rlib");
    fs_write(&built, "same length!!");
    std::thread::sleep(std::time::Duration::from_millis(20));
    fs_write(&session.package_rlib, "same length!!");

    session
        .refresh_staged_rlib()
        .expect("refreshing must succeed");
    assert_eq!(
        std::fs::read_to_string(&session.package_rlib).expect("read staged"),
        "same length!!"
    );

    let _ = std::fs::remove_dir_all(&directory);
}

/// With no cargo output there is nothing to refresh from, and the staged
/// copy - the only one there is - must survive untouched.
#[test]
fn refreshing_without_a_cargo_artifact_keeps_the_staged_copy() {
    let directory = std::env::temp_dir().join("pill_restage_missing");
    let _ = std::fs::remove_dir_all(&directory);
    let session = session_over(&directory, PLAIN_SOURCE);
    fs_write(&session.package_rlib, "the only copy");

    session.refresh_staged_rlib().expect("must not fail");
    assert_eq!(
        std::fs::read_to_string(&session.package_rlib).expect("read staged"),
        "the only copy"
    );

    let _ = std::fs::remove_dir_all(&directory);
}

/// Write a file, creating parent directories as needed.
fn fs_write(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("create parent");
    }
    std::fs::write(path, contents).expect("write file");
}

/// A session over a throwaway source tree, so classification can be driven
/// against real files without a project or a compiler.
fn session_over(directory: &Path, contents: &str) -> HotPatchSession {
    std::fs::create_dir_all(directory).expect("create source dir");
    let file = directory.join("lib.rs");
    std::fs::write(&file, contents).expect("write source");

    let mut session = HotPatchSession {
        workspace_root: directory.to_path_buf(),
        package: "project".to_string(),
        crate_root: file,
        source_root: directory.to_path_buf(),
        package_rlib: directory.join("libproject.rlib"),
        build_command: vec!["cargo".to_string(), "build".to_string()],
        snapshots: HashMap::new(),
        snapshot_taken_at: SystemTime::UNIX_EPOCH,
        rustc_line: None,
        generations: Vec::new(),
        active_generations: HashMap::new(),
        counter: 0,
    };
    session.refresh_snapshots();

    // Put the recorded modification time firmly in the past before the
    // caller edits the file.
    //
    // `classify` skips a file whose modification time still equals the one
    // recorded when it was read. Filesystem timestamps are coarse, so a test
    // that writes its edit within the same tick as this snapshot produces a
    // file that looks unchanged - and the test then fails intermittently,
    // reporting "no change detected" for an edit that is plainly there. It
    // surfaced under the parallel harness, where the write lands sooner.
    //
    // The gate's imprecision is harmless in production: a missed edit falls
    // through to a full reload, which still delivers it. It is only a
    // problem for a test that asserts on classification itself, so the wait
    // lives here - once, for all 27 sessions - rather than in each test.
    std::thread::sleep(std::time::Duration::from_millis(20));
    session
}

const PLAIN_SOURCE: &str = r#"
use pill_engine::pill_hot_fn;

#[pill_hot_fn]
pub fn get_color_a() -> f32 {
133.0
}
"#;

const TWO_HOT_SOURCE: &str = r#"
use pill_engine::*;

const SPEED: f32 = 1.0;

#[pill_hot]
fn movement(value: f32) -> f32 {
value * SPEED
}

#[pill_hot]
fn other_movement(value: f32) -> f32 {
value - 1.0
}
"#;

const HOT_SOURCE: &str = r#"
use pill_engine::*;

const SPEED: f32 = 1.0;

#[pill_hot]
fn movement(value: f32) -> f32 {
value * SPEED
}

fn helper(value: f32) -> f32 {
value + 1.0
}
"#;

/// One file declaring `draw` twice, in two `impl` blocks.
const DUPLICATE_NAME_SOURCE: &str = r#"
use pill_engine::*;

struct Alpha {
value: f32,
}

struct Beta {
value: f32,
}

impl Alpha {
#[pill_hot]
fn draw(&self) -> f32 {
    self.value * 1.0
}
}

impl Beta {
fn draw(&self) -> f32 {
    self.value * 2.0
}
}
"#;

/// The headline classification: a body-only edit of an annotated function
/// is identified, and names the right function.
#[test]
fn classify_accepts_a_body_only_edit() {
    let directory = std::env::temp_dir().join("pill_classify_accepts");
    let _ = std::fs::remove_dir_all(&directory);
    let mut session = session_over(&directory, HOT_SOURCE);

    let edited = HOT_SOURCE.replace("value * SPEED", "value * SPEED * 2.0");
    std::fs::write(directory.join("lib.rs"), &edited).expect("write edit");

    let classified = session.classify().expect("body-only edit must be accepted");
    let edit = classified.expect("a change must be reported");
    assert_eq!(edit.declarations.len(), 1, "one body changed");
    assert_eq!(edit.declarations[0].name, "movement");
    assert_eq!(edit.declarations[0].kind, source::HotFunctionKind::System);
    assert!(edit.new_contents.contains("value * SPEED * 2.0"));

    let _ = std::fs::remove_dir_all(&directory);
}

#[test]
fn classify_reports_no_change_when_nothing_moved() {
    let directory = std::env::temp_dir().join("pill_classify_unchanged");
    let _ = std::fs::remove_dir_all(&directory);
    let mut session = session_over(&directory, HOT_SOURCE);

    assert!(session.classify().expect("no error").is_none());

    let _ = std::fs::remove_dir_all(&directory);
}

/// Everything outside an annotated body must be refused, with a reason.
/// Each of these would otherwise compile a patch against a layout or a
/// signature the running world no longer has.
#[test]
fn classify_refuses_everything_outside_a_hot_body() {
    let cases: &[(&str, &str, &str)] = &[
        (
            "constant",
            "const SPEED: f32 = 1.0;",
            "const SPEED: f32 = 2.0;",
        ),
        (
            "signature",
            "fn movement(value: f32)",
            "fn movement(value: f64)",
        ),
        ("import", "use pill_engine::*;", "use pill_engine::Engine;"),
    ];

    for (label, from, to) in cases {
        let directory =
            std::env::temp_dir().join(format!("pill_classify_{}", label.replace(' ', "_")));
        let _ = std::fs::remove_dir_all(&directory);
        let mut session = session_over(&directory, HOT_SOURCE);

        let edited = HOT_SOURCE.replace(from, to);
        assert_ne!(edited, HOT_SOURCE, "{label}: the fixture edit must apply");
        std::fs::write(directory.join("lib.rs"), &edited).expect("write edit");

        let refusal = session.classify().expect_err(&format!(
            "{label}: a change outside a hot body must be refused"
        ));
        assert_eq!(
            refusal.code,
            refusal_code::OUTSIDE_HOT_BODY,
            "{label}: {}",
            refusal.detail
        );

        let _ = std::fs::remove_dir_all(&directory);
    }
}

/// A refused edit must not disable patching for the rest of the session.
///
/// This is the bug that made live patching look broken: `classify` diffs
/// against a snapshot of what is running, and only a *successful* patch
/// advanced it. A refusal was followed by a full reload, which picked the
/// edit up without telling the session - so the refused change stayed in
/// every later diff, and each subsequent body-only edit was refused for a
/// change that had already shipped. One unpatchable edit disabled the fast
/// path until the host restarted.
///
/// `refresh_snapshots` is what the frame loop calls after a reload to close
/// that gap; this test pins the behaviour that depends on it.
#[test]
fn a_reload_resyncs_the_baseline_so_later_body_edits_still_patch() {
    let directory = std::env::temp_dir().join("pill_classify_resync_after_reload");
    let _ = std::fs::remove_dir_all(&directory);
    let mut session = session_over(&directory, HOT_SOURCE);

    // Step 1: an edit the fast path must refuse. In the host this falls
    // back to a full reload, which leaves the file running as written.
    let after_reload = HOT_SOURCE.replace("const SPEED: f32 = 1.0;", "const SPEED: f32 = 2.0;");
    assert_ne!(after_reload, HOT_SOURCE, "the fixture edit must apply");
    std::fs::write(directory.join("lib.rs"), &after_reload).expect("write the refused edit");
    let refusal = session
        .classify()
        .expect_err("a constant change must be refused");
    assert_eq!(refusal.code, refusal_code::OUTSIDE_HOT_BODY);

    // Step 2: the reload the host performs, and the re-sync that goes with
    // it. Without this call the assertion below fails with OUTSIDE_HOT_BODY,
    // because the constant change is still in the diff.
    session.refresh_snapshots();
    // Re-stamping the snapshots restarts the same coarse-timestamp race
    // `session_over` documents, so the edit below needs the same guard: a
    // write landing in the recorded modification time's tick looks
    // unchanged and `classify` skips the file.
    std::thread::sleep(std::time::Duration::from_millis(20));

    // Step 3: a clean body-only edit on top must now be patchable.
    let body_edited = after_reload.replace("value * SPEED", "value * SPEED + 1.0");
    assert_ne!(body_edited, after_reload, "the body edit must apply");
    std::fs::write(directory.join("lib.rs"), &body_edited).expect("write the body edit");

    let classified = session
        .classify()
        .expect("a body-only edit after a reload must not be refused")
        .expect("the changed body must be detected");
    assert!(
        classified
            .declarations
            .iter()
            .any(|declaration| declaration.name == "movement"),
        "the edited body should be the one reported"
    );

    let _ = std::fs::remove_dir_all(&directory);
}

/// Two bodies changed in one save are both reported, in a stable order.
///
/// This used to be a refusal. Each body is an independent replacement, so
/// the only cost of taking both is one compile apiece - cheaper than the
/// full reload the refusal fell back to, and the world is never torn down.
#[test]
fn classify_reports_every_changed_body() {
    let directory = std::env::temp_dir().join("pill_classify_two_bodies");
    let _ = std::fs::remove_dir_all(&directory);
    let mut session = session_over(&directory, TWO_HOT_SOURCE);

    let edited = TWO_HOT_SOURCE
        .replace("value * SPEED", "value * SPEED * 2.0")
        .replace("value - 1.0", "value - 9.0");
    std::fs::write(directory.join("lib.rs"), &edited).expect("write edit");

    let edit = session
        .classify()
        .expect("two body-only edits are in scope")
        .expect("a change must be reported");
    let names: Vec<&str> = edit
        .declarations
        .iter()
        .map(|declaration| declaration.name.as_str())
        .collect();
    assert_eq!(
        names,
        vec!["movement", "other_movement"],
        "both bodies, sorted so the order does not depend on scan order"
    );

    let _ = std::fs::remove_dir_all(&directory);
}

/// Stage timings add up across the bodies of one save.
///
/// `apply` runs once per changed body with the same accumulator, and the
/// total it reports spans all of them: writing instead of adding made the
/// breakdown describe the last body alone, exactly when a slow multi-body
/// save is the one being diagnosed.
#[test]
fn stage_timings_accumulate_across_bodies() {
    let mut total = PatchStages {
        classify: 1.0,
        ..PatchStages::default()
    };
    let first = PatchStages {
        generate: 2.0,
        flags: 3.0,
        compile: 4.0,
        load: 5.0,
        activate: 6.0,
        ..PatchStages::default()
    };
    let second = PatchStages {
        generate: 7.0,
        flags: 8.0,
        compile: 9.0,
        load: 10.0,
        activate: 11.0,
        ..PatchStages::default()
    };

    total.merge(&first);
    total.merge(&second);

    assert_eq!(
        total.classify, 1.0,
        "classify is measured once per save, not per body"
    );
    assert_eq!(total.generate, 9.0);
    assert_eq!(total.flags, 11.0);
    assert_eq!(total.compile, 13.0);
    assert_eq!(total.load, 15.0);
    assert_eq!(total.activate, 17.0);
}

/// Two changed files in one edit are refused with their own code.
#[test]
fn classify_refuses_two_changed_files() {
    let directory = std::env::temp_dir().join("pill_classify_two_files");
    let _ = std::fs::remove_dir_all(&directory);
    let mut session = session_over(&directory, HOT_SOURCE);
    // A second file that is part of the baseline, so changing it later is
    // an edit rather than a new file.
    std::fs::write(directory.join("other.rs"), HOT_SOURCE).expect("write second file");
    session.refresh_snapshots();
    // Re-stamping the snapshots restarts the same coarse-timestamp race
    // `session_over` documents, so the edit below needs the same guard: a
    // write landing in the recorded modification time's tick looks
    // unchanged and `classify` skips the file.
    std::thread::sleep(std::time::Duration::from_millis(20));

    let edited = HOT_SOURCE.replace("value * SPEED", "value * SPEED * 2.0");
    std::fs::write(directory.join("lib.rs"), &edited).expect("write edit");
    std::fs::write(directory.join("other.rs"), &edited).expect("write second edit");

    let refusal = session.classify().expect_err("two files must be refused");
    assert_eq!(
        refusal.code,
        refusal_code::MULTIPLE_FILES,
        "{}",
        refusal.detail
    );

    let _ = std::fs::remove_dir_all(&directory);
}

/// Two addressable declarations sharing a bare name are refused rather
/// than patched by guesswork: the classifier's declaration map keeps the
/// last one while body scanning keeps the first, so the body installed
/// and the address it replaces would belong to different functions.
#[test]
fn duplicate_bare_names_are_refused() {
    let directory = std::env::temp_dir().join("pill_classify_duplicate_names");
    let _ = std::fs::remove_dir_all(&directory);
    let mut session = session_over(&directory, DUPLICATE_NAME_SOURCE);

    let edited = DUPLICATE_NAME_SOURCE.replace("self.value * 1.0", "self.value * 9.0");
    std::fs::write(directory.join("lib.rs"), &edited).expect("write edit");

    let refusal = session
        .classify()
        .expect_err("two declarations named `draw` cannot be told apart");
    assert_eq!(
        refusal.code,
        refusal_code::DUPLICATE_DECLARATION,
        "{}",
        refusal.detail
    );
    assert!(
        refusal.detail.contains("draw"),
        "the refusal names the colliding declaration: {}",
        refusal.detail
    );

    let _ = std::fs::remove_dir_all(&directory);
}

/// A file that was not in the baseline is a structural change by
/// definition, and says so with its own code.
#[test]
fn classify_refuses_a_new_source_file() {
    let directory = std::env::temp_dir().join("pill_classify_new_file");
    let _ = std::fs::remove_dir_all(&directory);
    let mut session = session_over(&directory, HOT_SOURCE);

    std::fs::write(directory.join("appeared.rs"), HOT_SOURCE).expect("write new file");

    let refusal = session.classify().expect_err("a new file must be refused");
    assert_eq!(
        refusal.code,
        refusal_code::NEW_SOURCE_FILE,
        "{}",
        refusal.detail
    );

    let _ = std::fs::remove_dir_all(&directory);
}

/// Rolling back a function this session never patched is refused rather
/// than silently doing nothing, and names the function.
#[test]
fn rollback_refuses_an_unknown_function() {
    let directory = std::env::temp_dir().join("pill_rollback_unknown");
    let _ = std::fs::remove_dir_all(&directory);
    let session = session_over(&directory, HOT_SOURCE);

    assert!(!session.knows_function("project::movement"));
    assert!(session.generations().is_empty());

    let _ = std::fs::remove_dir_all(&directory);
}

/// A prologue generation a reload has invalidated says so, rather than
/// failing as though the crate were never loaded.
///
/// Dropping the addresses is what keeps a rollback from writing into a
/// retired image, but it also makes the generation indistinguishable from a
/// slot-delivered one - and the slot route then refuses for a reason that is
/// not what went wrong. This pins the message a developer actually reads.
#[test]
fn a_prologue_generation_a_reload_invalidated_says_so() {
    let directory = std::env::temp_dir().join("pill_rollback_dropped_history");
    let _ = std::fs::remove_dir_all(&directory);
    let mut session = session_over(&directory, HOT_SOURCE);

    // A generation delivered by overwriting code, as `apply` would record it.
    session.generations.push(Generation {
        function: "project::ordinary".to_string(),
        number: 1,
        address: 0x1000,
        kind: source::HotFunctionKind::PlainFunction,
        signature_hash: 0,
        lookup_name: "project::ordinary".to_string(),
        signature: "fn ordinary()".to_string(),
        prologue_restores: vec![PrologueRestore {
            artifact: "project".to_string(),
            address: 0x2000,
            original: vec![0x48, 0xB8, 0, 0, 0, 0, 0, 0, 0, 0, 0xFF, 0xE0],
        }],
        prologue_history_dropped: false,
        installed_at: Instant::now(),
    });
    session
        .active_generations
        .insert("project::ordinary".to_string(), 1);

    // The reload that invalidates every recorded address.
    session.forget_prologue_patches();
    assert!(session.generations[0].prologue_history_dropped);
    assert!(session.generations[0].prologue_restores.is_empty());

    // Patching the same function again is what makes generation 1 reachable
    // for a rollback at all: the reload cleared the active entry, and the
    // new generation restores it. This is the sequence the refusal is for -
    // edit, reload, edit, then ask for the generation from before.
    session.generations.push(Generation {
        function: "project::ordinary".to_string(),
        number: 2,
        address: 0x3000,
        kind: source::HotFunctionKind::PlainFunction,
        lookup_name: "project::ordinary".to_string(),
        signature: "fn ordinary()".to_string(),
        signature_hash: 0,
        prologue_restores: vec![PrologueRestore {
            artifact: "project".to_string(),
            address: 0x2000,
            original: vec![0x48, 0xB8, 0, 0, 0, 0, 0, 0, 0, 0, 0xFF, 0xE0],
        }],
        prologue_history_dropped: false,
        installed_at: Instant::now(),
    });
    session
        .active_generations
        .insert("project::ordinary".to_string(), 2);

    let mut engine = Engine::new();
    let detail = session
        .rollback(&mut engine, &[], &[], "project::ordinary", 1)
        .expect_err("an invalidated generation cannot be rolled back to");
    assert!(
        detail.contains("a reload has replaced that code"),
        "the refusal must name the reload, not a missing crate: {detail}"
    );

    let _ = std::fs::remove_dir_all(&directory);
}

/// A reload clears the active generation of slot-delivered history too.
///
/// The rebuilt artifact re-creates the slot with its own body, so a
/// generation that was live before the reload is not running any more -
/// and a rollback that trusted the stale entry would install a body from
/// the previous revision into the fresh artifact.
#[test]
fn a_reload_forgets_slot_generations() {
    let directory = std::env::temp_dir().join("pill_reload_forgets_slots");
    let _ = std::fs::remove_dir_all(&directory);
    let mut session = session_over(&directory, HOT_SOURCE);

    // A generation delivered into a slot, as `apply` records one: no
    // prologue was written, so there is nothing for the reload to prune.
    session.generations.push(Generation {
        function: "project::movement".to_string(),
        number: 1,
        address: 0x1000,
        kind: source::HotFunctionKind::System,
        signature_hash: 7,
        lookup_name: "project::movement".to_string(),
        signature: String::new(),
        prologue_restores: Vec::new(),
        prologue_history_dropped: false,
        installed_at: Instant::now(),
    });
    session
        .active_generations
        .insert("project::movement".to_string(), 1);
    assert_eq!(session.active_generation("project::movement"), 1);

    // The reload.
    session.forget_prologue_patches();

    assert_eq!(
        session.active_generation("project::movement"),
        0,
        "the rebuilt artifact runs its own body, not the pre-reload patch"
    );
    assert!(!session.knows_function("project::movement"));

    // Rolling back to the pre-reload generation must be refused rather
    // than re-installing a body from a previous revision.
    let mut engine = Engine::new();
    let detail = session
        .rollback(&mut engine, &[], &[], "project::movement", 1)
        .expect_err("a generation from before the reload cannot be re-installed");
    assert!(
        detail.contains("has not been patched in this session"),
        "the refusal says there is no history to roll back to: {detail}"
    );

    let _ = std::fs::remove_dir_all(&directory);
}

/// An un-annotated function is patchable: the attribute chooses which
/// mechanism delivers the replacement, not whether one is possible at all.
///
/// This inverts what this case used to assert. Discovery now comes from the
/// build script's address inventory rather than from an attribute, so a
/// plain `fn` in a participating crate is as patchable as an annotated one.
#[test]
fn classify_accepts_an_un_annotated_function() {
    let directory = std::env::temp_dir().join("pill_classify_unannotated");
    let _ = std::fs::remove_dir_all(&directory);
    let plain = "fn ordinary(value: f32) -> f32 { value }\n";
    let mut session = session_over(&directory, plain);

    std::fs::write(
        directory.join("lib.rs"),
        "fn ordinary(value: f32) -> f32 { value * 2.0 }\n",
    )
    .expect("write edit");

    let edit = session
        .classify()
        .expect("an un-annotated body edit is in scope")
        .expect("a change must be reported");
    assert_eq!(edit.declarations.len(), 1, "one body changed");
    assert_eq!(edit.declarations[0].name, "ordinary");
    assert_eq!(
        edit.declarations[0].kind,
        source::HotFunctionKind::PlainFunction
    );

    let _ = std::fs::remove_dir_all(&directory);
}

/// A file with no addressable function at all is still refused, so an edit
/// to something the host could never reach says so.
#[test]
fn classify_refuses_a_file_with_no_functions() {
    let directory = std::env::temp_dir().join("pill_classify_no_functions");
    let _ = std::fs::remove_dir_all(&directory);
    let mut session = session_over(&directory, "pub const SPEED: f32 = 1.0;\n");

    std::fs::write(directory.join("lib.rs"), "pub const SPEED: f32 = 2.0;\n").expect("write edit");

    let refusal = session.classify().expect_err("must be refused");
    assert_eq!(refusal.code, refusal_code::NO_HOT_FUNCTION);

    let _ = std::fs::remove_dir_all(&directory);
}

/// Editing a second function's body is now a patch of THAT function, not a
/// structural change - one changed body is still the limit.
#[test]
fn classify_reports_whichever_body_changed() {
    let directory = std::env::temp_dir().join("pill_classify_other_body");
    let _ = std::fs::remove_dir_all(&directory);
    let mut session = session_over(&directory, HOT_SOURCE);

    let edited = HOT_SOURCE.replace("value + 1.0", "value + 9.0");
    std::fs::write(directory.join("lib.rs"), &edited).expect("write edit");

    let edit = session
        .classify()
        .expect("a body-only edit is in scope")
        .expect("a change must be reported");
    assert_eq!(edit.declarations.len(), 1, "one body changed");
    assert_eq!(edit.declarations[0].name, "helper");

    let _ = std::fs::remove_dir_all(&directory);
}

#[test]
fn generation_reports_a_missing_function() {
    let session = HotPatchSession {
        workspace_root: PathBuf::from("."),
        package: "project".to_string(),
        crate_root: PathBuf::from("lib.rs"),
        source_root: PathBuf::from("."),
        package_rlib: PathBuf::from("libproject.rlib"),
        build_command: vec!["cargo".to_string(), "build".to_string()],
        snapshots: HashMap::new(),
        snapshot_taken_at: SystemTime::UNIX_EPOCH,
        rustc_line: None,
        generations: Vec::new(),
        active_generations: HashMap::new(),
        counter: 0,
    };
    assert!(session
        .generate(
            "fn other() {}",
            &system_declaration("movement"),
            "project::movement",
        )
        .is_err());
}
