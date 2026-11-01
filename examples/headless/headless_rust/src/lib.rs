//! A pure simulation: no renderer, no assets, no window.
//!
//! # Responsibilities
//!
//! - Fill the world with [`TICKER_COUNT`] tickers, each counting the frames it
//!   has lived through.
//! - Report the world's total once a second, through the engine's log, which is
//!   the only output a headless run has.
//!
//! # Design
//!
//! `renderer: none` in `project_settings.yaml` keeps the renderer's GPU module
//! and its data crate out of the load order: no device is opened, no surface
//! exists, and nothing the frame draws has to be produced. What is left is the
//! engine's core - world, scheduler, hot reload - which is exactly what a
//! server-side or test build runs.
//!
//! Entities are filled up to a target count rather than spawned fresh, because
//! `init` runs again on every hot reload against the live world.

// External crates
use pill_engine::*;
use serde::{Deserialize, Serialize};

/// How many entities the world holds.
const TICKER_COUNT: usize = 256;

/// Seconds between two reports.
///
/// Wall-clock rather than frames: a headless host has no vsync, so it runs the
/// frame loop as fast as the machine allows and a frame-count interval would
/// print thousands of lines a second.
const REPORT_INTERVAL_SECONDS: f32 = 1.0;

/// One entity's frame counter.
///
/// The host serializes this component across hot-reload generations, so the
/// layout is pinned with `#[repr(C)]` and every field stays `Serialize` /
/// `Deserialize` compatible.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PillComponent)]
#[pill(persistable)]
pub struct TickCounter {
    /// Frames this entity has been stepped through.
    pub ticks: u64,
}

/// How far the report schedule has got, kept beside the world it describes.
#[derive(Debug, Default)]
pub struct ReportSchedule {
    /// Seconds since the last report was logged.
    pub seconds_since_report: f32,
}

impl Resource for ReportSchedule {}

/// Seeds the world, the report schedule and the one system that steps it.
#[pill_project]
pub fn init(engine: &mut Engine) -> u32 {
    engine
        .world_mut()
        .insert_resource(ReportSchedule::default());

    let world = engine.world_mut();
    let existing = Query::<&TickCounter>::new(world).iter_mut().count();
    for _ in existing..TICKER_COUNT {
        if let Err(error) = world.create_entity().with(TickCounter::default()).build() {
            tracing::error!("ticker spawn failed: {error}");
            return 1;
        }
    }

    engine.register_system("tick", tick_system);
    0
}

/// Advances every ticker and logs the world's total on the report seconds.
fn tick_system(
    time: Res<Time>,
    mut tickers: Query<&mut TickCounter>,
    mut schedule: ResMut<ReportSchedule>,
) -> Result<(), SystemError> {
    let mut total = 0_u64;
    for mut ticker in tickers.iter_mut() {
        ticker.ticks += 1;
        total += ticker.ticks;
    }

    let Some(time) = time.get() else {
        return Ok(());
    };
    let Some(mut schedule) = schedule.get_mut() else {
        return Ok(());
    };
    schedule.seconds_since_report += time.delta_seconds();
    if schedule.seconds_since_report >= REPORT_INTERVAL_SECONDS {
        schedule.seconds_since_report = 0.0;
        tracing::info!("[headless] tickers={TICKER_COUNT} total_ticks={total}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initialization_fills_the_world_once() {
        let mut engine = Engine::new();

        assert_eq!(init(&mut engine), 0);

        let tickers = Query::<&TickCounter>::new(engine.world_mut())
            .iter_mut()
            .count();
        assert_eq!(tickers, TICKER_COUNT);
    }

    /// A hot reload runs `init` again; it must not duplicate the world.
    #[test]
    fn initializing_again_keeps_one_world() {
        let mut engine = Engine::new();
        assert_eq!(init(&mut engine), 0);

        assert_eq!(init(&mut engine), 0);

        let tickers = Query::<&TickCounter>::new(engine.world_mut())
            .iter_mut()
            .count();
        assert_eq!(tickers, TICKER_COUNT);
    }
}
