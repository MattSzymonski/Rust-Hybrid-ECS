//! Running futures on every target.
//!
//! # Responsibilities
//!
//! - Run a future to completion on the calling thread on native
//!   ([`block_on`]).
//! - Start a future whose result nobody waits for ([`spawn`]): blocks on
//!   native, hands it to the browser's event loop on the web.
//!
//! A browser only resolves a future after control returns to its event loop,
//! so blocking on one there spins forever. That is why [`block_on`] exists
//! only on native: code that needs a future's value on the web must `.await`
//! it instead.

// Standard library
use std::future::Future;

/// Run `future` to completion on the calling thread and return its output.
///
/// Native only; on the web a future has to be awaited or [`spawn`]ed.
#[cfg(not(target_arch = "wasm32"))]
pub fn block_on<F: Future>(future: F) -> F::Output {
    pollster::block_on(future)
}

/// Start `future` without waiting for its result.
///
/// On native it runs to completion before this returns; on the web it runs on
/// the browser's event loop after the current task yields.
pub fn spawn<F: Future<Output = ()> + 'static>(future: F) {
    #[cfg(not(target_arch = "wasm32"))]
    pollster::block_on(future);
    #[cfg(target_arch = "wasm32")]
    wasm_bindgen_futures::spawn_local(future);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_on_returns_the_output() {
        assert_eq!(block_on(async { 7 }), 7);
    }

    #[test]
    fn spawn_runs_the_future_on_native() {
        let ran = std::rc::Rc::new(std::cell::Cell::new(false));
        let flag = ran.clone();
        spawn(async move { flag.set(true) });
        assert!(ran.get());
    }
}
