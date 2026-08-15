pub mod app;
pub mod evaluation;
pub mod lifecycle;
pub mod rootfs;
pub mod run;
pub mod snapshot;
pub mod store;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
