use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock};

use anyhow::{Context, Result};

static CANCELLED: LazyLock<Arc<AtomicBool>> = LazyLock::new(|| Arc::new(AtomicBool::new(false)));

pub(crate) fn install_signal_handlers() -> Result<()> {
    signal_hook::flag::register(signal_hook::consts::signal::SIGINT, Arc::clone(&CANCELLED))
        .context("install Ctrl-C handler")?;
    signal_hook::flag::register(signal_hook::consts::signal::SIGTERM, Arc::clone(&CANCELLED))
        .context("install termination handler")?;
    Ok(())
}

pub(crate) fn requested() -> bool {
    CANCELLED.load(Ordering::SeqCst)
}
