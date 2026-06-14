//! Drives the main-thread future/task executor for the standalone extension.
//!
//! The wizards (preset crawler, preview recorder) are `async` and progress only when
//! `Global::future_support()`'s middleware is run on the main thread. The full Helgobox
//! plugin pumps this from its control surface's `run()`; the standalone extension has no
//! such loop, so instead the script UI calls [`run_tasks`] once per frame (via
//! `HB_Pot_RunTasks`). That's sufficient because the only async work — the wizards — is
//! always initiated from, and observed by, that same UI loop.

use base::Global;
use reaper_high::{FutureMiddleware, MainTaskMiddleware};
use std::cell::RefCell;

thread_local! {
    /// Created lazily on first pump. Main-thread-only: the executor and all spawned
    /// futures (which hold REAPER handles) live on the main thread.
    static MIDDLEWARES: RefCell<Option<(MainTaskMiddleware, FutureMiddleware)>> =
        const { RefCell::new(None) };
}

/// Runs one tick of the main-task and future middlewares. Call repeatedly from the main
/// thread (the script's defer loop) so spawned futures make progress.
pub fn run_tasks() {
    reaper_low::firewall(|| {
        MIDDLEWARES.with(|m| {
            let mut cell = m.borrow_mut();
            let mw = cell.get_or_insert_with(|| {
                let global = Global::get();
                (
                    global.create_task_support_middleware(),
                    global.create_future_support_middleware(),
                )
            });
            mw.0.run();
            mw.1.run();
        });
    });
}
