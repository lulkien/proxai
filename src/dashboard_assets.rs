use rust_embed::RustEmbed;

/// Embedded dashboard files (HTML + CSS compiled from SCSS).
#[derive(RustEmbed)]
#[folder = "dashboard/"]
pub struct DashboardAssets;
