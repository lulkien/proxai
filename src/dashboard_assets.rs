use rust_embed::RustEmbed;

/// Embedded dashboard WASM files. Populated by build.sh which copies dx output
/// to pkg/dashboard-dist/ before compiling.
#[derive(RustEmbed)]
#[folder = "pkg/dashboard-dist"]
pub struct DashboardAssets;
