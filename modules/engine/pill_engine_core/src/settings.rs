//! Manager settings: what a project configures on a subsystem-owning resource.
//!
//! # Responsibilities
//!
//! - Parses a manager's settings document (`key: value` lines) into a
//!   [`SettingsDocument`] a manager reads typed values from.
//! - Declares [`ManagerResource`]: the settings name, the complete default
//!   document, and the applier that moves a document into the live value.
//! - Carries the applier as plain data ([`ManagerSettingsOps`]) so it can live
//!   in the per-type table that a reload re-points at the newest image.
//!
//! # Design
//!
//! A manager is a [`Resource`] that owns a subsystem - the asset store, the
//! audio device, a physics world. A project configures it through one file,
//! `settings/<settings_name>_settings.yaml`, which the development host creates
//! from [`ManagerResource::default_settings`] when it is missing, applies after
//! a load, and re-applies when it changes.
//!
//! The document is deliberately flat: `key: value` lines, `#` comments and
//! blank lines. The engine has no YAML dependency (a manifest change is a gate
//! of its own), and flat keys are all a subsystem's settings need; the same
//! shape the project's `logging:` section is read in
//! (`pill_player`'s `LoggingSettings::from_text`). Nesting is a later addition,
//! and a document that needs it can name separate keys.
//!
//! The applier is a function pointer into the artifact that registered the
//! manager. It therefore has the same lifetime rule as the drop function it sits
//! beside: valid while that image is mapped, and refreshed by the world's
//! re-homing pass together with the rest of the table. A manager that has to
//! survive a reload needs a `shared_name`, because an id built from `TypeId`
//! differs in every generation.

// Standard library
use std::fmt;

// Current crate
use crate::resource::Resource;

// =============================================================================
// Constants
// =============================================================================

/// The characters a settings key may consist of.
///
/// Letters, digits, `_` and `.` cover `gravity`, `max_steps_per_frame` and the
/// dotted form a later nested document would use. Anything else - a space, a
/// quote, a path separator - is refused rather than passed to a manager.
fn is_key_character(character: char) -> bool {
    character.is_ascii_alphanumeric() || character == '_' || character == '.'
}

// =============================================================================
// SettingsError
// =============================================================================

/// Why a settings document could not be read.
///
/// The line number is the whole point: the person is looking at a file they
/// just edited, and a message without a line sends them hunting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingsError {
    /// One-based line the problem was found on.
    line: usize,
    /// What is wrong with it, in the document's own vocabulary.
    message: String,
}

impl SettingsError {
    /// Builds an error for one line.
    fn new(line: usize, message: impl Into<String>) -> Self {
        Self {
            line,
            message: message.into(),
        }
    }

    /// The one-based line the problem was found on.
    pub fn line(&self) -> usize {
        self.line
    }

    /// What is wrong with that line.
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for SettingsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "line {}: {}", self.line, self.message)
    }
}

impl std::error::Error for SettingsError {}

// =============================================================================
// SettingsValue
// =============================================================================

/// One value a settings document carries.
#[derive(Debug, Clone, PartialEq)]
pub enum SettingsValue {
    /// `true` or `false`.
    Bool(bool),
    /// A whole number, without a decimal point.
    Integer(i64),
    /// A number with a decimal point or an exponent.
    Float(f64),
    /// Anything else, with a matching pair of surrounding quotes removed.
    Text(String),
}

impl SettingsValue {
    /// Reads one value: the first of `true`/`false`, a whole number, a decimal
    /// number, or the text itself.
    fn parse(raw_value: &str) -> Self {
        match raw_value {
            "true" => return Self::Bool(true),
            "false" => return Self::Bool(false),
            _ => {}
        }
        // A whole number first, so `2` stays an integer and `2.0` becomes a
        // float; a manager asks for the one its field is.
        if let Ok(integer) = raw_value.parse::<i64>() {
            return Self::Integer(integer);
        }
        if raw_value.contains(['.', 'e', 'E']) {
            if let Ok(float) = raw_value.parse::<f64>() {
                return Self::Float(float);
            }
        }
        Self::Text(strip_quotes(raw_value).to_owned())
    }

    /// This value as a boolean.
    ///
    /// Only a boolean reads as one: `1` and `"true"` do not, so a mistake in
    /// the file is a refusal rather than a silent interpretation.
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Bool(value) => Some(*value),
            _ => None,
        }
    }

    /// This value as a whole number. A float reads as one only when it has no
    /// fractional part.
    pub fn as_integer(&self) -> Option<i64> {
        match self {
            Self::Integer(value) => Some(*value),
            Self::Float(value) if value.fract() == 0.0 => Some(*value as i64),
            _ => None,
        }
    }

    /// This value as a number. A whole number widens.
    pub fn as_float(&self) -> Option<f64> {
        match self {
            Self::Float(value) => Some(*value),
            Self::Integer(value) => Some(*value as f64),
            _ => None,
        }
    }

    /// This value as text.
    pub fn as_text(&self) -> Option<&str> {
        match self {
            Self::Text(value) => Some(value),
            _ => None,
        }
    }
}

/// `raw_value` without one matching pair of surrounding quotes.
fn strip_quotes(raw_value: &str) -> &str {
    let bytes = raw_value.as_bytes();
    if bytes.len() >= 2 {
        let first = bytes[0];
        let last = bytes[bytes.len() - 1];
        if (first == b'"' || first == b'\'') && first == last {
            return &raw_value[1..raw_value.len() - 1];
        }
    }
    raw_value
}

// =============================================================================
// SettingsDocument
// =============================================================================

/// A parsed settings document: the `key: value` pairs of one file.
///
/// The document is read-only data owned by whoever parsed it. A manager reads
/// values out of it and must not keep it: it lives in the caller's allocation,
/// and the caller is a different DLL from the manager.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SettingsDocument {
    /// The entries, in the order the file wrote them.
    entries: Vec<(String, SettingsValue)>,
}

impl SettingsDocument {
    /// Parses a document.
    ///
    /// Every non-blank line that does not start with `#` must be `key: value`.
    /// A key is letters, digits, `_` and `.`; it may appear once.
    ///
    /// # Errors
    ///
    /// Returns a [`SettingsError`] naming the line when a line is not
    /// `key: value`, a key is empty or has a character outside the allowed set,
    /// or a key appears twice.
    pub fn parse(text: &str) -> Result<Self, SettingsError> {
        let mut entries: Vec<(String, SettingsValue)> = Vec::new();
        for (index, raw_line) in text.lines().enumerate() {
            let line_number = index + 1;
            let line = raw_line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((raw_key, raw_value)) = line.split_once(':') else {
                return Err(SettingsError::new(
                    line_number,
                    format!("`{line}` is not `key: value`"),
                ));
            };
            let key = raw_key.trim();
            if key.is_empty() {
                return Err(SettingsError::new(line_number, "the key is empty"));
            }
            if !key.chars().all(is_key_character) {
                return Err(SettingsError::new(
                    line_number,
                    format!("`{key}` may only hold letters, digits, `_` and `.`"),
                ));
            }
            if entries.iter().any(|(existing, _)| existing == key) {
                return Err(SettingsError::new(
                    line_number,
                    format!("`{key}` appears more than once"),
                ));
            }
            entries.push((key.to_owned(), SettingsValue::parse(raw_value.trim())));
        }
        Ok(Self { entries })
    }

    /// The value of `key`, or `None` when the document does not name it.
    pub fn get(&self, key: &str) -> Option<&SettingsValue> {
        self.entries
            .iter()
            .find(|(existing, _)| existing == key)
            .map(|(_, value)| value)
    }

    /// Whether the document names `key`.
    pub fn contains(&self, key: &str) -> bool {
        self.get(key).is_some()
    }

    /// The boolean at `key`.
    pub fn bool(&self, key: &str) -> Option<bool> {
        self.get(key).and_then(SettingsValue::as_bool)
    }

    /// The whole number at `key`.
    pub fn integer(&self, key: &str) -> Option<i64> {
        self.get(key).and_then(SettingsValue::as_integer)
    }

    /// The number at `key`.
    pub fn float(&self, key: &str) -> Option<f64> {
        self.get(key).and_then(SettingsValue::as_float)
    }

    /// The text at `key`.
    pub fn text(&self, key: &str) -> Option<&str> {
        self.get(key).and_then(SettingsValue::as_text)
    }

    /// Every key the document names, in file order.
    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().map(|(key, _)| key.as_str())
    }

    /// How many keys the document names.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the document names no keys at all.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The keys the document names that are not in `known`, in file order.
    ///
    /// A manager uses this to refuse a document with a misspelled key instead
    /// of silently ignoring it, which is the failure a person cannot see.
    pub fn unknown_keys<'a>(&'a self, known: &[&str]) -> Vec<&'a str> {
        self.keys().filter(|key| !known.contains(key)).collect()
    }
}

// =============================================================================
// SettingsOutcome
// =============================================================================

/// What a manager did with a document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsOutcome {
    /// The document is in effect.
    Applied,
    /// The manager refused it, kept what it had, and logged why.
    Refused,
}

// =============================================================================
// ManagerResource
// =============================================================================

/// A resource that owns a subsystem and can be configured from the project.
///
/// Implement it for a manager a project can meaningfully configure - the audio
/// device, a physics world - and register the value through
/// `World::insert_manager_resource` or `World::register_manager_resource`. The
/// development host then writes `settings/<settings_name>_settings.yaml` from
/// [`Self::default_settings`] when the project has none, applies it after a
/// load, and applies it again when the file changes.
///
/// A manager with nothing to configure does not implement the trait: an empty
/// document in every project would be a file nobody can act on.
pub trait ManagerResource: Resource {
    /// The file stem this manager's settings live under: `settings/<name>_settings.yaml`.
    ///
    /// Letters, digits and `_` only, so the name is safe as a file name. The
    /// module's own name is the convention (`pill_audio`, `pill_physics`),
    /// because a person looking at the folder knows which module a file
    /// belongs to.
    fn settings_name() -> &'static str;

    /// The complete default document: every key the applier reads, with the
    /// value the manager starts with.
    ///
    /// Written out as text rather than serialized from a `Default` impl,
    /// because the engine has no YAML dependency and because a person editing
    /// the file should see every key with the value it takes when untouched.
    /// [`Self::apply_settings`] must accept this document unchanged and leave
    /// the manager exactly as it starts.
    fn default_settings() -> &'static str;

    /// Applies a parsed document to the live value.
    ///
    /// Return [`SettingsOutcome::Refused`] when the document cannot be used -
    /// an unknown key, a value out of range, a setting that cannot change
    /// while the subsystem runs. A refusal must leave the manager exactly as it
    /// was, and the manager logs the reason itself. A manager applies its own
    /// state only: it is handed no world, so a setting that would need another
    /// resource (an asset handle to resolve) does not belong here.
    fn apply_settings(&mut self, settings: &SettingsDocument) -> SettingsOutcome;
}

// =============================================================================
// ManagerSettingsOps
// =============================================================================

/// A manager's settings surface as plain data, stored in its per-type table.
///
/// The applier belongs to the artifact that registered the manager, so it
/// follows the same lifetime rule as the drop function beside it: valid while
/// that image is mapped, and replaced together with the rest of the table when
/// the world re-homes a resource.
#[derive(Clone, Copy)]
pub struct ManagerSettingsOps {
    /// The file stem, from [`ManagerResource::settings_name`].
    pub settings_name: &'static str,
    /// The complete default document, from [`ManagerResource::default_settings`].
    pub default_document: &'static str,
    /// Applies a document to the value at the first argument.
    ///
    /// # Safety
    ///
    /// The pointer must point at a live, correctly aligned value of the type
    /// this table was built for, and the document must outlive the call.
    pub apply: unsafe fn(*mut u8, &SettingsDocument) -> SettingsOutcome,
}

impl ManagerSettingsOps {
    /// Builds the settings surface of a manager type.
    pub fn of<T: ManagerResource>() -> Self {
        Self {
            settings_name: T::settings_name(),
            default_document: T::default_settings(),
            apply: apply_manager_settings_of::<T>,
        }
    }
}

/// Apply a document to the `T` at `value_pointer`.
///
/// # Safety
///
/// `value_pointer` must point at a live, correctly aligned `T`.
unsafe fn apply_manager_settings_of<T: ManagerResource>(
    value_pointer: *mut u8,
    settings: &SettingsDocument,
) -> SettingsOutcome {
    // SAFETY: guaranteed by the caller; the table and the box it describes
    // come from the same `insert_manager_resource` call.
    let manager = unsafe { &mut *value_pointer.cast::<T>() };
    manager.apply_settings(settings)
}

// =============================================================================
// ManagerSettingsEntry
// =============================================================================

/// A manager's settings surface as owned strings, copied out of its table.
///
/// The table holds borrowed names and documents that live in the artifact that
/// registered the manager. The host keeps the answer past that artifact's
/// retirement, so it takes this copy while the image is still mapped.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ManagerSettingsEntry {
    /// Full path of the declaring type, which is what a log line names.
    pub declaring_type_name: String,
    /// The file stem the manager's settings live under.
    pub settings_name: String,
    /// The complete default document.
    pub default_document: String,
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    // Current crate
    use super::*;
    use crate::resource::ResourceId;

    /// Every shape a value takes is read as what it looks like.
    #[test]
    fn values_are_read_as_the_type_they_look_like() {
        let document = SettingsDocument::parse(
            "# a comment\n\
             enabled: true\n\
             steps: 4\n\
             gain: 2.5\n\
             label: \"witness\"\n\
             plain: a text value\n",
        )
        .expect("the document reads");

        assert_eq!(document.bool("enabled"), Some(true));
        assert_eq!(document.integer("steps"), Some(4));
        assert_eq!(document.float("gain"), Some(2.5));
        assert_eq!(document.text("label"), Some("witness"));
        assert_eq!(document.text("plain"), Some("a text value"));
        assert_eq!(document.len(), 5);
    }

    /// A whole number reads as a float too, and neither crosses to the other
    /// type by accident.
    #[test]
    fn numbers_widen_but_do_not_become_booleans() {
        let document =
            SettingsDocument::parse("count: 3\nratio: 4.0\nflag: 1\n").expect("the document reads");

        assert_eq!(document.float("count"), Some(3.0));
        assert_eq!(document.integer("ratio"), Some(4));
        assert_eq!(document.bool("flag"), None, "`1` is not a boolean");
    }

    /// A malformed line, a bad key and a repeated key are refused with the
    /// line they are on.
    #[test]
    fn a_malformed_document_names_the_line_it_fails_on() {
        let missing_colon = SettingsDocument::parse("ok: 1\nnot a pair\n").unwrap_err();
        assert_eq!(missing_colon.line(), 2);
        assert!(missing_colon.to_string().contains("key: value"));

        let bad_key = SettingsDocument::parse("ok: 1\nbad key: 2\n").unwrap_err();
        assert_eq!(bad_key.line(), 2);
        assert!(bad_key.to_string().contains("bad key"));

        let repeated = SettingsDocument::parse("ok: 1\nok: 2\n").unwrap_err();
        assert_eq!(repeated.line(), 2);
        assert!(repeated.to_string().contains("more than once"));
    }

    /// A key the manager does not know is reported, which is how a misspelling
    /// becomes a refusal instead of a silent no-op.
    #[test]
    fn unknown_keys_are_reported_in_file_order() {
        let document = SettingsDocument::parse("gain: 1\nvolume: 2\nlabel: three\n")
            .expect("the document reads");

        assert_eq!(document.unknown_keys(&["gain", "label"]), vec!["volume"]);
    }

    /// The empty document is valid and names nothing.
    #[test]
    fn an_empty_document_is_valid() {
        let document = SettingsDocument::parse("# only a comment\n\n").expect("the document reads");

        assert!(document.is_empty());
        assert_eq!(document.unknown_keys(&["gain"]), Vec::<&str>::new());
    }

    /// A manager the tests register: two knobs, and a refusal for any key it
    /// does not know.
    #[derive(Debug, PartialEq)]
    struct TestManager {
        /// A number the applier reads.
        gain: f32,
        /// A text value the applier reads.
        label: String,
    }

    impl TestManager {
        /// The values [`ManagerResource::default_settings`] describes.
        fn new() -> Self {
            Self {
                gain: 1.0,
                label: "initial".to_owned(),
            }
        }
    }

    impl Resource for TestManager {
        /// A shared name, so the type keeps one world id across a reload - the
        /// rule every kept manager follows.
        fn shared_name() -> Option<&'static str> {
            Some("pill_engine_core::settings::tests::TestManager")
        }
    }

    impl ManagerResource for TestManager {
        fn settings_name() -> &'static str {
            "test_manager"
        }

        fn default_settings() -> &'static str {
            "# Every key, with the value it takes when untouched.\ngain: 1.0\nlabel: initial\n"
        }

        fn apply_settings(&mut self, settings: &SettingsDocument) -> SettingsOutcome {
            if !settings.unknown_keys(&["gain", "label"]).is_empty() {
                return SettingsOutcome::Refused;
            }
            // A key the document leaves out keeps the value the manager has.
            if let Some(gain) = settings.float("gain") {
                self.gain = gain as f32;
            }
            if let Some(label) = settings.text("label") {
                self.label = label.to_owned();
            }
            SettingsOutcome::Applied
        }
    }

    /// A manager is found by its registration, named by its declaration, and
    /// applies a document it accepts.
    #[test]
    fn a_manager_is_listed_named_and_applied() {
        let mut world = crate::world::World::new();
        world.insert_manager_resource(TestManager::new());
        let id = ResourceId::of::<TestManager>();

        assert_eq!(world.manager_resource_ids(), vec![id]);
        let entry = world.manager_settings(id).expect("the manager is declared");
        assert_eq!(entry.settings_name, "test_manager");
        assert_eq!(entry.default_document, TestManager::default_settings());

        // The default document applies unchanged to a fresh manager.
        let defaults = SettingsDocument::parse(TestManager::default_settings())
            .expect("the default document is valid");
        assert_eq!(
            world.apply_manager_settings(id, &defaults),
            Some(SettingsOutcome::Applied)
        );
        assert_eq!(
            world.get_resource::<TestManager>(),
            Some(&TestManager::new())
        );

        let edited = SettingsDocument::parse("gain: 4.5\nlabel: edited\n").expect("it reads");
        assert_eq!(
            world.apply_manager_settings(id, &edited),
            Some(SettingsOutcome::Applied)
        );
        let manager = world.get_resource::<TestManager>().expect("it is there");
        assert_eq!(manager.gain, 4.5);
        assert_eq!(manager.label, "edited");
    }

    /// A refused document leaves every value as it was.
    #[test]
    fn a_refused_document_leaves_the_manager_alone() {
        let mut world = crate::world::World::new();
        world.insert_manager_resource(TestManager::new());
        let id = ResourceId::of::<TestManager>();

        let misspelled = SettingsDocument::parse("gain: 4.5\ngian: 2\n").expect("it reads");

        assert_eq!(
            world.apply_manager_settings(id, &misspelled),
            Some(SettingsOutcome::Refused)
        );
        assert_eq!(
            world.get_resource::<TestManager>(),
            Some(&TestManager::new())
        );
    }

    /// Re-registering the type - what a reloaded generation does - refreshes
    /// the table the applier comes from, and the kept value stays appliable.
    #[test]
    fn a_re_registered_manager_still_applies_through_its_table() {
        let mut world = crate::world::World::new();
        world.insert_manager_resource(TestManager::new());

        // Stands in for the next generation declaring the type again.
        world.register_manager_resource::<TestManager>();
        world.rehome_resources();

        let edited = SettingsDocument::parse("gain: 2.5\n").expect("it reads");
        let id = ResourceId::of::<TestManager>();
        assert_eq!(
            world.apply_manager_settings(id, &edited),
            Some(SettingsOutcome::Applied)
        );
        assert_eq!(
            world
                .get_resource::<TestManager>()
                .expect("the value was kept")
                .gain,
            2.5
        );
    }

    /// A resource that is not a manager is invisible to the settings pipeline.
    #[test]
    fn a_plain_resource_is_not_a_manager() {
        let mut world = crate::world::World::new();
        world.insert_resource(PlainResource);

        assert!(world.manager_resource_ids().is_empty());
        assert_eq!(
            world.manager_settings(ResourceId::of::<PlainResource>()),
            None
        );
    }

    /// A resource with nothing to configure.
    #[derive(Debug)]
    struct PlainResource;

    impl Resource for PlainResource {}
}
