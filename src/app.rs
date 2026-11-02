use std::{error::Error, sync::Arc};

use directories::ProjectDirs;
use slint::{ComponentHandle, ModelRc, VecModel};
use tokio::runtime::Handle;

use crate::{
    ApkItem, AppWindow,
    controller::Controller,
    navigation::Screen,
    persistence::{
        api_cache::{CachePolicy, PersistentJsonCache},
        preferences::PreferencesStore,
    },
    platform,
    runtime::TaskRegistry,
    services::github,
};

pub fn run(runtime: Handle) -> Result<(), Box<dyn Error>> {
    let preferences = PreferencesStore::new()?;
    let initial_preferences = preferences.load()?;
    let apk_cache = ProjectDirs::from("com", "MirzaDev", "OneCenter").map(|dirs| {
        PersistentJsonCache::new(dirs.cache_dir().join("apks.json"), CachePolicy::STANDARD)
    });
    if apk_cache.is_none() {
        log::warn!("Application cache directory is unavailable; APK caching is disabled.");
    }
    let controller =
        Arc::new(Controller::new(preferences, initial_preferences).with_apk_cache(apk_cache));
    // One registry for the whole application; every callback gets a clone of
    // the same shared handle.
    let tasks = TaskRegistry::default();
    let client = github::http_client()?;

    let window = AppWindow::new()?;
    window.set_apks(ModelRc::new(VecModel::from(Vec::<ApkItem>::new())));
    if let Ok(state) = controller.state().lock() {
        window.set_information_message(state.home.message.clone().into());
        window.set_private_unavailable_reason(state.private.unavailable_reason.clone().into());
        window.set_glass_mode(state.settings.glass_mode.index());
    } else {
        return Err(std::io::Error::other("Application state could not be initialized.").into());
    }

    let navigate_controller = Arc::clone(&controller);
    let navigate_runtime = runtime.clone();
    let navigate_tasks = tasks.clone();
    let navigate_client = client.clone();
    let navigate_weak = window.as_weak();
    window.on_navigate(move |index| {
        let Some(screen) = navigate_controller.select_screen(index) else {
            log::warn!("Rejected unknown screen index: {index}");
            return;
        };
        let Some(window) = navigate_weak.upgrade() else {
            return;
        };
        window.set_active_screen(screen.index());
        window.set_selected_index(screen.index());

        if screen == Screen::Apks {
            navigate_controller.load_apks(
                &window,
                &navigate_runtime,
                &navigate_tasks,
                navigate_client.clone(),
                false,
            );
        }
    });

    let refresh_controller = Arc::clone(&controller);
    let refresh_runtime = runtime.clone();
    let refresh_tasks = tasks.clone();
    let refresh_client = client.clone();
    let refresh_weak = window.as_weak();
    window.on_refresh_apks(move || {
        if let Some(window) = refresh_weak.upgrade() {
            refresh_controller.load_apks(
                &window,
                &refresh_runtime,
                &refresh_tasks,
                refresh_client.clone(),
                true,
            );
        }
    });

    let download_weak = window.as_weak();
    window.on_download_apk(move |url| {
        open_external_url(url.as_str(), &download_weak, true);
    });

    let open_url_weak = window.as_weak();
    window.on_open_external_url(move |url| {
        open_external_url(url.as_str(), &open_url_weak, false);
    });

    let settings_controller = Arc::clone(&controller);
    let settings_weak = window.as_weak();
    window.on_glass_mode_selected(move |index| {
        match settings_controller.update_glass_mode(index) {
            Ok(mode) => {
                if let Some(window) = settings_weak.upgrade() {
                    window.set_glass_mode(mode.index());
                    window.set_settings_message("".into());
                }
            }
            Err(message) => {
                log::error!("Could not save visual preferences: {message}");
                settings_controller.set_settings_error(message.clone());
                if let Some(window) = settings_weak.upgrade() {
                    window.set_settings_message(message.into());
                }
            }
        }
    });

    let result = window.run();
    // The window is closed: abort whatever is still in flight. Only tasks are
    // aborted here; the shared Tokio runtime is shut down by `main` afterwards.
    let cancelled = tasks.cancel_all();
    controller.reset_apks_request_state();
    if cancelled > 0 {
        log::debug!("Cancelled {cancelled} background task(s) on exit.");
    }
    result?;
    Ok(())
}

fn open_external_url(url: &str, window: &slint::Weak<AppWindow>, is_download: bool) {
    match platform::open_url(url) {
        Ok(()) if !is_download => {
            if let Some(window) = window.upgrade() {
                window.set_home_action_message("".into());
            }
        }
        Ok(()) => {}
        Err(message) => {
            log::error!("{message}");
            if let Some(window) = window.upgrade() {
                if is_download {
                    window.set_apks_error(message.into());
                } else {
                    window.set_home_action_message(message.into());
                }
            }
        }
    }
}
