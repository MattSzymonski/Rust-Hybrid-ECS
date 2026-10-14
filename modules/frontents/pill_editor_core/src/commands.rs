//! The editor's command registry: named actions, their menus and their
//! rebindable shortcuts (Stage 14, D27/D31).
//!
//! # Responsibilities
//!
//! - Model a command ([`CommandSpec`]: id, title, menu path, per-preset
//!   shortcuts, global flag) and the registry that owns every registration
//!   ([`CommandRegistry`]), grouped by the module that registered it.
//! - Resolve effective shortcuts from the active keymap preset plus the
//!   keybindings file's overrides, and report conflicts by command name.
//! - Parse and format a chord as the `ctrl+shift+p` text both files use
//!   ([`Chord`], [`KeyName`]).
//! - Read the keybindings and preferences files ([`KeybindingsFile`],
//!   [`EditorPreferences`]) and hand back problems to report once.
//!
//! # Design
//!
//! The registry is plain data in a world resource, because three DLLs write
//! and read it: the executable (applies the batch the shell queued), the
//! reloadable shell (dispatches and paints menus) and every editor crate
//! (registers its own commands from `register`). A module re-registering its
//! owner replaces the previous generation's set, so a reload never strands
//! duplicate commands. Chords are this crate's own serde types rather than
//! the engine's input types, so the files stay stable when the engine's key
//! vocabulary changes; the one conversion lives here.

// Standard library
use std::collections::BTreeMap;
use std::str::FromStr;

// External crates
use serde::{Deserialize, Serialize};

// Current crate
use crate::editor_state::EditorCommand;

// =============================================================================
// Keys and chords
// =============================================================================

/// The key vocabulary command bindings use.
///
/// Deliberately small: every key a preset or a user binding names in this
/// stage, converted to the engine's [`pill_engine::KeyCode`] in one place so
/// the two lists cannot drift silently.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum KeyName {
    A,
    B,
    C,
    D,
    E,
    F,
    G,
    H,
    I,
    J,
    K,
    L,
    M,
    N,
    O,
    P,
    Q,
    R,
    S,
    T,
    U,
    V,
    W,
    X,
    Y,
    Z,
    Space,
    Escape,
    Enter,
    Delete,
    Left,
    Right,
    Up,
    Down,
    F3,
    F5,
    F6,
    F10,
    Numpad1,
    Numpad3,
    Numpad7,
}

impl KeyName {
    /// The engine key this name denotes, when the engine knows it.
    pub fn engine_key(self) -> Option<pill_engine::KeyCode> {
        use pill_engine::KeyCode as K;
        Some(match self {
            Self::A => K::KeyA,
            Self::B => K::KeyB,
            Self::C => K::KeyC,
            Self::D => K::KeyD,
            Self::E => K::KeyE,
            Self::F => K::KeyF,
            Self::G => K::KeyG,
            Self::H => K::KeyH,
            Self::I => K::KeyI,
            Self::J => K::KeyJ,
            Self::K => K::KeyK,
            Self::L => K::KeyL,
            Self::M => K::KeyM,
            Self::N => K::KeyN,
            Self::O => K::KeyO,
            Self::P => K::KeyP,
            Self::Q => K::KeyQ,
            Self::R => K::KeyR,
            Self::S => K::KeyS,
            Self::T => K::KeyT,
            Self::U => K::KeyU,
            Self::V => K::KeyV,
            Self::W => K::KeyW,
            Self::X => K::KeyX,
            Self::Y => K::KeyY,
            Self::Z => K::KeyZ,
            Self::Space => K::Space,
            Self::Escape => K::Escape,
            Self::Enter => K::Enter,
            Self::Delete => K::Delete,
            Self::Left => K::ArrowLeft,
            Self::Right => K::ArrowRight,
            Self::Up => K::ArrowUp,
            Self::Down => K::ArrowDown,
            Self::F3 => K::F3,
            Self::F5 => K::F5,
            Self::F6 => K::F6,
            Self::F10 => K::F10,
            Self::Numpad1 => K::Numpad1,
            Self::Numpad3 => K::Numpad3,
            Self::Numpad7 => K::Numpad7,
        })
    }

    /// The name of the engine key, for building chords from input.
    pub fn from_engine(key: pill_engine::KeyCode) -> Option<Self> {
        use pill_engine::KeyCode as K;
        Some(match key {
            K::KeyA => Self::A,
            K::KeyB => Self::B,
            K::KeyC => Self::C,
            K::KeyD => Self::D,
            K::KeyE => Self::E,
            K::KeyF => Self::F,
            K::KeyG => Self::G,
            K::KeyH => Self::H,
            K::KeyI => Self::I,
            K::KeyJ => Self::J,
            K::KeyK => Self::K,
            K::KeyL => Self::L,
            K::KeyM => Self::M,
            K::KeyN => Self::N,
            K::KeyO => Self::O,
            K::KeyP => Self::P,
            K::KeyQ => Self::Q,
            K::KeyR => Self::R,
            K::KeyS => Self::S,
            K::KeyT => Self::T,
            K::KeyU => Self::U,
            K::KeyV => Self::V,
            K::KeyW => Self::W,
            K::KeyX => Self::X,
            K::KeyY => Self::Y,
            K::KeyZ => Self::Z,
            K::Space => Self::Space,
            K::Escape => Self::Escape,
            K::Enter => Self::Enter,
            K::Delete => Self::Delete,
            K::ArrowLeft => Self::Left,
            K::ArrowRight => Self::Right,
            K::ArrowUp => Self::Up,
            K::ArrowDown => Self::Down,
            K::F3 => Self::F3,
            K::F5 => Self::F5,
            K::F6 => Self::F6,
            K::F10 => Self::F10,
            K::Numpad1 => Self::Numpad1,
            K::Numpad3 => Self::Numpad3,
            K::Numpad7 => Self::Numpad7,
            _ => return None,
        })
    }
}

/// One shortcut: a key plus the modifiers that must be held with it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Chord {
    /// Whether control was held.
    pub ctrl: bool,
    /// Whether shift was held.
    pub shift: bool,
    /// Whether alt was held.
    pub alt: bool,
    /// The key itself.
    pub key: KeyName,
}

impl Chord {
    /// A chord without modifiers.
    pub fn plain(key: KeyName) -> Self {
        Self {
            ctrl: false,
            shift: false,
            alt: false,
            key,
        }
    }

    /// A chord with modifiers.
    pub fn with(key: KeyName, ctrl: bool, shift: bool, alt: bool) -> Self {
        Self {
            ctrl,
            shift,
            alt,
            key,
        }
    }

    /// Whether this chord fires for a key event with the given modifiers.
    pub fn matches(&self, key: pill_engine::KeyCode, modifiers: pill_engine::Modifiers) -> bool {
        KeyName::from_engine(key) == Some(self.key)
            && self.ctrl == modifiers.control
            && self.shift == modifiers.shift
            && self.alt == modifiers.alt
    }
}

impl std::fmt::Display for KeyName {
    /// The lowercase name the files use (`space`, `f10`, `numpad1`).
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}", format!("{self:?}").to_lowercase())
    }
}

impl std::fmt::Display for Chord {
    /// The `ctrl+shift+p` form both files use.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.ctrl {
            write!(formatter, "ctrl+")?;
        }
        if self.shift {
            write!(formatter, "shift+")?;
        }
        if self.alt {
            write!(formatter, "alt+")?;
        }
        write!(formatter, "{}", self.key)
    }
}

/// Why a chord text could not be parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChordParseError(pub String);

impl std::fmt::Display for ChordParseError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}", self.0)
    }
}

impl FromStr for Chord {
    type Err = ChordParseError;

    /// Parses `[ctrl+][shift+][alt+]key`, case-insensitively.
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let mut chord = Chord::plain(KeyName::Escape);
        let mut key_seen = false;
        for part in text.split('+') {
            let part = part.trim();
            match part.to_ascii_lowercase().as_str() {
                "ctrl" | "control" => chord.ctrl = true,
                "shift" => chord.shift = true,
                "alt" => chord.alt = true,
                _ => {
                    chord.key = KeyName::from_str(part).map_err(|_| {
                        ChordParseError(format!("unknown key `{part}` in `{text}`"))
                    })?;
                    key_seen = true;
                }
            }
        }
        if !key_seen {
            return Err(ChordParseError(format!("no key in `{text}`")));
        }
        Ok(chord)
    }
}

impl FromStr for KeyName {
    type Err = ChordParseError;

    /// Parses a key name case-insensitively; `esc`/`del` are aliases.
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let lower = text.to_ascii_lowercase();
        let named = match lower.as_str() {
            "space" => Some(Self::Space),
            "escape" | "esc" => Some(Self::Escape),
            "enter" | "return" => Some(Self::Enter),
            "delete" | "del" => Some(Self::Delete),
            "left" => Some(Self::Left),
            "right" => Some(Self::Right),
            "up" => Some(Self::Up),
            "down" => Some(Self::Down),
            "f3" => Some(Self::F3),
            "f5" => Some(Self::F5),
            "f6" => Some(Self::F6),
            "f10" => Some(Self::F10),
            "numpad1" => Some(Self::Numpad1),
            "numpad3" => Some(Self::Numpad3),
            "numpad7" => Some(Self::Numpad7),
            other => {
                let mut characters = other.chars();
                match (characters.next(), characters.next()) {
                    (Some(letter), None) if letter.is_ascii_lowercase() => {
                        LETTERS.get(letter as usize - 'a' as usize).copied()
                    }
                    _ => None,
                }
            }
        };
        named.ok_or_else(|| ChordParseError(format!("unknown key `{text}`")))
    }
}

/// The letters, in order, for the single-letter key parse.
const LETTERS: [KeyName; 26] = [
    KeyName::A,
    KeyName::B,
    KeyName::C,
    KeyName::D,
    KeyName::E,
    KeyName::F,
    KeyName::G,
    KeyName::H,
    KeyName::I,
    KeyName::J,
    KeyName::K,
    KeyName::L,
    KeyName::M,
    KeyName::N,
    KeyName::O,
    KeyName::P,
    KeyName::Q,
    KeyName::R,
    KeyName::S,
    KeyName::T,
    KeyName::U,
    KeyName::V,
    KeyName::W,
    KeyName::X,
    KeyName::Y,
    KeyName::Z,
];

// =============================================================================
// Presets
// =============================================================================

/// The keymap presets the editor ships (D27).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum KeymapPreset {
    /// The default: Blender-like.
    #[default]
    Blender,
    /// Unity-like.
    Unity,
    /// Godot-like.
    Godot,
    /// Unreal-Engine-like.
    Unreal,
}

impl std::fmt::Display for KeymapPreset {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            Self::Blender => "blender",
            Self::Unity => "unity",
            Self::Godot => "godot",
            Self::Unreal => "unreal",
        };
        write!(formatter, "{name}")
    }
}

// =============================================================================
// Commands
// =============================================================================

/// One registered command.
#[derive(Clone, Debug, PartialEq)]
pub struct CommandSpec {
    /// Stable identifier (`editor.pause_toggle`).
    pub id: String,
    /// The title menus and the palette show.
    pub title: String,
    /// Menu path (`["View", "Pause Simulation"]`); empty means palette-only.
    pub menu_path: Vec<String>,
    /// The default binding per preset; a preset that is absent leaves the
    /// command unbound in it, as the plan requires.
    pub shortcuts: BTreeMap<KeymapPreset, Vec<Chord>>,
    /// Whether the shortcut wins while a text field has focus (a global
    /// binding); everything else yields to the focused field.
    pub global: bool,
}

impl CommandSpec {
    /// A command with a title and no menu or bindings yet.
    pub fn new(id: &str, title: &str) -> Self {
        Self {
            id: id.to_string(),
            title: title.to_string(),
            menu_path: Vec::new(),
            shortcuts: BTreeMap::new(),
            global: false,
        }
    }

    /// Adds a menu path.
    pub fn menu(mut self, path: &[&str]) -> Self {
        self.menu_path = path.iter().map(|segment| segment.to_string()).collect();
        self
    }

    /// Adds a binding for one preset.
    pub fn bind(mut self, preset: KeymapPreset, chord: &str) -> Self {
        let chord = chord
            .parse::<Chord>()
            .unwrap_or_else(|error| panic!("built-in chord `{chord}` is invalid: {error}"));
        self.shortcuts.entry(preset).or_default().push(chord);
        self
    }

    /// Marks the shortcut as global (it wins over a focused text field).
    pub fn global(mut self) -> Self {
        self.global = true;
        self
    }
}

/// Every command every owner registered, plus the resolved keybindings.
///
/// A world resource (inserted by [`CommandRegistry::ensure`]); owners are
/// keyed by the module that registered them so a re-registration replaces
/// exactly that owner's set.
#[derive(Clone, Debug, Default)]
pub struct CommandRegistry {
    owners: BTreeMap<String, Vec<CommandSpec>>,
    overrides: BTreeMap<String, Vec<Chord>>,
    preset: KeymapPreset,
}

impl CommandRegistry {
    /// The registry holding the built-in commands, under owner `editor`.
    pub fn with_builtins() -> Self {
        let mut registry = Self {
            owners: BTreeMap::new(),
            overrides: BTreeMap::new(),
            preset: KeymapPreset::default(),
        };
        registry.register("editor", builtin_commands());
        registry
    }

    /// The world's registry, created with the built-ins when absent.
    pub fn ensure(world: &mut pill_engine::World) {
        world.register_resource::<CommandRegistry>();
        if world.get_resource::<CommandRegistry>().is_none() {
            world.insert_resource(Self::with_builtins());
        }
    }

    /// Replaces `owner`'s registrations with `commands`.
    pub fn register(&mut self, owner: &str, commands: Vec<CommandSpec>) {
        self.owners.insert(owner.to_string(), commands);
    }

    /// Every command, built-ins first, then owners in registration order.
    pub fn commands(&self) -> impl Iterator<Item = &CommandSpec> {
        self.owners.values().flatten()
    }

    /// One command by id, across every owner.
    pub fn command(&self, id: &str) -> Option<&CommandSpec> {
        self.commands().find(|spec| spec.id == id)
    }

    /// The active preset.
    pub fn preset(&self) -> KeymapPreset {
        self.preset
    }

    /// The number of resolved overrides (for the status log).
    pub fn override_count(&self) -> usize {
        self.overrides.len()
    }

    /// The chords a command currently answers to: the overrides when the
    /// file names any (an empty list means unbound), else the preset's
    /// defaults.
    pub fn effective_chords(&self, id: &str) -> Vec<Chord> {
        if let Some(chords) = self.overrides.get(id) {
            return chords.clone();
        }
        self.command(id)
            .and_then(|spec| spec.shortcuts.get(&self.preset).cloned())
            .unwrap_or_default()
    }

    /// Applies a keybindings file: its preset (when named), its overrides,
    /// and the problems a caller reports once.
    pub fn apply_file(&mut self, file: &KeybindingsFile) -> Vec<String> {
        let mut problems = Vec::new();
        if let Some(preset) = file.preset {
            self.preset = preset;
        }
        self.overrides.clear();
        for (id, texts) in &file.overrides {
            if self.command(id).is_none() {
                problems.push(format!(
                    "keybindings: override for unknown command `{id}` (kept; it binds once that id exists)"
                ));
            }
            let mut chords = Vec::new();
            for text in texts {
                match text.parse::<Chord>() {
                    Ok(chord) => chords.push(chord),
                    Err(error) => problems.push(format!("keybindings: {error}")),
                }
            }
            self.overrides.insert(id.clone(), chords);
        }
        problems
    }

    /// Whether a chord fires for `key` with `modifiers`, and which command.
    ///
    /// The first command in registration order wins when two share a chord;
    /// [`Self::conflicts`] is what reports the collision.
    pub fn resolve(
        &self,
        key: pill_engine::KeyCode,
        modifiers: pill_engine::Modifiers,
    ) -> Option<&CommandSpec> {
        self.commands().find(|spec| {
            self.effective_chords(&spec.id)
                .iter()
                .any(|chord| chord.matches(key, modifiers))
        })
    }

    /// Every chord bound to more than one command, with the ids involved.
    pub fn conflicts(&self) -> Vec<(Chord, Vec<String>)> {
        let mut by_chord: BTreeMap<Chord, Vec<String>> = BTreeMap::new();
        for spec in self.commands() {
            for chord in self.effective_chords(&spec.id) {
                by_chord.entry(chord).or_default().push(spec.id.clone());
            }
        }
        by_chord
            .into_iter()
            .filter(|(_, ids)| ids.len() > 1)
            .collect()
    }
}

impl pill_engine::Resource for CommandRegistry {}

/// The registry's commands grouped by their top-level menu, in command
/// order; the paths the menu bar paints.
pub fn menu_tree(registry: &CommandRegistry) -> BTreeMap<String, Vec<&CommandSpec>> {
    let mut menus: BTreeMap<String, Vec<&CommandSpec>> = BTreeMap::new();
    for spec in registry.commands() {
        let Some(top) = spec.menu_path.first() else {
            continue;
        };
        menus.entry(top.clone()).or_default().push(spec);
    }
    menus
}

/// The built-in commands every editor has (owner `editor`).
fn builtin_commands() -> Vec<CommandSpec> {
    use KeymapPreset::{Blender, Godot, Unity, Unreal};
    vec![
        CommandSpec::new("editor.pause_toggle", "Pause / Resume Simulation")
            .menu(&["View", "Pause / Resume Simulation"])
            .bind(Blender, "space")
            .bind(Unity, "ctrl+p")
            .bind(Godot, "f5")
            .bind(Unreal, "alt+p"),
        CommandSpec::new("editor.step", "Step One Frame")
            .menu(&["View", "Step One Frame"])
            .bind(Blender, "right")
            .bind(Unity, "f10")
            .bind(Godot, "f6")
            .bind(Unreal, "f10"),
        CommandSpec::new("editor.save_layout", "Save Layout")
            .menu(&["Layout", "Save Layout"])
            .bind(Blender, "ctrl+alt+s")
            .bind(Unity, "ctrl+shift+l")
            .bind(Godot, "ctrl+alt+l")
            .bind(Unreal, "ctrl+shift+l"),
        CommandSpec::new("editor.reset_layout", "Reset Layout")
            .menu(&["Layout", "Reset Layout"])
            .bind(Blender, "ctrl+alt+r")
            .bind(Unity, "alt+r")
            .bind(Godot, "ctrl+alt+r")
            .bind(Unreal, "alt+r"),
        CommandSpec::new("editor.command_palette", "Command Palette")
            .bind(Blender, "f3")
            .bind(Unity, "ctrl+shift+p")
            .bind(Godot, "ctrl+shift+p")
            .bind(Unreal, "ctrl+shift+p")
            .global(),
        CommandSpec::new("editor.undo", "Undo")
            .menu(&["Edit", "Undo"])
            .bind(Blender, "ctrl+z")
            .bind(Unity, "ctrl+z")
            .bind(Godot, "ctrl+z")
            .bind(Unreal, "ctrl+z")
            .global(),
        CommandSpec::new("editor.redo", "Redo")
            .menu(&["Edit", "Redo"])
            .bind(Blender, "ctrl+shift+z")
            .bind(Unity, "ctrl+y")
            .bind(Godot, "ctrl+shift+z")
            .bind(Unreal, "ctrl+y")
            .global(),
        // The clipboard commands stay un-global on purpose: while a widget
        // holds the focus (a text field), ctrl+c/x/v belong to the field's
        // own Edit handling (Stage 18).
        CommandSpec::new("editor.copy", "Copy")
            .menu(&["Edit", "Copy"])
            .bind(Blender, "ctrl+c")
            .bind(Unity, "ctrl+c")
            .bind(Godot, "ctrl+c")
            .bind(Unreal, "ctrl+c"),
        CommandSpec::new("editor.cut", "Cut")
            .menu(&["Edit", "Cut"])
            .bind(Blender, "ctrl+x")
            .bind(Unity, "ctrl+x")
            .bind(Godot, "ctrl+x")
            .bind(Unreal, "ctrl+x"),
        CommandSpec::new("editor.paste", "Paste")
            .menu(&["Edit", "Paste"])
            .bind(Blender, "ctrl+v")
            .bind(Unity, "ctrl+v")
            .bind(Godot, "ctrl+v")
            .bind(Unreal, "ctrl+v"),
        // The transform tools (Stage 20): the chords are the ones Unity and
        // Unreal already use, and Blender's own grab/rotate/scale keys.
        CommandSpec::new("editor.tool_translate", "Translate Tool")
            .menu(&["Tools", "Translate Tool"])
            .bind(Blender, "g")
            .bind(Unity, "w")
            .bind(Godot, "w")
            .bind(Unreal, "w"),
        CommandSpec::new("editor.tool_rotate", "Rotate Tool")
            .menu(&["Tools", "Rotate Tool"])
            .bind(Blender, "r")
            .bind(Unity, "e")
            .bind(Godot, "e")
            .bind(Unreal, "e"),
        CommandSpec::new("editor.tool_scale", "Scale Tool")
            .menu(&["Tools", "Scale Tool"])
            .bind(Blender, "s")
            .bind(Unity, "r")
            .bind(Godot, "r")
            .bind(Unreal, "r"),
        CommandSpec::new("editor.tool_snap_toggle", "Toggle Snapping")
            .menu(&["Tools", "Toggle Snapping"])
            .bind(Blender, "shift+x")
            .bind(Unity, "ctrl+shift+x")
            .bind(Godot, "ctrl+shift+x")
            .bind(Unreal, "ctrl+shift+x"),
        // Unbound on purpose: the space toggle is a menu, palette and
        // toolbar command, and the presets have no free chord that would
        // mean "local" anywhere else either.
        CommandSpec::new("editor.tool_space_toggle", "Toggle Local / World Space")
            .menu(&["Tools", "Toggle Local / World Space"]),
        // The Assets dialogs' buttons (Stage 25). They are registered like
        // any other command - the button row runs them by id - with no menu
        // entry of their own: they answer through the dialog they were
        // pressed on, and invoked without one they warn and do nothing.
        CommandSpec::new("editor.asset.create", "Create Asset"),
        CommandSpec::new("editor.asset.rename", "Rename Asset"),
    ]
}

// =============================================================================
// Files
// =============================================================================

/// The keybindings file: the active preset and single-binding overrides.
///
/// Shape: `{ "preset": "unity", "overrides": { "editor.pause_toggle": ["space"] } }`.
/// A command missing from `overrides` keeps its preset binding; a command
/// listed with an empty array is unbound.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct KeybindingsFile {
    /// The preset to activate; absent keeps the current one.
    #[serde(default)]
    pub preset: Option<KeymapPreset>,
    /// Per-command chord lists replacing the preset's bindings.
    #[serde(default)]
    pub overrides: BTreeMap<String, Vec<String>>,
}

/// The preferences file (D31): everything the editor lets a user persist
/// that is not the layout or the bindings.
///
/// The keymap preset lives in the keybindings file instead (the plan's
/// acceptance reads it there); this file holds the preferences the later
/// stages consume, parsed here so one reader owns the file.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct EditorPreferences {
    /// The UI theme name, when a stylesheet selects one.
    #[serde(default)]
    pub theme: Option<String>,
    /// Transform snapping steps, in world units and degrees.
    #[serde(default)]
    pub snapping_steps: Option<SnappingSteps>,
    /// The gizmo tool's persisted state (Stage 20).
    #[serde(default)]
    pub gizmo: Option<GizmoPreferences>,
    /// The command that opens the user's code editor.
    #[serde(default)]
    pub code_editor_command: Option<String>,
    /// Autosave interval for the editor state, in seconds.
    #[serde(default)]
    pub autosave_interval_seconds: Option<f64>,
    /// Maximum undo steps kept (absent uses the history default, 500).
    #[serde(default)]
    pub history_step_cap: Option<usize>,
    /// Undo memory budget in megabytes (absent uses the default, 256).
    #[serde(default)]
    pub history_memory_budget_mb: Option<usize>,
}

/// Snapping steps for a later stage's transforms.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct SnappingSteps {
    /// Translation step in world units.
    pub translation: f32,
    /// Rotation step in degrees.
    pub rotation_degrees: f32,
    /// Scale step, as a factor increment (Stage 20).
    #[serde(default = "default_scale_step")]
    pub scale: f32,
}

/// The scale step a preferences file without one uses.
fn default_scale_step() -> f32 {
    0.1
}

/// The gizmo tool's persisted state (Stage 20).
///
/// The top-level fields are the defaults; `projects` remembers a choice per
/// project - the plan's "per project" snapping decision - keyed by the
/// project directory's name, so moving between projects brings each one's
/// tool back.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct GizmoPreferences {
    /// The last active mode, when one was chosen.
    #[serde(default)]
    pub mode: Option<crate::gizmo::GizmoMode>,
    /// The last active space, when one was chosen.
    #[serde(default)]
    pub space: Option<crate::gizmo::GizmoSpace>,
    /// Whether dragging snapped, when the toggle was used.
    #[serde(default)]
    pub snap: Option<bool>,
    /// Per-project overrides, by project directory name.
    #[serde(default)]
    pub projects: BTreeMap<String, GizmoProjectPreferences>,
}

/// One project's gizmo choices.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct GizmoProjectPreferences {
    /// The project's last active mode.
    #[serde(default)]
    pub mode: Option<crate::gizmo::GizmoMode>,
    /// The project's last active space.
    #[serde(default)]
    pub space: Option<crate::gizmo::GizmoSpace>,
    /// The project's last snapping toggle.
    #[serde(default)]
    pub snap: Option<bool>,
}

/// The preferences file's name, next to the layout file (D31).
///
/// Declared here rather than in an editor frontend: the executable reads the
/// file at startup and the shell writes it, and both link this crate.
pub const PREFERENCES_FILE: &str = "preferences.json";

/// The gizmo settings a project starts a session with (Stage 20).
///
/// Resolution order: the project's own remembered entry, then the file's
/// last-used values, then the defaults; the snapping steps are the file's,
/// one set for every project (D31). A missing or unreadable file is the
/// defaults, never an error: the tool has to work before anyone edits
/// preferences.
pub fn read_gizmo_settings(
    directory: &std::path::Path,
    project: &str,
) -> crate::gizmo::GizmoSettings {
    let preferences: EditorPreferences = std::fs::read_to_string(directory.join(PREFERENCES_FILE))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default();
    let mut settings = crate::gizmo::GizmoSettings::default();
    if let Some(steps) = preferences.snapping_steps {
        settings.translate_step = steps.translation;
        settings.rotate_step_degrees = steps.rotation_degrees;
        settings.scale_step = steps.scale;
    }
    let Some(gizmo) = &preferences.gizmo else {
        return settings;
    };
    if let Some(mode) = gizmo.mode {
        settings.mode = mode;
    }
    if let Some(space) = gizmo.space {
        settings.space = space;
    }
    if let Some(snap) = gizmo.snap {
        settings.snap = snap;
    }
    if let Some(project) = gizmo.projects.get(project) {
        if let Some(mode) = project.mode {
            settings.mode = mode;
        }
        if let Some(space) = project.space {
            settings.space = space;
        }
        if let Some(snap) = project.snap {
            settings.snap = snap;
        }
    }
    settings
}

/// Remember one project's gizmo choices in the preferences file (Stage 20).
///
/// Read-modify-write, so every other key survives; the top-level fields are
/// the last-used values a shell toggles against, and the per-project entry
/// is what the next session on this project starts with. Written through a
/// temporary file and a rename, so the live reload reader never observes a
/// half-written document.
///
/// # Errors
///
/// A message when the file cannot be read as JSON, serialized, or replaced.
pub fn write_gizmo_preferences(
    directory: &std::path::Path,
    project: &str,
    settings: &crate::gizmo::GizmoSettings,
) -> Result<(), String> {
    let path = directory.join(PREFERENCES_FILE);
    let mut document = std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default();
    let mut gizmo = document
        .get("gizmo")
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default();
    gizmo.insert("mode".to_string(), serde_json::json!(settings.mode));
    gizmo.insert("space".to_string(), serde_json::json!(settings.space));
    gizmo.insert("snap".to_string(), serde_json::json!(settings.snap));
    let mut projects = gizmo
        .get("projects")
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default();
    projects.insert(
        project.to_string(),
        serde_json::json!({
            "mode": settings.mode,
            "space": settings.space,
            "snap": settings.snap,
        }),
    );
    gizmo.insert("projects".to_string(), serde_json::Value::Object(projects));
    document.insert("gizmo".to_string(), serde_json::Value::Object(gizmo));
    let text = serde_json::to_string_pretty(&serde_json::Value::Object(document))
        .map_err(|error| error.to_string())?;
    // The directory belongs to the layout, which may never have been saved
    // in this profile: the write creates it rather than assuming a save
    // came first.
    std::fs::create_dir_all(directory).map_err(|error| error.to_string())?;
    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, text).map_err(|error| error.to_string())?;
    std::fs::rename(&temporary, &path).map_err(|error| error.to_string())
}

/// The world resource carrying commands executed this frame to their owners.
///
/// The shell pushes every executed command id here after dispatch; a module
/// owns the ids it registered and drains them in a system (the shell cannot
/// call into a module's code, and a reloadable owner must not hand function
/// pointers across the boundary).
#[derive(Clone, Debug, Default)]
pub struct CommandInbox {
    /// Command ids executed this frame, in execution order.
    pub executed: Vec<String>,
}

impl pill_engine::Resource for CommandInbox {}

/// The world resource carrying engine mutations queued by command handlers.
///
/// The shell has no `Engine`, so its handlers queue [`EditorCommand`]s; the
/// executable applies the batch at the frame boundary
/// ([`EditorCommand::apply`]), exactly the contract the plan states.
#[derive(Clone, Debug, Default)]
pub struct PendingEditorCommands {
    /// The queued batches, drained by the executable.
    pub batches: Vec<PendingCommandBatch>,
}

impl PendingEditorCommands {
    /// Queue a batch that engine controls issue (pause, step, undo replays):
    /// it is applied, never recorded as a new undo step.
    pub fn push(&mut self, commands: Vec<EditorCommand>) {
        self.batches.push(PendingCommandBatch {
            commands,
            record: false,
            label: String::new(),
            coalesce_key: None,
        });
    }

    /// Queue an edit the history should record as one undo step.
    ///
    /// `coalesce_key` merges continuous edits (a slider drag) that arrive
    /// back to back under the same key into the previous step.
    pub fn push_recorded(
        &mut self,
        commands: Vec<EditorCommand>,
        label: impl Into<String>,
        coalesce_key: Option<String>,
    ) {
        self.batches.push(PendingCommandBatch {
            commands,
            record: true,
            label: label.into(),
            coalesce_key,
        });
    }
}

/// One queued batch and what the executable does with it at the frame
/// boundary.
#[derive(Clone, Debug)]
pub struct PendingCommandBatch {
    /// The commands to apply, in order.
    pub commands: Vec<EditorCommand>,
    /// Whether the batch is an edit the history records.
    pub record: bool,
    /// History label, when recorded.
    pub label: String,
    /// Continuous-edit key, when recorded.
    pub coalesce_key: Option<String>,
}

impl pill_engine::Resource for PendingEditorCommands {}

/// Inserts every command resource the editor relies on, keeping existing
/// values (the reload protocol's re-claim rule).
pub fn ensure_command_resources(world: &mut pill_engine::World) {
    CommandRegistry::ensure(world);
    world.register_resource::<CommandInbox>();
    if world.get_resource::<CommandInbox>().is_none() {
        world.insert_resource(CommandInbox::default());
    }
    world.register_resource::<PendingEditorCommands>();
    if world.get_resource::<PendingEditorCommands>().is_none() {
        world.insert_resource(PendingEditorCommands::default());
    }
    crate::history::CommandHistory::ensure(world);
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// The gizmo choices round-trip through the preferences file, and the
    /// keys another writer owns survive.
    #[test]
    fn gizmo_preferences_round_trip_and_preserve_other_keys() {
        let directory = std::env::temp_dir().join(format!(
            "ecs-gizmo-preferences-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(
            directory.join(PREFERENCES_FILE),
            r#"{ "theme": "dark", "history_step_cap": 42 }"#,
        )
        .unwrap();
        let settings = crate::gizmo::GizmoSettings {
            mode: crate::gizmo::GizmoMode::Rotate,
            space: crate::gizmo::GizmoSpace::Local,
            snap: true,
            ..crate::gizmo::GizmoSettings::default()
        };
        write_gizmo_preferences(&directory, "italian_brainrot", &settings).unwrap();
        // A second project's write keeps the first project's entry.
        let other = crate::gizmo::GizmoSettings {
            mode: crate::gizmo::GizmoMode::Scale,
            ..crate::gizmo::GizmoSettings::default()
        };
        write_gizmo_preferences(&directory, "project_rs", &other).unwrap();
        let text = std::fs::read_to_string(directory.join(PREFERENCES_FILE)).unwrap();
        let document: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(document["theme"], "dark");
        assert_eq!(document["history_step_cap"], 42);
        assert_eq!(
            document["gizmo"]["projects"]["italian_brainrot"]["mode"],
            "rotate"
        );
        assert_eq!(document["gizmo"]["projects"]["project_rs"]["mode"], "scale");

        let read_back = read_gizmo_settings(&directory, "italian_brainrot");
        assert_eq!(read_back.mode, crate::gizmo::GizmoMode::Rotate);
        assert_eq!(read_back.space, crate::gizmo::GizmoSpace::Local);
        assert!(read_back.snap);
        // A project the file never saw keeps the last-used values.
        let unknown = read_gizmo_settings(&directory, "circus_demo");
        assert_eq!(unknown.mode, crate::gizmo::GizmoMode::Scale);
        // And no file at all is the defaults.
        let empty = std::env::temp_dir().join("ecs-gizmo-nonexistent-dir");
        let defaults = read_gizmo_settings(&empty, "anything");
        assert_eq!(defaults, crate::gizmo::GizmoSettings::default());
        // A profile whose layout was never saved has no directory yet: the
        // write creates it instead of failing.
        let fresh = directory.join("unsaved-profile");
        write_gizmo_preferences(&fresh, "italian_brainrot", &settings).unwrap();
        assert_eq!(
            read_gizmo_settings(&fresh, "italian_brainrot").mode,
            crate::gizmo::GizmoMode::Rotate
        );
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn chords_parse_and_format_round_trip() {
        let chord: Chord = "ctrl+shift+p".parse().unwrap();
        assert!(chord.ctrl && chord.shift && !chord.alt);
        assert_eq!(chord.key, KeyName::P);
        assert_eq!(chord.to_string(), "ctrl+shift+p");
        let plain: Chord = "space".parse().unwrap();
        assert_eq!(plain, Chord::plain(KeyName::Space));
        assert!("ctrl+".parse::<Chord>().is_err());
        assert!("ctrl+bogus".parse::<Chord>().is_err());
        assert!("hyper+x".parse::<Chord>().is_err());
    }

    #[test]
    fn presets_rebind_builtins_and_overrides_win() {
        let mut registry = CommandRegistry::with_builtins();
        let pause = "editor.pause_toggle";
        // Blender is the default: Space pauses.
        assert_eq!(
            registry.effective_chords(pause),
            vec![Chord::plain(KeyName::Space)]
        );
        // Switching the preset rebinds without touching any command.
        let file: KeybindingsFile = serde_json::from_str(r#"{ "preset": "unity" }"#).unwrap();
        assert!(registry.apply_file(&file).is_empty());
        assert_eq!(
            registry.effective_chords(pause),
            vec![Chord::with(KeyName::P, true, false, false)]
        );
        // A single override wins on top of the preset.
        let file: KeybindingsFile = serde_json::from_str(
            r#"{ "preset": "unity", "overrides": { "editor.pause_toggle": ["ctrl+j"] } }"#,
        )
        .unwrap();
        assert!(registry.apply_file(&file).is_empty());
        assert_eq!(
            registry.effective_chords(pause),
            vec![Chord::with(KeyName::J, true, false, false)]
        );
        // An empty list unbinds it.
        let file: KeybindingsFile =
            serde_json::from_str(r#"{ "overrides": { "editor.pause_toggle": [] } }"#).unwrap();
        registry.apply_file(&file);
        assert!(registry.effective_chords(pause).is_empty());
    }

    #[test]
    fn conflicts_and_unknown_overrides_are_reported() {
        let mut registry = CommandRegistry::with_builtins();
        let file: KeybindingsFile = serde_json::from_str(
            r#"{ "overrides": { "editor.pause_toggle": ["f10"], "editor.nope": ["x"] } }"#,
        )
        .unwrap();
        let problems = registry.apply_file(&file);
        assert!(problems
            .iter()
            .any(|problem| problem.contains("editor.nope")));
        // f10 now binds pause and step (step's unity default, but the
        // preset is still blender: use blender's right for a real clash).
        let file: KeybindingsFile =
            serde_json::from_str(r#"{ "overrides": { "editor.step": ["space"] } }"#).unwrap();
        assert!(registry.apply_file(&file).is_empty());
        let conflicts = registry.conflicts();
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].0, Chord::plain(KeyName::Space));
        assert!(conflicts[0].1.contains(&"editor.pause_toggle".to_string()));
        assert!(conflicts[0].1.contains(&"editor.step".to_string()));
    }

    #[test]
    fn owners_replace_their_own_set_only() {
        let mut registry = CommandRegistry::with_builtins();
        registry.register(
            "pill_spline_editor",
            vec![
                CommandSpec::new("spline_editor.reset_offset", "Reset Spline Offset")
                    .menu(&["Spline", "Reset Offset"])
                    .bind(KeymapPreset::Blender, "ctrl+j"),
            ],
        );
        assert!(registry.command("spline_editor.reset_offset").is_some());
        assert!(registry.command("editor.pause_toggle").is_some());
        // The menu tree lists it under its top segment, beside the built-ins.
        let menus = menu_tree(&registry);
        assert!(menus.contains_key("Spline"));
        assert!(menus.contains_key("View"));
        assert!(!menus.contains_key("editor.command_palette"));
        // The next generation re-registers; nothing duplicates.
        registry.register(
            "pill_spline_editor",
            vec![CommandSpec::new(
                "spline_editor.reset_offset",
                "Reset Spline Offset",
            )],
        );
        assert_eq!(
            registry
                .commands()
                .filter(|spec| spec.id.starts_with("spline_editor."))
                .count(),
            1
        );
    }

    #[test]
    fn resolve_finds_the_first_matching_command() {
        let registry = CommandRegistry::with_builtins();
        let modifiers = pill_engine::Modifiers::default();
        let resolved = registry.resolve(pill_engine::KeyCode::Space, modifiers);
        assert_eq!(
            resolved.map(|spec| spec.id.as_str()),
            Some("editor.pause_toggle")
        );
        assert!(registry
            .resolve(pill_engine::KeyCode::KeyQ, modifiers)
            .is_none());
    }
}
