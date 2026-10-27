//! The components the crate's unit tests build worlds out of, and one way to
//! register them.
//!
//! # Responsibilities
//!
//! - Define the crate's test fixtures once: [`Position`], [`Velocity`] and
//!   [`Health`].
//! - Offer [`setup_world`], a [`World`] with all three registered, for tests
//!   whose subject is not registration itself.
//!
//! # Design
//!
//! One definition serves every suite, so the world, query and commands tests
//! build their worlds out of the same shapes. A fixture that drifts between
//! suites makes a passing test prove less than it reads like it does.
//!
//! `Health` is a tuple struct on purpose: the query suite writes it as a
//! literal throughout, and a named field buys nothing a comment cannot.

// Current crate
use crate::component::Component;
use crate::world::World;

/// 2D position component used as a test fixture across the crate's suites.
///
/// Plain `f32` fields keep the fixture minimal so tests focus on the system
/// under test rather than component behaviour.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Position {
    /// Horizontal coordinate.
    pub x: f32,
    /// Vertical coordinate.
    pub y: f32,
}

impl Component for Position {}

/// 2D velocity component used as a test fixture across the crate's suites.
///
/// Paired with [`Position`] in tests that exercise multi-component queries
/// and parallel mutation.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Velocity {
    /// Horizontal coordinate.
    pub x: f32,
    /// Vertical coordinate.
    pub y: f32,
}

impl Component for Velocity {}

/// Health value component used as a test fixture across the crate's suites.
///
/// A tuple struct that creates archetypes distinct from the
/// [`Position`]/[`Velocity`] pair, giving filters disjoint sets to match.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Health(pub i32);

impl Component for Health {}

/// A fresh [`World`] with the three fixture components registered.
///
/// The order is fixed so component ids are deterministic; a test that needs
/// a different registration should register its own components after this
/// one rather than forking the fixture.
pub(crate) fn setup_world() -> World {
    let mut world = World::new();
    world.register_component::<Position>();
    world.register_component::<Velocity>();
    world.register_component::<Health>();
    world
}
