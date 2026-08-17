pub mod app;
pub mod evaluation;
pub mod lifecycle;
pub mod review;
pub mod rootfs;
pub mod run;
pub mod snapshot;
pub mod store;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const BUILD_ID: Option<&str> = option_env!("AGENTLAB_BUILD_ID");

pub fn build_version() -> String {
    match BUILD_ID
        .map(str::trim)
        .filter(|build_id| !build_id.is_empty())
    {
        Some(build_id) => format!("{VERSION}+{build_id}"),
        None => VERSION.to_owned(),
    }
}
