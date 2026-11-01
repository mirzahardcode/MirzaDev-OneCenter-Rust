use crate::{
    navigation::Screen,
    persistence::preferences::Preferences,
    state::screens::{ApksState, HomeState, PrivateState, SettingsState},
};

#[derive(Clone, Debug)]
pub struct AppState {
    pub screen: Screen,
    pub home: HomeState,
    pub apks: ApksState,
    pub private: PrivateState,
    pub settings: SettingsState,
}

impl AppState {
    pub fn new(preferences: Preferences) -> Self {
        Self {
            screen: Screen::Home,
            home: HomeState {
                message: "Konten informasi privat memerlukan layanan backend terotorisasi."
                    .to_owned(),
                ..HomeState::default()
            },
            apks: ApksState::default(),
            private: PrivateState {
                unavailable_reason: "Akses Private di Windows dinonaktifkan sampai kontrak device binding dan backend authorization disepakati.".to_owned(),
            },
            settings: SettingsState {
                glass_mode: preferences.glass_mode,
                message: None,
            },
        }
    }
}
