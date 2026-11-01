use std::sync::{Arc, Mutex, MutexGuard};

use slint::{ComponentHandle, ModelRc, VecModel};
use tokio::runtime::Handle;

use crate::{
    ApkItem, AppWindow,
    domain::models::ApkRelease,
    navigation::Screen,
    persistence::preferences::{GlassMode, Preferences, PreferencesStore},
    runtime::TaskCancellation,
    services::github,
    state::app_state::AppState,
};

pub struct Controller {
    state: Arc<Mutex<AppState>>,
    preferences: PreferencesStore,
}

impl Controller {
    pub fn new(preferences: PreferencesStore, initial: Preferences) -> Self {
        Self {
            state: Arc::new(Mutex::new(AppState::new(initial))),
            preferences,
        }
    }

    pub fn state(&self) -> Arc<Mutex<AppState>> {
        Arc::clone(&self.state)
    }

    pub fn select_screen(&self, index: i32) -> Option<Screen> {
        let screen = Screen::try_from(index).ok()?;
        self.lock_state().screen = screen;
        Some(screen)
    }

    pub fn update_glass_mode(&self, index: i32) -> Result<GlassMode, String> {
        let mode =
            GlassMode::from_index(index).ok_or_else(|| "Mode visual tidak valid.".to_owned())?;
        let preferences = Preferences { glass_mode: mode };
        self.preferences
            .save(&preferences)
            .map_err(|error| error.to_string())?;
        let mut state = self.lock_state();
        state.settings.glass_mode = mode;
        state.settings.message = None;
        Ok(mode)
    }

    pub fn set_settings_error(&self, message: String) {
        self.lock_state().settings.message = Some(message);
    }

    pub fn set_apks_loading(&self, loading: bool) {
        self.lock_state().apks.is_loading = loading;
    }

    pub fn load_apks(
        &self,
        window: &AppWindow,
        runtime: &Handle,
        cancellation: &TaskCancellation,
        client: reqwest::Client,
        force_refresh: bool,
    ) {
        let is_fresh = {
            let state = self.lock_state();
            state.apks.is_loading
                || (!force_refresh
                    && state.apks.loaded_at.is_some_and(|loaded_at| {
                        loaded_at.elapsed()
                            < crate::persistence::api_cache::CachePolicy::STANDARD.ttl
                    }))
        };
        if is_fresh {
            return;
        }

        self.set_apks_loading(true);
        window.set_apks_loading(true);
        window.set_apks_error("".into());

        let weak = window.as_weak();
        let controller_state = Arc::clone(&self.state);
        let cancellation_after = cancellation.clone();
        let task = runtime.spawn(async move {
            let result = github::fetch_apks(&client)
                .await
                .map_err(|error| format!("Gagal mengambil daftar aplikasi: {error}"));

            let weak_for_ui = weak.clone();
            let state_for_ui = Arc::clone(&controller_state);
            let cancellation_if_error = cancellation_after.clone();
            let completion = slint::invoke_from_event_loop(move || {
                if let Some(window) = weak_for_ui.upgrade() {
                    let mut state = state_for_ui.lock().unwrap_or_else(|poisoned| {
                        log::error!(
                            "Application state lock was poisoned; recovering its contents."
                        );
                        poisoned.into_inner()
                    });
                    state.apks.is_loading = false;

                    match result {
                        Ok(items) => {
                            let model = items.iter().map(apk_item).collect::<Vec<ApkItem>>();
                            state.apks.items = items;
                            state.apks.error = None;
                            state.apks.loaded_at = Some(std::time::Instant::now());
                            window.set_apks(ModelRc::new(VecModel::from(model)));
                            window.set_apks_error("".into());
                        }
                        Err(message) => {
                            state.apks.error = Some(message.clone());
                            window.set_apks_error(message.into());
                        }
                    }
                    window.set_apks_loading(false);
                }
                cancellation_after.clear();
            });

            if let Err(error) = completion {
                log::warn!("Could not deliver APK response to the UI event loop: {error}");
                cancellation_if_error.clear();
            }
        });
        cancellation.replace(task.abort_handle());
    }

    fn lock_state(&self) -> MutexGuard<'_, AppState> {
        self.state.lock().unwrap_or_else(|poisoned| {
            log::error!("Application state lock was poisoned; recovering its contents.");
            poisoned.into_inner()
        })
    }
}

fn apk_item(apk: &ApkRelease) -> ApkItem {
    ApkItem {
        name: apk.name.clone().into(),
        version: apk.version.clone().into(),
        size: apk.size.clone().into(),
        download_url: apk.download_url.clone().into(),
    }
}
