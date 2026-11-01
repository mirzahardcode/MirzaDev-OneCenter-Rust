use crate::{
    domain::models::{ApkRelease, LatestInformation},
    persistence::preferences::GlassMode,
};

#[derive(Clone, Debug, Default)]
pub struct HomeState {
    pub information: Option<LatestInformation>,
    pub message: String,
    pub is_online: bool,
}

#[derive(Clone, Debug, Default)]
pub struct ApksState {
    pub items: Vec<ApkRelease>,
    pub is_loading: bool,
    pub error: Option<String>,
    pub loaded_at: Option<std::time::Instant>,
}

#[derive(Clone, Debug, Default)]
pub struct PrivateState {
    pub unavailable_reason: String,
}

#[derive(Clone, Debug, Default)]
pub struct SettingsState {
    pub glass_mode: GlassMode,
    pub message: Option<String>,
}
