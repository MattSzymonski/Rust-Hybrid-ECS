//! Components for the `--cs_scripting` path.
//!
//! Defined here — in the host, never rebuilt while the process runs —
//! because C# mirrors their exact layout (`tracy_live_game_cs/src/
//! Components.cs`) and needs it to stay byte-stable for the whole process
//! lifetime. This is a deliberate contrast with `tracy_live_game`'s
//! components (used by `--rs_scripting`), which live in the *reloaded*
//! crate — that's fine there because nothing outside Rust needs their
//! layout to stay fixed across a rebuild.

use ecs_hybrid::*;
use trait_type_map::impl_trait_accessible;

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Position {
    pub x: f32,
    pub y: f32,
}
impl Component for Position {}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Velocity {
    pub x: f32,
    pub y: f32,
}
impl Component for Velocity {}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Health(pub f32);
impl Component for Health {}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Mass(pub f32);
impl Component for Mass {}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct GravityForce {
    pub x: f32,
    pub y: f32,
}
impl Component for GravityForce {}

impl_trait_accessible!(dyn Component; Position, Velocity, Health, Mass, GravityForce);

/// Fast LCG random f32 — seeded from CPU counter, no syscalls. Identical
/// helper to `tracy_live_game::game::lcg`, duplicated rather than shared:
/// it's a 15-line leaf helper and these two crates are never built together.
fn lcg() -> f32 {
    #[cfg(target_arch = "x86_64")]
    fn seed() -> u64 {
        // RDTSC — fast, non-crypto seed. No syscall, no blocking.
        unsafe { std::arch::x86_64::_rdtsc() }
    }
    #[cfg(not(target_arch = "x86_64"))]
    fn seed() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_micros() as u64
    }

    use std::cell::Cell;
    thread_local! {
        static S: Cell<u64> = Cell::new(seed().wrapping_mul(6364136223846793005).wrapping_add(1));
    }
    S.with(|s| {
        let mut x = s.get();
        x = x
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        s.set(x);
        (x >> 32) as f32 / u32::MAX as f32
    })
}

/// Registers components and spawns the same 30000-entity population as
/// `tracy_live_game::game::setup` — called once at startup, never again.
/// The C# path has no world-reset-on-reload; only `Systems.cs`'s code gets
/// swapped, the entity population underneath stays put.
///
/// No `Enemy` marker component: nothing in the C# path filters on it (no
/// `enemy_ai_system` equivalent), so it's simply omitted.
pub fn setup(engine: &mut Engine) {
    engine.world_mut().register_component::<Position>();
    engine.world_mut().register_component::<Velocity>();
    engine.world_mut().register_component::<Health>();
    engine.world_mut().register_component::<Mass>();
    engine.world_mut().register_component::<GravityForce>();

    engine.world_mut().reserve_entities(32000);
    for _ in 0..30000 {
        let _ = engine
            .world_mut()
            .create_entity()
            .with(Position {
                x: (lcg() - 0.5) * 1000.0,
                y: (lcg() - 0.5) * 1000.0,
            })
            .with(Velocity {
                x: (lcg() - 0.5) * 0.2,
                y: (lcg() - 0.5) * 0.2,
            })
            .with(Health(100.0))
            .with(Mass(1.0 + lcg() * 9.0))
            .with(GravityForce { x: 0.0, y: 0.0 })
            .build();
    }
}
