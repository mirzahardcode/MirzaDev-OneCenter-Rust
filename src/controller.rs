use std::{
    future::Future,
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
        api_cache::{CacheLookup, CachePolicy, CachedValue, PersistentJsonCache},
        preferences::{GlassMode, Preferences, PreferencesStore},
    },
    runtime::{TaskGuard, TaskKind, TaskRegistry, TaskTicket},
    services::github,
    state::{
        app_state::AppState,
        screens::{ApkDataStatus, ApksState},
    },
};

type ApkCache = PersistentJsonCache<Vec<ApkRelease>>;

struct ApkLoadContext {
    cache: Option<ApkCache>,
    cache_write_lock: Arc<Mutex<()>>,
    tasks: TaskRegistry,
    ticket: TaskTicket,
    force_refresh: bool,
    memory_fallback: Option<ApkFallback>,
}

#[derive(Clone, Debug)]
struct ApkFallback {
    items: Vec<ApkRelease>,
    loaded_at: Option<Instant>,
    age: Option<Duration>,
}

enum ApkLoadOutcome {
    Cached(ApkFallback),
    Updated(Vec<ApkRelease>),
    Superseded,
    Failed {
        message: String,
        fallback: Option<ApkFallback>,
    },
}

struct ApkUiUpdate {
    items: Option<Vec<ApkItem>>,
    error: Option<String>,
    data_status: ApkDataStatus,
    is_loading: bool,
    is_cache_checking: bool,
}

pub struct Controller {
    state: Arc<Mutex<AppState>>,
    preferences: PreferencesStore,
    apk_cache: Option<ApkCache>,
    apk_cache_write_lock: Arc<Mutex<()>>,
}

impl Controller {
    pub fn new(preferences: PreferencesStore, initial: Preferences) -> Self {
        Self {
            state: Arc::new(Mutex::new(AppState::new(initial))),
            preferences,
            apk_cache: None,
            apk_cache_write_lock: Arc::new(Mutex::new(())),
        }
    }

    pub fn with_apk_cache(mut self, cache: Option<PersistentJsonCache<Vec<ApkRelease>>>) -> Self {
        self.apk_cache = cache;
        self
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

    pub fn reset_apks_request_state(&self) {
        let mut state = self.lock_state();
        state.apks.is_loading = false;
        state.apks.is_cache_checking = false;
        if matches!(
            state.apks.data_status,
            ApkDataStatus::Refreshing | ApkDataStatus::RefreshingStaleCache
        ) {
            state.apks.data_status = if !state.apks.has_loaded_data {
                ApkDataStatus::None
            } else if state.apks.loaded_at.is_some_and(|loaded_at| {
                is_within_ttl(Some(loaded_at), Instant::now(), CachePolicy::STANDARD.ttl)
            }) {
                ApkDataStatus::Cached
            } else {
                ApkDataStatus::StaleCache
            };
        }
    }

    /// Loads APKs from fresh memory or disk cache when possible, then refreshes
    /// from GitHub. A forced refresh bypasses both cache shortcuts and replaces
    /// any running APK request.
    ///
    /// Must be called on the Slint event-loop thread. Cache I/O runs through
    /// Tokio's blocking pool and the GitHub request on the async runtime. State
    /// and UI updates are returned to the event loop and guarded against stale
    /// requests.
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

        let ticket = if force_refresh {
            tasks.begin(TaskKind::ApkFetch)
        } else {
            let Some(ticket) = tasks.try_begin(TaskKind::ApkFetch) else {
                log::debug!("APK request ignored because another one is still running.");
                return;
            };
            ticket
        };

        let memory_fallback = {
            let mut state = self.lock_state();
            let fallback = memory_fallback_from_state(&state.apks, Instant::now());
            state.apks.is_cache_checking = self.apk_cache.is_some() && !force_refresh;
            state.apks.is_loading = force_refresh || self.apk_cache.is_none();
            state.apks.error = None;
            state.apks.data_status = if fallback.is_some() {
                ApkDataStatus::Refreshing
            } else {
                ApkDataStatus::None
            };
            fallback
        };
        window.set_apks_checking_cache(self.apk_cache.is_some() && !force_refresh);
        window.set_apks_loading(force_refresh || self.apk_cache.is_none());
        window.set_apks_error("".into());
        window.set_apks_status(self.lock_state().apks.data_status.label().into());

        // The guard releases the registry entry wherever it is finally
        // dropped: after the UI update, on abort, on panic, or if the event
        // loop is gone.
        let guard = tasks.guard(ticket);
        let weak = window.as_weak();
        let state = Arc::clone(&self.state);
        let cache = self.apk_cache.clone();
        let cache_write_lock = Arc::clone(&self.apk_cache_write_lock);
        let callback_tasks = tasks.clone();
        let cache_weak = weak.clone();
        let cache_state = Arc::clone(&state);
        let cache_tasks = callback_tasks.clone();
        let task = runtime.spawn(async move {
            let outcome = load_apks_with_cache(
                ApkLoadContext {
                    cache,
                    cache_write_lock,
                    tasks: callback_tasks.clone(),
                    ticket,
                    force_refresh,
                    memory_fallback,
                },
                move || async move { github::fetch_apks(&client).await },
                move |cached| {
                    deliver_apk_cache_snapshot(
                        cache_weak.clone(),
                        Arc::clone(&cache_state),
                        cache_tasks.clone(),
                        ticket,
                        cached,
                    );
                },
            )
            .await;
            deliver_apk_result(weak, state, callback_tasks, guard, outcome);
        });

        if !tasks.attach(ticket, task.abort_handle()) {
            log::debug!(
                "APK request {} was superseded before its task could be attached; it was aborted.",
                ticket.id()
            );
        }
    }

    fn apks_are_fresh(&self) -> bool {
        let state = self.lock_state();
        state.apks.has_loaded_data
            && is_within_ttl(
                state.apks.loaded_at,
                Instant::now(),
                CachePolicy::STANDARD.ttl,
            )
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

fn memory_fallback_from_state(state: &ApksState, now: Instant) -> Option<ApkFallback> {
    state.has_loaded_data.then(|| ApkFallback {
        items: state.items.clone(),
        loaded_at: state.loaded_at,
        age: state
            .loaded_at
            .and_then(|loaded_at| now.checked_duration_since(loaded_at)),
    })
}

async fn load_apks_with_cache<F, Fut, C>(
    context: ApkLoadContext,
    fetcher: F,
    on_cache_snapshot: C,
) -> ApkLoadOutcome
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = crate::error::AppResult<Vec<ApkRelease>>> + Send + 'static,
    C: FnOnce(Option<CachedValue<Vec<ApkRelease>>>) + Send + 'static,
{
    let ApkLoadContext {
        cache,
        cache_write_lock,
        tasks,
        ticket,
        force_refresh,
        memory_fallback,
    } = context;
    let cached = match cache.clone() {
        Some(cache) => {
            let read_tasks = tasks.clone();
            match tokio::task::spawn_blocking(move || {
                read_tasks.is_current(ticket).then(|| cache.load())
            })
            .await
            {
                Ok(None) => return ApkLoadOutcome::Superseded,
                Ok(Some(Ok(CacheLookup::Hit(value)))) => Some(value),
                Ok(Some(Ok(CacheLookup::Missing | CacheLookup::Corrupted(_)))) => None,
                Ok(Some(Err(error))) => {
                    log::warn!("Could not read the APK cache; fetching from GitHub: {error}");
                    None
                }
                Err(error) => {
                    log::warn!("APK cache read task failed; fetching from GitHub: {error}");
                    None
                }
            }
        }
        None => None,
    };

    if !tasks.is_current(ticket) {
        return ApkLoadOutcome::Superseded;
    }

    if let Some(value) = cached.as_ref()
        && value.is_fresh
        && !force_refresh
    {
        return ApkLoadOutcome::Cached(fallback_from_cache(value));
    }

    on_cache_snapshot(cached.clone());
    if !tasks.is_current(ticket) {
        return ApkLoadOutcome::Superseded;
    }
    let cache_fallback = cached.as_ref().map(fallback_from_cache);
    let fallback = choose_fallback(memory_fallback, cache_fallback).map(|(fallback, _)| fallback);
    let fetched = fetcher().await;
    if !tasks.is_current(ticket) {
        return ApkLoadOutcome::Superseded;
    }

    match fetched {
        Ok(items) => {
            if let Some(cache) = cache
                && tasks.is_current(ticket)
            {
                let write_tasks = tasks.clone();
                let write_lock = Arc::clone(&cache_write_lock);
                let write_items = items.clone();
                match tokio::task::spawn_blocking(move || {
                    let _write_guard = lock_cache_write(&write_lock);
                    if write_tasks.is_current(ticket) {
                        cache.save(&write_items)
                    } else {
                        Ok(())
                    }
                })
                .await
                {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => {
                        log::warn!("Could not save the APK cache; keeping fetched data: {error}");
                    }
                    Err(error) => {
                        log::warn!("APK cache write task failed; keeping fetched data: {error}");
                    }
                }
            }
            ApkLoadOutcome::Updated(items)
        }
        Err(error) => ApkLoadOutcome::Failed {
            message: format!("Gagal mengambil daftar aplikasi: {error}"),
            fallback,
        },
    }
}

fn fallback_from_cache(cached: &CachedValue<Vec<ApkRelease>>) -> ApkFallback {
    let age = cache_age_if_known(cached);
    ApkFallback {
        items: cached.value.clone(),
        loaded_at: age.and_then(|age| Instant::now().checked_sub(age)),
        age,
    }
}

fn cache_age_if_known(cached: &CachedValue<Vec<ApkRelease>>) -> Option<Duration> {
    if cached.is_fresh || !cached.age.is_zero() {
        Some(cached.age)
    } else {
        None
    }
}

fn choose_fallback(
    memory: Option<ApkFallback>,
    persistent: Option<ApkFallback>,
) -> Option<(ApkFallback, bool)> {
    match (memory, persistent) {
        (Some(memory), Some(persistent)) => match (memory.age, persistent.age) {
            (Some(memory_age), Some(persistent_age)) if persistent_age < memory_age => {
                Some((persistent, true))
            }
            (Some(_), None) => Some((memory, false)),
            (None, Some(_)) => Some((persistent, true)),
            _ => Some((memory, false)),
        },
        (Some(memory), None) => Some((memory, false)),
        (None, Some(persistent)) => Some((persistent, true)),
        (None, None) => None,
    }
}

#[cfg(test)]
fn instant_from_cache(cached: &CachedValue<Vec<ApkRelease>>) -> Option<Instant> {
    cache_age_if_known(cached).and_then(|age| Instant::now().checked_sub(age))
}

fn lock_cache_write(lock: &Mutex<()>) -> MutexGuard<'_, ()> {
    lock.lock().unwrap_or_else(|poisoned| {
        log::error!("APK cache write lock was poisoned; recovering its contents.");
        poisoned.into_inner()
    })
}

fn deliver_apk_cache_snapshot(
    weak: slint::Weak<AppWindow>,
    state: Arc<Mutex<AppState>>,
    tasks: TaskRegistry,
    ticket: TaskTicket,
    cached: Option<CachedValue<Vec<ApkRelease>>>,
) {
    let delivery = slint::invoke_from_event_loop(move || {
        if let Some(update) = commit_apk_cache_snapshot(&state, &tasks, ticket, cached)
            && let Some(window) = weak.upgrade()
        {
            apply_apk_ui_update(&window, update);
        }
    });

    if let Err(error) = delivery {
        log::warn!("Could not deliver the APK cache snapshot to the UI: {error}");
    }
}

fn commit_apk_cache_snapshot(
    state: &Mutex<AppState>,
    tasks: &TaskRegistry,
    ticket: TaskTicket,
    cached: Option<CachedValue<Vec<ApkRelease>>>,
) -> Option<ApkUiUpdate> {
    if !tasks.is_current(ticket) {
        return None;
    }
    let mut state = lock_app_state(state);
    let memory = memory_fallback_from_state(&state.apks, Instant::now());
    let cache_fallback = cached.as_ref().map(fallback_from_cache);
    let selected = choose_fallback(memory, cache_fallback);
    let using_stale_cache = selected.as_ref().is_some_and(|(_, from_cache)| *from_cache)
        && cached.as_ref().is_some_and(|cached| !cached.is_fresh);
    let has_data = selected.is_some();
    let items = selected.and_then(|(fallback, from_cache)| {
        if from_cache {
            state.apks.items = fallback.items;
            state.apks.loaded_at = fallback.loaded_at;
            state.apks.has_loaded_data = true;
            Some(state.apks.items.iter().map(apk_item).collect())
        } else {
            None
        }
    });
    state.apks.is_loading = true;
    state.apks.is_cache_checking = false;
    state.apks.error = None;
    state.apks.data_status = match (using_stale_cache, has_data) {
        (true, _) => ApkDataStatus::RefreshingStaleCache,
        (false, true) => ApkDataStatus::Refreshing,
        (false, false) => ApkDataStatus::None,
    };
    Some(apk_ui_update(&state, items))
}

/// Hands a finished request over to the Slint event loop.
///
/// The guard travels inside the closure. If the closure runs it decides
/// whether the result is still current and then drops the guard; if delivery
/// fails or the closure is discarded, dropping it still releases the ticket.
fn deliver_apk_result(
    weak: slint::Weak<AppWindow>,
    state: Arc<Mutex<AppState>>,
    tasks: TaskRegistry,
    guard: TaskGuard,
    outcome: ApkLoadOutcome,
) {
    let ticket = guard.ticket();
    let delivery = slint::invoke_from_event_loop(move || {
        match commit_apk_result(&state, &guard, outcome) {
            Some(update) => {
                if let Some(window) = weak.upgrade() {
                    apply_apk_ui_update(&window, update);
                }
            }
            None => log::debug!("Discarded stale APK response for request {}.", ticket.id()),
        }
        drop(guard);
    });

    if let Err(error) = delivery {
        log::warn!("Could not deliver APK response to the UI event loop: {error}");
        tasks.finish(ticket);
    }
}

/// Applies a finished request to the shared state, but only if its ticket is
/// still the current one. Returns `None` for a stale request, leaving both the
/// state and the loading flag (now owned by the newer request) untouched.
fn commit_apk_result(
    state: &Mutex<AppState>,
    guard: &TaskGuard,
    outcome: ApkLoadOutcome,
) -> Option<ApkUiUpdate> {
    if matches!(&outcome, ApkLoadOutcome::Superseded) || !guard.is_current() {
        return None;
    }

    let mut state = lock_app_state(state);
    state.apks.is_loading = false;
    state.apks.is_cache_checking = false;
    match outcome {
        ApkLoadOutcome::Superseded => return None,
        ApkLoadOutcome::Cached(cached) => {
            state.apks.items = cached.items;
            state.apks.loaded_at = cached.loaded_at;
            state.apks.has_loaded_data = true;
            state.apks.error = None;
            state.apks.data_status = ApkDataStatus::Cached;
        }
        ApkLoadOutcome::Updated(items) => {
            state.apks.items = items;
            state.apks.error = None;
            state.apks.loaded_at = Some(Instant::now());
            state.apks.has_loaded_data = true;
            state.apks.data_status = ApkDataStatus::Updated;
        }
        ApkLoadOutcome::Failed { message, fallback } => {
            if let Some(fallback) = fallback {
                state.apks.items = fallback.items;
                state.apks.loaded_at = fallback.loaded_at;
                state.apks.has_loaded_data = true;
                state.apks.error = None;
                state.apks.data_status = ApkDataStatus::OfflineFallback;
            } else {
                state.apks.error = Some(message);
                if state.apks.has_loaded_data {
                    state.apks.data_status = if state.apks.loaded_at.is_some_and(|loaded_at| {
                        is_within_ttl(Some(loaded_at), Instant::now(), CachePolicy::STANDARD.ttl)
                    }) {
                        ApkDataStatus::Cached
                    } else {
                        ApkDataStatus::StaleCache
                    };
                } else {
                    state.apks.data_status = ApkDataStatus::None;
                }
            }
        }
    }
    let items = state.apks.items.iter().map(apk_item).collect();
    Some(apk_ui_update(&state, Some(items)))
}

fn apk_ui_update(state: &AppState, items: Option<Vec<ApkItem>>) -> ApkUiUpdate {
    ApkUiUpdate {
        items,
        error: state.apks.error.clone(),
        data_status: state.apks.data_status,
        is_loading: state.apks.is_loading,
        is_cache_checking: state.apks.is_cache_checking,
    }
}

fn apply_apk_ui_update(window: &AppWindow, update: ApkUiUpdate) {
    if let Some(items) = update.items {
        window.set_apks(ModelRc::new(VecModel::from(items)));
    }
    window.set_apks_error(update.error.unwrap_or_default().into());
    window.set_apks_status(update.data_status.label().into());
    window.set_apks_loading(update.is_loading);
    window.set_apks_checking_cache(update.is_cache_checking);
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
    use std::{
        fs,
        future::Future,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::{Duration, Instant},
    };

    use super::{
        ApkCache, ApkFallback, ApkLoadContext, ApkLoadOutcome, Controller,
        commit_apk_cache_snapshot, commit_apk_result, is_within_ttl, load_apks_with_cache,
        lock_app_state, memory_fallback_from_state,
    };
    use crate::{
        domain::models::ApkRelease,
        error::AppError,
        navigation::Screen,
        persistence::api_cache::{CacheLookup, CachePolicy, PersistentJsonCache},
        persistence::preferences::{
            GlassMode, Preferences, PreferencesStore,
            test_support::{TestDir, file_names},
        },
        runtime::{TaskKind, TaskRegistry},
        state::screens::{ApkDataStatus, ApksState},
    };
    use tokio::{
        runtime::{Builder, Runtime},
        sync::oneshot,
    };

    fn test_runtime() -> Runtime {
        Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime")
    }

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

    fn apk_cache(directory: &TestDir, name: &str) -> ApkCache {
        PersistentJsonCache::new(directory.path().join(name), CachePolicy::STANDARD)
    }

    fn stale_cache(cache: &ApkCache, items: &[ApkRelease]) {
        let contents = serde_json::json!({
            "saved_at_ms": 1,
            "data": items,
        });
        fs::write(cache.path(), serde_json::to_vec(&contents).unwrap()).unwrap();
    }

    fn request() -> (TaskRegistry, crate::runtime::TaskTicket) {
        let tasks = TaskRegistry::default();
        let ticket = tasks.try_begin(TaskKind::ApkFetch).unwrap();
        (tasks, ticket)
    }

    fn load_context(
        cache: Option<ApkCache>,
        tasks: TaskRegistry,
        ticket: crate::runtime::TaskTicket,
        force_refresh: bool,
        fallback: Option<ApkFallback>,
    ) -> ApkLoadContext {
        ApkLoadContext {
            cache,
            cache_write_lock: Arc::new(std::sync::Mutex::new(())),
            tasks,
            ticket,
            force_refresh,
            memory_fallback: fallback,
        }
    }

    fn run_load<F, Fut, C>(
        runtime: &Runtime,
        context: ApkLoadContext,
        fetcher: F,
        on_cache_snapshot: C,
    ) -> ApkLoadOutcome
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = crate::error::AppResult<Vec<ApkRelease>>> + Send + 'static,
        C: FnOnce(Option<super::CachedValue<Vec<ApkRelease>>>) + Send + 'static,
    {
        runtime.block_on(load_apks_with_cache(context, fetcher, on_cache_snapshot))
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
        assert_eq!(
            controller.lock_state().settings.glass_mode,
            GlassMode::Liquid
        );

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

        let update = commit_apk_result(
            &controller.state,
            &guard,
            ApkLoadOutcome::Updated(vec![apk("a"), apk("b")]),
        );
        assert!(
            matches!(&update, Some(update) if update.items.as_ref().is_some_and(|items| items.len() == 2))
        );
        if let Some(update) = &update {
            let items = update.items.as_ref().unwrap();
            assert_eq!(items[0].name.as_str(), "a");
            assert_eq!(items[1].download_url.as_str(), "https://example.test/b.apk");
        }

        {
            let state = controller.lock_state();
            assert_eq!(state.apks.items.len(), 2);
            assert!(!state.apks.is_loading);
            assert!(!state.apks.is_cache_checking);
            assert!(state.apks.error.is_none());
            assert!(state.apks.loaded_at.is_some());
            assert!(state.apks.has_loaded_data);
            assert_eq!(state.apks.data_status, super::ApkDataStatus::Updated);
        }

        drop(guard);
        assert!(!tasks.is_active(TaskKind::ApkFetch));
    }

    #[test]
    fn fresh_cache_commit_clears_checking_without_showing_loading() {
        let directory = TestDir::new("controller-fresh-cache-commit");
        let controller = new_controller(&directory);
        let tasks = TaskRegistry::default();
        let ticket = tasks.try_begin(TaskKind::ApkFetch).unwrap();
        let guard = tasks.guard(ticket);
        {
            let mut state = controller.lock_state();
            state.apks.is_loading = true;
            state.apks.is_cache_checking = true;
        }

        let cached = ApkFallback {
            items: vec![apk("cached")],
            loaded_at: Some(Instant::now()),
            age: Some(Duration::ZERO),
        };
        let update = commit_apk_result(&controller.state, &guard, ApkLoadOutcome::Cached(cached))
            .expect("current cache result commits");

        assert_eq!(update.data_status, super::ApkDataStatus::Cached);
        assert!(!update.is_loading);
        assert!(!update.is_cache_checking);
        assert_eq!(controller.lock_state().apks.items[0].name, "cached");
        assert!(controller.lock_state().apks.has_loaded_data);
    }

    #[test]
    fn empty_successful_cache_is_valid_loaded_data() {
        let directory = TestDir::new("apks-empty-cache-valid");
        let controller = new_controller(&directory);
        let cache = apk_cache(&directory, "apks.json");
        cache.save(&Vec::<ApkRelease>::new()).unwrap();
        let (tasks, ticket) = request();
        let outcome = run_load(
            &test_runtime(),
            load_context(Some(cache), tasks.clone(), ticket, false, None),
            || async { panic!("fresh empty cache must skip fetching") },
            |_| {},
        );
        let guard = tasks.guard(ticket);

        assert!(matches!(&outcome, ApkLoadOutcome::Cached(cached) if cached.items.is_empty()));
        commit_apk_result(&controller.state, &guard, outcome)
            .expect("valid empty cache result commits");

        let state = controller.lock_state();
        assert!(state.apks.has_loaded_data);
        assert!(state.apks.items.is_empty());
        assert!(state.apks.loaded_at.is_some());
        assert!(is_within_ttl(
            state.apks.loaded_at,
            Instant::now(),
            CachePolicy::STANDARD.ttl,
        ));
        assert_eq!(state.apks.data_status, ApkDataStatus::Cached);
    }

    #[test]
    fn unknown_timestamp_persistent_data_remains_a_memory_fallback() {
        let directory = TestDir::new("apks-unknown-timestamp-fallback");
        let controller = new_controller(&directory);
        let (tasks, original_ticket) = request();
        let old_cache = super::CachedValue {
            value: vec![apk("very-old-cache")],
            age: Duration::MAX,
            is_fresh: false,
        };
        commit_apk_cache_snapshot(&controller.state, &tasks, original_ticket, Some(old_cache))
            .expect("persistent snapshot is applied");
        {
            let state = controller.lock_state();
            assert!(state.apks.has_loaded_data);
            assert!(state.apks.loaded_at.is_none());
            assert_eq!(state.apks.items[0].name, "very-old-cache");
        }

        let ticket = tasks.begin(TaskKind::ApkFetch);
        let fallback = memory_fallback_from_state(&controller.lock_state().apks, Instant::now())
            .expect("loaded persistent data remains in memory");
        assert_eq!(fallback.items[0].name, "very-old-cache");
        assert_eq!(fallback.loaded_at, None);
        assert_eq!(fallback.age, None);

        let guard = tasks.guard(ticket);
        let outcome = run_load(
            &test_runtime(),
            load_context(None, tasks.clone(), ticket, true, Some(fallback)),
            || async { Err(AppError::DataDirectoryUnavailable) },
            |_| {},
        );
        assert!(matches!(
            &outcome,
            ApkLoadOutcome::Failed {
                fallback: Some(ApkFallback { items, loaded_at: None, age: None }),
                ..
            } if items[0].name == "very-old-cache"
        ));
        commit_apk_result(&controller.state, &guard, outcome)
            .expect("offline memory fallback commits");
        let state = controller.lock_state();
        assert!(state.apks.has_loaded_data);
        assert_eq!(state.apks.items[0].name, "very-old-cache");
        assert_eq!(state.apks.loaded_at, None);
        assert_eq!(state.apks.data_status, ApkDataStatus::OfflineFallback);
    }

    #[test]
    fn loaded_memory_without_timestamp_has_unknown_age() {
        let state = ApksState {
            items: vec![apk("untimed-memory")],
            has_loaded_data: true,
            loaded_at: None,
            ..ApksState::default()
        };

        let fallback =
            memory_fallback_from_state(&state, Instant::now()).expect("data is marked loaded");

        assert_eq!(fallback.items[0].name, "untimed-memory");
        assert_eq!(fallback.loaded_at, None);
        assert_eq!(fallback.age, None);
    }

    #[test]
    fn stale_cache_snapshot_is_shown_while_refresh_is_active() {
        let directory = TestDir::new("controller-stale-cache-snapshot");
        let controller = new_controller(&directory);
        let (tasks, ticket) = request();
        controller.lock_state().apks.is_cache_checking = true;
        let cached = super::CachedValue {
            value: vec![apk("stale")],
            age: Duration::from_secs(31 * 60),
            is_fresh: false,
        };

        let update = commit_apk_cache_snapshot(&controller.state, &tasks, ticket, Some(cached))
            .expect("active cache snapshot is applied");

        assert!(update.is_loading);
        assert!(!update.is_cache_checking);
        assert_eq!(
            update.data_status,
            super::ApkDataStatus::RefreshingStaleCache
        );
        assert_eq!(controller.lock_state().apks.items[0].name, "stale");
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
        let update = commit_apk_result(
            &controller.state,
            &first_guard,
            ApkLoadOutcome::Updated(vec![apk("old")]),
        );
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
        let update = commit_apk_result(
            &controller.state,
            &second_guard,
            ApkLoadOutcome::Updated(vec![apk("new")]),
        );
        assert!(
            matches!(&update, Some(update) if update.items.as_ref().is_some_and(|items| items.len() == 1))
        );
        assert_eq!(controller.lock_state().apks.items[0].name, "new");
        drop(second_guard);
        assert!(!tasks.is_active(TaskKind::ApkFetch));
    }

    #[test]
    fn failed_request_uses_previous_items_as_an_informational_fallback() {
        let directory = TestDir::new("controller-failure");
        let controller = new_controller(&directory);
        let tasks = TaskRegistry::default();

        let first = tasks.try_begin(TaskKind::ApkFetch).unwrap();
        let first_guard = tasks.guard(first);
        assert!(
            commit_apk_result(
                &controller.state,
                &first_guard,
                ApkLoadOutcome::Updated(vec![apk("kept")]),
            )
            .is_some()
        );
        drop(first_guard);

        controller.set_apks_loading(true);
        let second = tasks.try_begin(TaskKind::ApkFetch).unwrap();
        let second_guard = tasks.guard(second);
        let loaded_at = controller.lock_state().apks.loaded_at;
        let update = commit_apk_result(
            &controller.state,
            &second_guard,
            ApkLoadOutcome::Failed {
                message: "boom".to_owned(),
                fallback: Some(ApkFallback {
                    items: vec![apk("kept")],
                    loaded_at,
                    age: loaded_at
                        .map(|loaded_at| Instant::now().saturating_duration_since(loaded_at)),
                }),
            },
        );
        assert!(matches!(&update, Some(update) if update.error.is_none()));

        let state = lock_app_state(&controller.state);
        assert_eq!(state.apks.items.len(), 1);
        assert!(state.apks.error.is_none());
        assert_eq!(
            state.apks.data_status,
            super::ApkDataStatus::OfflineFallback
        );
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

        assert!(
            commit_apk_result(
                &controller.state,
                &guard,
                ApkLoadOutcome::Updated(vec![apk("late")]),
            )
            .is_none()
        );
        assert!(controller.lock_state().apks.items.is_empty());
    }

    #[test]
    fn fresh_persistent_cache_skips_github_and_preserves_its_age() {
        let directory = TestDir::new("apks-cache-fresh");
        let cache = apk_cache(&directory, "apks.json");
        cache.save(&vec![apk("cached")]).unwrap();
        let (tasks, ticket) = request();
        let fetch_count = Arc::new(AtomicUsize::new(0));
        let fetch_count_for_call = Arc::clone(&fetch_count);
        let outcome = run_load(
            &test_runtime(),
            load_context(Some(cache), tasks, ticket, false, None),
            move || async move {
                fetch_count_for_call.fetch_add(1, Ordering::SeqCst);
                Ok(vec![apk("network")])
            },
            |_| {},
        );

        assert!(
            matches!(outcome, ApkLoadOutcome::Cached(cached) if cached.items[0].name == "cached" && cached.loaded_at.is_some())
        );
        assert_eq!(fetch_count.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn stale_cache_is_delivered_before_the_background_refresh_finishes() {
        let directory = TestDir::new("apks-cache-stale");
        let cache = apk_cache(&directory, "apks.json");
        stale_cache(&cache, &[apk("old")]);
        let (tasks, ticket) = request();
        let runtime = test_runtime();
        let (snapshot_tx, snapshot_rx) = oneshot::channel();
        let (fetch_tx, fetch_rx) = oneshot::channel();
        let task = runtime.spawn(load_apks_with_cache(
            load_context(Some(cache), tasks, ticket, false, None),
            move || async move { fetch_rx.await.expect("release fake fetch") },
            move |snapshot| snapshot_tx.send(snapshot).expect("send cached snapshot"),
        ));

        let snapshot = runtime
            .block_on(snapshot_rx)
            .expect("cache snapshot arrives while fetch is pending")
            .expect("stale cache entry");
        assert_eq!(snapshot.value[0].name, "old");
        assert!(!snapshot.is_fresh);

        fetch_tx.send(Ok(vec![apk("new")])).unwrap();
        assert!(matches!(
            runtime.block_on(task).unwrap(),
            ApkLoadOutcome::Updated(items) if items[0].name == "new"
        ));
    }

    #[test]
    fn successful_refresh_replaces_the_persistent_cache() {
        let directory = TestDir::new("apks-cache-save");
        let cache = apk_cache(&directory, "apks.json");
        let (tasks, ticket) = request();
        let outcome = run_load(
            &test_runtime(),
            load_context(Some(cache.clone()), tasks, ticket, false, None),
            || async { Ok(vec![apk("latest")]) },
            |_| {},
        );

        assert!(matches!(outcome, ApkLoadOutcome::Updated(items) if items[0].name == "latest"));
        assert!(matches!(
            cache.load().unwrap(),
            CacheLookup::Hit(cached) if cached.is_fresh && cached.value[0].name == "latest"
        ));
    }

    #[test]
    fn request_failure_uses_stale_cache_as_offline_fallback() {
        let directory = TestDir::new("apks-cache-offline");
        let cache = apk_cache(&directory, "apks.json");
        stale_cache(&cache, &[apk("offline")]);
        let (tasks, ticket) = request();
        let outcome = run_load(
            &test_runtime(),
            load_context(Some(cache), tasks, ticket, false, None),
            || async { Err(AppError::DataDirectoryUnavailable) },
            |_| {},
        );

        assert!(matches!(
            &outcome,
            ApkLoadOutcome::Failed {
                fallback: Some(ApkFallback {
                    items,
                    loaded_at: Some(_),
                    ..
                }),
                ..
            } if items[0].name == "offline"
        ));
    }

    #[test]
    fn request_failure_without_cache_preserves_the_network_error() {
        let directory = TestDir::new("apks-request-failure-no-data");
        let controller = new_controller(&directory);
        let (tasks, ticket) = request();
        let guard = tasks.guard(ticket);
        let outcome = run_load(
            &test_runtime(),
            load_context(None, tasks.clone(), ticket, false, None),
            || async { Err(AppError::DataDirectoryUnavailable) },
            |_| {},
        );

        assert!(matches!(
            &outcome,
            ApkLoadOutcome::Failed { message, fallback: None }
                if message.contains("Gagal mengambil daftar aplikasi")
        ));
        commit_apk_result(&controller.state, &guard, outcome)
            .expect("current failure is committed");
        let state = controller.lock_state();
        assert!(!state.apks.has_loaded_data);
        assert!(state.apks.items.is_empty());
        assert_eq!(state.apks.data_status, ApkDataStatus::None);
        assert!(state.apks.error.is_some());
    }

    #[test]
    fn missing_cache_continues_to_the_injected_fetcher() {
        let directory = TestDir::new("apks-cache-missing");
        let cache = apk_cache(&directory, "apks.json");
        let (tasks, ticket) = request();
        let fetch_count = Arc::new(AtomicUsize::new(0));
        let fetch_count_for_call = Arc::clone(&fetch_count);
        let outcome = run_load(
            &test_runtime(),
            load_context(Some(cache), tasks, ticket, false, None),
            move || async move {
                fetch_count_for_call.fetch_add(1, Ordering::SeqCst);
                Ok(vec![apk("fetched")])
            },
            |_| {},
        );

        assert!(matches!(outcome, ApkLoadOutcome::Updated(items) if items[0].name == "fetched"));
        assert_eq!(fetch_count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn corrupted_cache_is_quarantined_and_does_not_block_fetching() {
        let directory = TestDir::new("apks-cache-corrupt");
        let cache = apk_cache(&directory, "apks.json");
        fs::write(cache.path(), b"not-json").unwrap();
        let (tasks, ticket) = request();
        let outcome = run_load(
            &test_runtime(),
            load_context(Some(cache), tasks, ticket, false, None),
            || async { Ok(vec![apk("recovered")]) },
            |_| {},
        );

        assert!(matches!(outcome, ApkLoadOutcome::Updated(items) if items[0].name == "recovered"));
        assert!(
            file_names(directory.path())
                .iter()
                .any(|name| name.starts_with("apks.json.corrupt-"))
        );
    }

    #[test]
    fn cache_read_error_does_not_block_fetching() {
        let directory = TestDir::new("apks-cache-read-error");
        let path = directory.path().join("apks.json");
        fs::create_dir(&path).unwrap();
        let cache = PersistentJsonCache::new(path, CachePolicy::STANDARD);
        let (tasks, ticket) = request();
        let outcome = run_load(
            &test_runtime(),
            load_context(Some(cache), tasks, ticket, false, None),
            || async { Ok(vec![apk("read-error-recovered")]) },
            |_| {},
        );

        assert!(matches!(
            outcome,
            ApkLoadOutcome::Updated(items) if items[0].name == "read-error-recovered"
        ));
    }

    #[test]
    fn cache_write_error_does_not_discard_successful_http_data() {
        let directory = TestDir::new("apks-cache-write-error");
        let path = directory.path().join("apks.json");
        fs::create_dir(&path).unwrap();
        let cache = PersistentJsonCache::new(path, CachePolicy::STANDARD);
        let (tasks, ticket) = request();
        let outcome = run_load(
            &test_runtime(),
            load_context(Some(cache), tasks, ticket, false, None),
            || async { Ok(vec![apk("write-error-still-shown")]) },
            |_| {},
        );

        assert!(matches!(
            outcome,
            ApkLoadOutcome::Updated(items) if items[0].name == "write-error-still-shown"
        ));
    }

    #[test]
    fn manual_refresh_bypasses_a_fresh_persistent_cache() {
        let directory = TestDir::new("apks-cache-force");
        let cache = apk_cache(&directory, "apks.json");
        cache.save(&vec![apk("cached")]).unwrap();
        let (tasks, ticket) = request();
        let fetch_count = Arc::new(AtomicUsize::new(0));
        let fetch_count_for_call = Arc::clone(&fetch_count);
        let outcome = run_load(
            &test_runtime(),
            load_context(Some(cache), tasks, ticket, true, None),
            move || async move {
                fetch_count_for_call.fetch_add(1, Ordering::SeqCst);
                Ok(vec![apk("forced")])
            },
            |_| {},
        );

        assert!(matches!(outcome, ApkLoadOutcome::Updated(items) if items[0].name == "forced"));
        assert_eq!(fetch_count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn stale_request_finishing_after_supersession_does_not_write_cache() {
        let directory = TestDir::new("apks-cache-stale-write");
        let cache = apk_cache(&directory, "apks.json");
        cache.save(&vec![apk("already-latest")]).unwrap();
        let (tasks, ticket) = request();
        let runtime = test_runtime();
        let (fetch_started_tx, fetch_started_rx) = oneshot::channel();
        let (fetch_result_tx, fetch_result_rx) = oneshot::channel();
        let old_tasks = tasks.clone();
        let old_cache = cache.clone();
        let old_task = runtime.spawn(load_apks_with_cache(
            load_context(Some(old_cache), old_tasks, ticket, true, None),
            move || async move {
                fetch_started_tx.send(()).expect("signal fetch started");
                fetch_result_rx.await.expect("release fake fetch")
            },
            |_| {},
        ));

        runtime
            .block_on(fetch_started_rx)
            .expect("fetch is in progress");
        let replacement = tasks.begin(TaskKind::ApkFetch);
        fetch_result_tx
            .send(Ok(vec![apk("stale-response")]))
            .expect("send late successful response");

        assert!(matches!(
            runtime.block_on(old_task).unwrap(),
            ApkLoadOutcome::Superseded
        ));
        assert!(matches!(
            cache.load().unwrap(),
            CacheLookup::Hit(cached) if cached.value[0].name == "already-latest"
        ));
        assert!(tasks.is_current(replacement));
    }

    #[test]
    fn stale_cache_snapshot_is_ignored_after_ticket_replacement() {
        let directory = TestDir::new("apks-stale-snapshot");
        let controller = new_controller(&directory);
        let (tasks, ticket) = request();
        {
            let mut state = controller.lock_state();
            state.apks.items = vec![apk("new-memory")];
            state.apks.loaded_at = Some(Instant::now());
            state.apks.is_loading = true;
            state.apks.data_status = super::ApkDataStatus::Refreshing;
        }
        tasks.begin(TaskKind::ApkFetch);
        let cached = super::CachedValue {
            value: vec![apk("old-cache")],
            age: Duration::from_secs(40 * 60),
            is_fresh: false,
        };

        let update = commit_apk_cache_snapshot(&controller.state, &tasks, ticket, Some(cached));

        assert!(update.is_none());
        let state = controller.lock_state();
        assert_eq!(state.apks.items[0].name, "new-memory");
        assert!(state.apks.is_loading);
        assert_eq!(state.apks.data_status, super::ApkDataStatus::Refreshing);
    }

    #[test]
    fn memory_fallback_prefers_newer_persistent_cache() {
        let directory = TestDir::new("apks-newer-disk-fallback");
        let cache = apk_cache(&directory, "apks.json");
        cache.save(&vec![apk("new-disk")]).unwrap();
        let (tasks, ticket) = request();
        let memory_loaded_at = Instant::now() - Duration::from_secs(60);
        let memory = ApkFallback {
            items: vec![apk("old-memory")],
            loaded_at: Some(memory_loaded_at),
            age: Some(Duration::from_secs(60)),
        };

        let outcome = run_load(
            &test_runtime(),
            load_context(Some(cache), tasks, ticket, true, Some(memory)),
            || async { Err(AppError::DataDirectoryUnavailable) },
            |_| {},
        );

        assert!(matches!(
            outcome,
            ApkLoadOutcome::Failed {
                fallback: Some(ApkFallback { items, .. }),
                ..
            } if items[0].name == "new-disk"
        ));
    }

    #[test]
    fn persistent_fallback_prefers_newer_memory() {
        let directory = TestDir::new("apks-newer-memory-fallback");
        let cache = apk_cache(&directory, "apks.json");
        stale_cache(&cache, &[apk("old-disk")]);
        let (tasks, ticket) = request();
        let memory_loaded_at = Instant::now();
        let memory = ApkFallback {
            items: vec![apk("new-memory")],
            loaded_at: Some(memory_loaded_at),
            age: Some(Duration::ZERO),
        };

        let outcome = run_load(
            &test_runtime(),
            load_context(Some(cache), tasks, ticket, true, Some(memory)),
            || async { Err(AppError::DataDirectoryUnavailable) },
            |_| {},
        );

        assert!(matches!(
            &outcome,
            ApkLoadOutcome::Failed {
                fallback: Some(ApkFallback { items, loaded_at: Some(_), .. }),
                ..
            } if items[0].name == "new-memory"
        ));
    }

    #[test]
    fn empty_successful_memory_result_is_valid_fallback() {
        let directory = TestDir::new("apks-empty-memory-fallback");
        let controller = new_controller(&directory);
        let tasks = TaskRegistry::default();
        let memory_loaded_at = Instant::now();
        {
            let mut state = controller.lock_state();
            state.apks.loaded_at = Some(memory_loaded_at);
            state.apks.data_status = super::ApkDataStatus::Updated;
        }
        let first = tasks.try_begin(TaskKind::ApkFetch).unwrap();
        let first_guard = tasks.guard(first);
        drop(first_guard);
        let second = tasks.try_begin(TaskKind::ApkFetch).unwrap();
        let second_guard = tasks.guard(second);
        let memory = ApkFallback {
            items: Vec::new(),
            loaded_at: Some(memory_loaded_at),
            age: Some(Duration::ZERO),
        };

        let outcome = run_load(
            &test_runtime(),
            load_context(None, tasks, second, true, Some(memory)),
            || async { Err(AppError::DataDirectoryUnavailable) },
            |_| {},
        );
        assert!(matches!(
            &outcome,
            ApkLoadOutcome::Failed {
                fallback: Some(ApkFallback { items, loaded_at: Some(_), .. }),
                ..
            } if items.is_empty()
        ));

        let update = commit_apk_result(&controller.state, &second_guard, outcome);
        assert!(update.is_some());
        let state = controller.lock_state();
        assert!(state.apks.items.is_empty());
        assert!(state.apks.loaded_at.is_some());
        assert_eq!(
            state.apks.data_status,
            super::ApkDataStatus::OfflineFallback
        );
    }

    #[test]
    fn cache_snapshot_does_not_replace_newer_memory_state() {
        let directory = TestDir::new("apks-snapshot-newer-memory");
        let controller = new_controller(&directory);
        let (tasks, ticket) = request();
        let memory_loaded_at = Instant::now();
        {
            let mut state = controller.lock_state();
            state.apks.items = vec![apk("new-memory")];
            state.apks.loaded_at = Some(memory_loaded_at);
            state.apks.has_loaded_data = true;
            state.apks.is_loading = true;
        }
        let cached = super::CachedValue {
            value: vec![apk("old-cache")],
            age: Duration::from_secs(45 * 60),
            is_fresh: false,
        };

        let update = commit_apk_cache_snapshot(&controller.state, &tasks, ticket, Some(cached))
            .expect("active cache snapshot is applied");

        assert!(update.items.is_none());
        let state = controller.lock_state();
        assert_eq!(state.apks.items[0].name, "new-memory");
        assert_eq!(state.apks.loaded_at, Some(memory_loaded_at));
        assert!(state.apks.has_loaded_data);
    }

    #[test]
    fn persistent_cache_age_is_not_reset_when_mapped_to_monotonic_time() {
        let age = Duration::from_secs(29 * 60 + 59);
        let cached = super::CachedValue {
            value: vec![apk("near-expiry")],
            age,
            is_fresh: true,
        };
        let loaded_at = super::instant_from_cache(&cached).expect("fresh age fits in Instant");

        assert!(is_within_ttl(
            Some(loaded_at),
            Instant::now(),
            CachePolicy::STANDARD.ttl
        ));
        assert!(!is_within_ttl(
            Some(loaded_at),
            Instant::now() + Duration::from_secs(2),
            CachePolicy::STANDARD.ttl
        ));
    }

    #[test]
    fn window_shutdown_clears_loading_and_cache_checking_state() {
        let directory = TestDir::new("apks-window-close");
        let controller = new_controller(&directory);
        let (tasks, ticket) = request();
        let guard = tasks.guard(ticket);
        {
            let mut state = controller.lock_state();
            state.apks.is_loading = true;
            state.apks.is_cache_checking = true;
            state.apks.has_loaded_data = true;
            state.apks.items.clear();
            state.apks.loaded_at = Some(Instant::now());
            state.apks.data_status = super::ApkDataStatus::Refreshing;
        }

        controller.reset_apks_request_state();

        let state = controller.lock_state();
        assert!(!state.apks.is_loading);
        assert!(!state.apks.is_cache_checking);
        assert!(state.apks.has_loaded_data);
        assert!(state.apks.items.is_empty());
        assert_eq!(state.apks.data_status, ApkDataStatus::Cached);
        drop(state);
        drop(guard);

        {
            let mut state = controller.lock_state();
            state.apks.is_loading = true;
            state.apks.data_status = ApkDataStatus::Refreshing;
            state.apks.loaded_at = None;
        }
        controller.reset_apks_request_state();
        let state = controller.lock_state();
        assert!(state.apks.has_loaded_data);
        assert_eq!(state.apks.data_status, ApkDataStatus::StaleCache);
    }
}
