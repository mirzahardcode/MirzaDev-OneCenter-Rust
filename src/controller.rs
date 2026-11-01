use std::{
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, Instant},
};

use slint::{ComponentHandle, ModelRc, VecModel};
use tokio::runtime::Handle;

use crate::{
    ApkItem, AppWindow,
    domain::models::ApkRelease,
    navigation::Screen,
    persistence::{
        api_cache::CachePolicy,
        preferences::{GlassMode, Preferences, PreferencesStore},
    },
    runtime::{TaskGuard, TaskKind, TaskRegistry},
    services::github,
    state::app_state::AppState,
};

type ApkResult = Result<Vec<ApkRelease>, String>;

/// What the UI has to show after a *current* APK request has been committed to
/// the application state.
enum ApkUpdate {
    Loaded(Vec<ApkItem>),
    Failed(String),
}

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

    /// Starts an APK request unless the cached list is still fresh (and no
    /// refresh was forced) or another APK request is already running.
    ///
    /// Must be called on the Slint event-loop thread. The network request runs
    /// on the Tokio runtime; its result is applied back on the event-loop
    /// thread, and only if no newer request has replaced it in the meantime.
    pub fn load_apks(
        &self,
        window: &AppWindow,
        runtime: &Handle,
        tasks: &TaskRegistry,
        client: reqwest::Client,
        force_refresh: bool,
    ) {
        if !force_refresh && self.apks_are_fresh() {
            return;
        }

        // Reserve the ticket before spawning so the request is registered
        // (and duplicates are rejected) atomically.
        let Some(ticket) = tasks.try_begin(TaskKind::ApkFetch) else {
            log::debug!("APK request ignored because another one is still running.");
            return;
        };

        self.set_apks_loading(true);
        window.set_apks_loading(true);
        window.set_apks_error("".into());

        // The guard releases the registry entry wherever it is finally
        // dropped: after the UI update, on abort, on panic, or if the event
        // loop is gone.
        let guard = tasks.guard(ticket);
        let weak = window.as_weak();
        let state = Arc::clone(&self.state);
        let task = runtime.spawn(async move {
            let result = github::fetch_apks(&client)
                .await
                .map_err(|error| format!("Gagal mengambil daftar aplikasi: {error}"));
            deliver_apk_result(weak, state, guard, result);
        });

        if !tasks.attach(ticket, task.abort_handle()) {
            log::debug!(
                "APK request {} was superseded before its task could be attached; it was aborted.",
                ticket.id()
            );
        }
    }

    fn apks_are_fresh(&self) -> bool {
        let loaded_at = self.lock_state().apks.loaded_at;
        is_within_ttl(loaded_at, Instant::now(), CachePolicy::STANDARD.ttl)
    }

    fn lock_state(&self) -> MutexGuard<'_, AppState> {
        lock_app_state(&self.state)
    }
}

fn lock_app_state(state: &Mutex<AppState>) -> MutexGuard<'_, AppState> {
    state.lock().unwrap_or_else(|poisoned| {
        log::error!("Application state lock was poisoned; recovering its contents.");
        poisoned.into_inner()
    })
}

fn is_within_ttl(loaded_at: Option<Instant>, now: Instant, ttl: Duration) -> bool {
    loaded_at.is_some_and(|loaded_at| now.saturating_duration_since(loaded_at) < ttl)
}

/// Hands a finished request over to the Slint event loop.
///
/// The guard travels inside the closure. If the closure runs it decides
/// whether the result is still current and then drops the guard; if delivery
/// fails or the closure is discarded, dropping it still releases the ticket.
fn deliver_apk_result(
    weak: slint::Weak<AppWindow>,
    state: Arc<Mutex<AppState>>,
    guard: TaskGuard,
    result: ApkResult,
) {
    let delivery = slint::invoke_from_event_loop(move || {
        match commit_apk_result(&state, &guard, result) {
            Some(update) => {
                // The window may already be closed; then there is nothing to
                // update but the shared state, which has been committed above.
                if let Some(window) = weak.upgrade() {
                    apply_apk_update(&window, update);
                }
            }
            None => log::debug!(
                "Discarded stale APK response for request {}.",
                guard.ticket().id()
            ),
        }
    });

    if let Err(error) = delivery {
        log::warn!("Could not deliver APK response to the UI event loop: {error}");
    }
}

/// Applies a finished request to the shared state, but only if its ticket is
/// still the current one. Returns `None` for a stale request, leaving both the
/// state and the loading flag (now owned by the newer request) untouched.
fn commit_apk_result(
    state: &Mutex<AppState>,
    guard: &TaskGuard,
    result: ApkResult,
) -> Option<ApkUpdate> {
    if !guard.is_current() {
        return None;
    }

    let mut state = lock_app_state(state);
    state.apks.is_loading = false;
    match result {
        Ok(items) => {
            let model = items.iter().map(apk_item).collect::<Vec<ApkItem>>();
            state.apks.items = items;
            state.apks.error = None;
            state.apks.loaded_at = Some(Instant::now());
            Some(ApkUpdate::Loaded(model))
        }
        Err(message) => {
            state.apks.error = Some(message.clone());
            Some(ApkUpdate::Failed(message))
        }
    }
}

fn apply_apk_update(window: &AppWindow, update: ApkUpdate) {
    match update {
        ApkUpdate::Loaded(items) => {
            window.set_apks(ModelRc::new(VecModel::from(items)));
            window.set_apks_error("".into());
        }
        ApkUpdate::Failed(message) => window.set_apks_error(message.into()),
    }
    window.set_apks_loading(false);
}

fn apk_item(apk: &ApkRelease) -> ApkItem {
    ApkItem {
        name: apk.name.clone().into(),
        version: apk.version.clone().into(),
        size: apk.size.clone().into(),
        download_url: apk.download_url.clone().into(),
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::{
        ApkUpdate, Controller, commit_apk_result, is_within_ttl, lock_app_state,
    };
    use crate::{
        domain::models::ApkRelease,
        navigation::Screen,
        persistence::preferences::{
            GlassMode, Preferences, PreferencesStore,
            test_support::{TestDir, file_names},
        },
        runtime::{TaskKind, TaskRegistry},
    };

    fn new_controller(directory: &TestDir) -> Controller {
        Controller::new(
            PreferencesStore::with_path(directory.path().join("preferences.json")),
            Preferences::default(),
        )
    }

    fn apk(name: &str) -> ApkRelease {
        ApkRelease {
            name: name.to_owned(),
            version: "1.0.0".to_owned(),
            size: "1.0 MB".to_owned(),
            download_url: format!("https://example.test/{name}.apk"),
        }
    }

    #[test]
    fn select_screen_accepts_known_indexes_and_rejects_unknown_ones() {
        let directory = TestDir::new("controller-screens");
        let controller = new_controller(&directory);

        assert_eq!(controller.select_screen(1), Some(Screen::Apks));
        assert_eq!(controller.lock_state().screen, Screen::Apks);

        assert_eq!(controller.select_screen(42), None);
        assert_eq!(controller.lock_state().screen, Screen::Apks);
    }

    #[test]
    fn glass_mode_update_persists_and_updates_state() {
        let directory = TestDir::new("controller-glass");
        let controller = new_controller(&directory);

        assert_eq!(controller.update_glass_mode(2), Ok(GlassMode::Liquid));
        assert_eq!(controller.lock_state().settings.glass_mode, GlassMode::Liquid);

        let reloaded = PreferencesStore::with_path(directory.path().join("preferences.json"))
            .load()
            .unwrap();
        assert_eq!(reloaded.glass_mode, GlassMode::Liquid);
    }

    #[test]
    fn invalid_glass_mode_is_rejected_without_touching_disk_or_state() {
        let directory = TestDir::new("controller-glass-invalid");
        let controller = new_controller(&directory);

        assert!(controller.update_glass_mode(9).is_err());
        assert_eq!(controller.lock_state().settings.glass_mode, GlassMode::Off);
        assert!(file_names(directory.path()).is_empty());
    }

    #[test]
    fn settings_error_is_recorded_and_cleared_by_a_successful_update() {
        let directory = TestDir::new("controller-settings-error");
        let controller = new_controller(&directory);

        controller.set_settings_error("gagal".to_owned());
        assert_eq!(
            controller.lock_state().settings.message.as_deref(),
            Some("gagal")
        );

        controller.update_glass_mode(1).unwrap();
        assert!(controller.lock_state().settings.message.is_none());
    }

    #[test]
    fn ttl_check_uses_the_standard_boundary() {
        let ttl = Duration::from_secs(30 * 60);
        let loaded = Instant::now();

        assert!(!is_within_ttl(None, loaded, ttl));
        assert!(is_within_ttl(Some(loaded), loaded, ttl));
        assert!(is_within_ttl(
            Some(loaded),
            loaded + ttl - Duration::from_secs(1),
            ttl
        ));
        assert!(!is_within_ttl(Some(loaded), loaded + ttl, ttl));
        // An earlier "now" must not underflow.
        assert!(is_within_ttl(Some(loaded + ttl), loaded, ttl));
    }

    #[test]
    fn current_request_updates_state_and_releases_its_ticket() {
        let directory = TestDir::new("controller-commit");
        let controller = new_controller(&directory);
        let tasks = TaskRegistry::default();

        controller.set_apks_loading(true);
        let ticket = tasks.try_begin(TaskKind::ApkFetch).unwrap();
        let guard = tasks.guard(ticket);

        let update = commit_apk_result(&controller.state, &guard, Ok(vec![apk("a"), apk("b")]));
        assert!(matches!(&update, Some(ApkUpdate::Loaded(items)) if items.len() == 2));
        if let Some(ApkUpdate::Loaded(items)) = &update {
            assert_eq!(items[0].name.as_str(), "a");
            assert_eq!(items[1].download_url.as_str(), "https://example.test/b.apk");
        }

        {
            let state = controller.lock_state();
            assert_eq!(state.apks.items.len(), 2);
            assert!(!state.apks.is_loading);
            assert!(state.apks.error.is_none());
            assert!(state.apks.loaded_at.is_some());
        }

        drop(guard);
        assert!(!tasks.is_active(TaskKind::ApkFetch));
    }

    #[test]
    fn stale_request_cannot_overwrite_state_or_clear_the_newer_entry() {
        let directory = TestDir::new("controller-stale");
        let controller = new_controller(&directory);
        let tasks = TaskRegistry::default();

        // Request A starts, then request B replaces it.
        let first = tasks.try_begin(TaskKind::ApkFetch).unwrap();
        let first_guard = tasks.guard(first);
        let second = tasks.begin(TaskKind::ApkFetch);
        let second_guard = tasks.guard(second);
        controller.set_apks_loading(true);

        // A finishes late.
        let update = commit_apk_result(&controller.state, &first_guard, Ok(vec![apk("old")]));
        assert!(update.is_none());
        {
            let state = controller.lock_state();
            assert!(state.apks.items.is_empty());
            assert!(state.apks.is_loading);
            assert!(state.apks.loaded_at.is_none());
        }
        drop(first_guard);
        assert!(tasks.is_current(second));

        // B finishes normally.
        let update = commit_apk_result(&controller.state, &second_guard, Ok(vec![apk("new")]));
        assert!(matches!(&update, Some(ApkUpdate::Loaded(items)) if items.len() == 1));
        assert_eq!(controller.lock_state().apks.items[0].name, "new");
        drop(second_guard);
        assert!(!tasks.is_active(TaskKind::ApkFetch));
    }

    #[test]
    fn failed_request_records_the_error_and_keeps_previous_items() {
        let directory = TestDir::new("controller-failure");
        let controller = new_controller(&directory);
        let tasks = TaskRegistry::default();

        let first = tasks.try_begin(TaskKind::ApkFetch).unwrap();
        let first_guard = tasks.guard(first);
        assert!(commit_apk_result(&controller.state, &first_guard, Ok(vec![apk("kept")])).is_some());
        drop(first_guard);

        controller.set_apks_loading(true);
        let second = tasks.try_begin(TaskKind::ApkFetch).unwrap();
        let second_guard = tasks.guard(second);
        let update = commit_apk_result(&controller.state, &second_guard, Err("boom".to_owned()));
        assert!(matches!(&update, Some(ApkUpdate::Failed(message)) if message == "boom"));

        let state = lock_app_state(&controller.state);
        assert_eq!(state.apks.items.len(), 1);
        assert_eq!(state.apks.error.as_deref(), Some("boom"));
        assert!(!state.apks.is_loading);
    }

    #[test]
    fn cancelled_request_is_ignored_after_cancel_all() {
        let directory = TestDir::new("controller-cancelled");
        let controller = new_controller(&directory);
        let tasks = TaskRegistry::default();

        let ticket = tasks.try_begin(TaskKind::ApkFetch).unwrap();
        let guard = tasks.guard(ticket);
        assert_eq!(tasks.cancel_all(), 1);

        assert!(commit_apk_result(&controller.state, &guard, Ok(vec![apk("late")])).is_none());
        assert!(controller.lock_state().apks.items.is_empty());
    }
}