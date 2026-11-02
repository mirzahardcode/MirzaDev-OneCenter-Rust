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
    pub has_loaded_data: bool,
    pub is_loading: bool,
    pub is_cache_checking: bool,
    pub error: Option<String>,
    pub data_status: ApkDataStatus,
    pub loaded_at: Option<std::time::Instant>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ApkDataStatus {
    #[default]
    None,
    Cached,
    StaleCache,
    Refreshing,
    RefreshingStaleCache,
    OfflineFallback,
    Updated,
}

impl ApkDataStatus {
    pub const fn label(self) -> &'static str {
        match self {
            Self::None => "",
            Self::Cached => "Data tersimpan",
            Self::StaleCache => "Menggunakan cache lama",
            Self::Refreshing => "Sedang memperbarui data...",
            Self::RefreshingStaleCache => "Memakai cache lama sambil memperbarui...",
            Self::OfflineFallback => "Gagal memperbarui, menggunakan data terakhir",
            Self::Updated => "Data terbaru",
        }
    }
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
