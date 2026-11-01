use std::{
    ffi::{OsStr, OsString},
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

use crate::error::{AppError, AppResult};

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GlassMode {
    #[default]
    Off,
    Frosted,
    Liquid,
}

impl GlassMode {
    pub const fn index(self) -> i32 {
        match self {
            Self::Off => 0,
            Self::Frosted => 1,
            Self::Liquid => 2,
        }
    }

    pub fn from_index(index: i32) -> Option<Self> {
        match index {
            0 => Some(Self::Off),
            1 => Some(Self::Frosted),
            2 => Some(Self::Liquid),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Preferences {
    #[serde(default)]
    pub glass_mode: GlassMode,
}

#[derive(Clone, Debug)]
pub struct PreferencesStore {
    path: PathBuf,
}

impl PreferencesStore {
    pub fn new() -> AppResult<Self> {
        let dirs = ProjectDirs::from("com", "MirzaDev", "OneCenter")
            .ok_or(AppError::DataDirectoryUnavailable)?;
        Ok(Self {
            path: dirs.config_dir().join("preferences.json"),
        })
    }

    /// Creates a store backed by an explicit file path.
    ///
    /// Useful for tests and for platforms (such as Android) where the data
    /// directory is not discovered through `ProjectDirs`.
    pub fn with_path(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Loads the stored preferences.
    ///
    /// * A missing file yields the defaults.
    /// * A file that is not valid preferences JSON never fails startup: it is
    ///   moved aside to a unique `<name>.corrupt-*` file (best effort), the
    ///   problem is logged, and the defaults are returned.
    /// * Genuine I/O failures (for example permission errors) are still
    ///   reported as [`AppError::PreferencesRead`].
    pub fn load(&self) -> AppResult<Preferences> {
        let data = match fs::read(&self.path) {
            Ok(data) => data,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(Preferences::default());
            }
            Err(error) => return Err(AppError::PreferencesRead(error)),
        };

        match serde_json::from_slice::<Preferences>(&data) {
            Ok(preferences) => Ok(preferences),
            Err(error) => {
                log::error!(
                    "Preferences file {} is corrupted ({error}); falling back to defaults.",
                    self.path.display()
                );
                match quarantine_corrupt_file(&self.path) {
                    Ok(quarantined) => log::warn!(
                        "Corrupted preferences were preserved at {}.",
                        quarantined.display()
                    ),
                    Err(error) => log::error!(
                        "Could not set aside the corrupted preferences file {}: {error}",
                        self.path.display()
                    ),
                }
                Ok(Preferences::default())
            }
        }
    }

    /// Saves the preferences using a temporary file in the same directory.
    ///
    /// The previous file stays untouched until the new contents have been
    /// written and flushed to disk. See `write_file_replacing` in this module
    /// for the platform behavior of the final replacement step.
    pub fn save(&self, preferences: &Preferences) -> AppResult<()> {
        if self.path.parent().is_none() || self.path.file_name().is_none() {
            return Err(AppError::DataDirectoryUnavailable);
        }
        let data = serde_json::to_vec_pretty(preferences)?;
        write_file_replacing(&self.path, &data).map_err(AppError::PreferencesWrite)
    }
}

/// Writes `data` to `path` by way of a temporary file in the same directory.
///
/// Sequence: create the parent directory, create a uniquely named temporary
/// file with `create_new`, write the data, `sync_all` the file, close it, and
/// finally `fs::rename` it over the destination. If any step fails the
/// temporary file is removed and the destination keeps its previous contents.
///
/// Platform notes:
/// * Unix: `rename` is an atomic replacement, and the parent directory is
///   flushed on a best-effort basis afterwards.
/// * Windows: `std::fs::rename` replaces an existing destination file (it uses
///   `MoveFileExW` / `SetFileInformationByHandle` internally). There is no
///   delete-then-rename step, so the destination is never absent because of
///   this code. Std does not document the replacement as strictly atomic on
///   Windows, and it can fail with an access-denied style error if another
///   process holds the destination open without delete sharing; in that case
///   the error is returned and the old file is kept.
pub(crate) fn write_file_replacing(path: &Path, data: &[u8]) -> io::Result<()> {
    let file_name = path.file_name().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "destination has no file name")
    })?;
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "destination has no parent directory",
        )
    })?;
    fs::create_dir_all(parent)?;

    let (temp_path, file) = create_temp_file(parent, file_name)?;
    match write_then_replace(file, &temp_path, path, data) {
        Ok(()) => {
            sync_directory(parent);
            Ok(())
        }
        Err(error) => {
            remove_temp_file(&temp_path);
            Err(error)
        }
    }
}

/// Moves a corrupted file next to its original location under a unique
/// `<file name>.corrupt-<suffix>` name and returns the new path.
pub(crate) fn quarantine_corrupt_file(path: &Path) -> io::Result<PathBuf> {
    let file_name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no file name"))?;
    let parent = path.parent().unwrap_or_else(|| Path::new(""));

    let mut quarantined_name = file_name.to_os_string();
    quarantined_name.push(format!(".corrupt-{}", unique_suffix()));
    let candidate = parent.join(quarantined_name);
    if candidate.exists() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "quarantine destination already exists",
        ));
    }
    fs::rename(path, &candidate)?;
    Ok(candidate)
}

fn create_temp_file(directory: &Path, file_name: &OsStr) -> io::Result<(PathBuf, File)> {
    let mut collisions = 0u8;
    loop {
        let mut name = OsString::from(".");
        name.push(file_name);
        name.push(format!(".tmp-{}", unique_suffix()));
        let path = directory.join(name);

        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists && collisions < 8 => {
                collisions += 1;
            }
            Err(error) => return Err(error),
        }
    }
}

fn write_then_replace(
    mut file: File,
    temp_path: &Path,
    destination: &Path,
    data: &[u8],
) -> io::Result<()> {
    file.write_all(data)?;
    file.sync_all()?;
    // Close the handle before renaming; Windows is stricter about open files.
    drop(file);
    fs::rename(temp_path, destination)
}

fn remove_temp_file(path: &Path) {
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => log::warn!(
            "Could not remove temporary file {}: {error}",
            path.display()
        ),
    }
}

#[cfg(unix)]
fn sync_directory(directory: &Path) {
    if let Err(error) = File::open(directory).and_then(|handle| handle.sync_all()) {
        log::debug!(
            "Could not flush directory {} after replacing a file: {error}",
            directory.display()
        );
    }
}

#[cfg(not(unix))]
fn sync_directory(_directory: &Path) {}

/// Process-unique suffix: wall-clock nanoseconds, process id and a counter.
fn unique_suffix() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{nanos}-{}-{sequence}", std::process::id())
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::{
        fs,
        path::{Path, PathBuf},
    };

    /// Uniquely named temporary directory that is removed on drop, so tests
    /// can run concurrently without colliding.
    pub(crate) struct TestDir {
        path: PathBuf,
    }

    impl TestDir {
        pub(crate) fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "mirzadev-onecenter-{label}-{}",
                super::unique_suffix()
            ));
            fs::create_dir_all(&path).expect("create test directory");
            Self { path }
        }

        pub(crate) fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    /// Sorted names of the entries directly inside `directory`.
    pub(crate) fn file_names(directory: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(directory)
            .expect("read test directory")
            .map(|entry| {
                entry
                    .expect("read directory entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        names.sort();
        names
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::{
        GlassMode, Preferences, PreferencesStore,
        test_support::{TestDir, file_names},
    };
    use crate::error::AppError;

    fn store_in(directory: &TestDir) -> PreferencesStore {
        PreferencesStore::with_path(directory.path().join("preferences.json"))
    }

    #[test]
    fn missing_preferences_use_defaults_and_create_nothing() {
        let directory = TestDir::new("prefs-missing");
        let store = store_in(&directory);

        assert_eq!(store.load().unwrap().glass_mode, GlassMode::Off);
        assert!(file_names(directory.path()).is_empty());
    }

    #[test]
    fn default_mode_is_off() {
        assert_eq!(GlassMode::default(), GlassMode::Off);
        assert_eq!(Preferences::default().glass_mode, GlassMode::Off);
        assert_eq!(GlassMode::from_index(0), Some(GlassMode::Off));
        assert_eq!(GlassMode::from_index(3), None);
    }

    #[test]
    fn every_mode_round_trips_through_the_store() {
        let directory = TestDir::new("prefs-roundtrip");
        let store = store_in(&directory);

        for mode in [GlassMode::Off, GlassMode::Frosted, GlassMode::Liquid] {
            store.save(&Preferences { glass_mode: mode }).unwrap();
            assert_eq!(store.load().unwrap().glass_mode, mode);
            assert_eq!(GlassMode::from_index(mode.index()), Some(mode));
        }
    }

    #[test]
    fn saved_file_keeps_the_documented_format() {
        let directory = TestDir::new("prefs-format");
        let store = store_in(&directory);
        let path = directory.path().join("preferences.json");

        store
            .save(&Preferences {
                glass_mode: GlassMode::Frosted,
            })
            .unwrap();
        let raw: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(raw, serde_json::json!({ "glass_mode": "frosted" }));

        fs::write(&path, br#"{ "glass_mode": "liquid" }"#).unwrap();
        assert_eq!(store.load().unwrap().glass_mode, GlassMode::Liquid);
    }

    #[test]
    fn missing_field_uses_default_without_quarantine() {
        let directory = TestDir::new("prefs-missing-field");
        let store = store_in(&directory);
        fs::write(directory.path().join("preferences.json"), b"{}").unwrap();

        assert_eq!(store.load().unwrap().glass_mode, GlassMode::Off);
        assert_eq!(file_names(directory.path()), vec!["preferences.json"]);
    }

    #[test]
    fn malformed_json_falls_back_to_defaults_and_preserves_the_file() {
        let directory = TestDir::new("prefs-malformed");
        let store = store_in(&directory);
        let path = directory.path().join("preferences.json");
        fs::write(&path, b"{ not valid json").unwrap();

        let loaded = store
            .load()
            .expect("startup must survive corrupt preferences");
        assert_eq!(loaded.glass_mode, GlassMode::Off);
        assert!(!path.exists());

        let names = file_names(directory.path());
        assert_eq!(names.len(), 1);
        assert!(names[0].starts_with("preferences.json.corrupt-"));
        assert_eq!(
            fs::read(directory.path().join(&names[0])).unwrap(),
            b"{ not valid json"
        );
    }

    #[test]
    fn empty_and_unknown_value_files_are_treated_as_corrupt() {
        let directory = TestDir::new("prefs-invalid-values");
        let store = store_in(&directory);
        let path = directory.path().join("preferences.json");

        fs::write(&path, b"").unwrap();
        assert_eq!(store.load().unwrap().glass_mode, GlassMode::Off);

        fs::write(&path, br#"{ "glass_mode": "does-not-exist" }"#).unwrap();
        assert_eq!(store.load().unwrap().glass_mode, GlassMode::Off);

        assert!(!path.exists());
        assert_eq!(file_names(directory.path()).len(), 2);
    }

    #[test]
    fn repeated_corruption_creates_distinct_quarantine_files() {
        let directory = TestDir::new("prefs-repeated");
        let store = store_in(&directory);
        let path = directory.path().join("preferences.json");

        for _ in 0..3 {
            fs::write(&path, b"[broken").unwrap();
            store.load().unwrap();
        }

        let names = file_names(directory.path());
        assert_eq!(names.len(), 3);
        assert!(
            names
                .iter()
                .all(|name| name.starts_with("preferences.json.corrupt-"))
        );
    }

    #[test]
    fn saving_after_corruption_recovers_the_store() {
        let directory = TestDir::new("prefs-recover");
        let store = store_in(&directory);
        fs::write(directory.path().join("preferences.json"), b"garbage").unwrap();

        assert_eq!(store.load().unwrap().glass_mode, GlassMode::Off);
        store
            .save(&Preferences {
                glass_mode: GlassMode::Liquid,
            })
            .unwrap();

        assert_eq!(store.load().unwrap().glass_mode, GlassMode::Liquid);
        assert_eq!(file_names(directory.path()).len(), 2);
    }

    #[test]
    fn save_replaces_an_existing_file_and_leaves_no_temporary_files() {
        let directory = TestDir::new("prefs-replace");
        let store = store_in(&directory);

        store
            .save(&Preferences {
                glass_mode: GlassMode::Frosted,
            })
            .unwrap();
        store
            .save(&Preferences {
                glass_mode: GlassMode::Liquid,
            })
            .unwrap();

        assert_eq!(store.load().unwrap().glass_mode, GlassMode::Liquid);
        assert_eq!(file_names(directory.path()), vec!["preferences.json"]);
    }

    #[test]
    fn save_creates_missing_parent_directories() {
        let directory = TestDir::new("prefs-nested");
        let store = PreferencesStore::with_path(
            directory
                .path()
                .join("nested")
                .join("config")
                .join("preferences.json"),
        );

        store
            .save(&Preferences {
                glass_mode: GlassMode::Frosted,
            })
            .unwrap();
        assert_eq!(store.load().unwrap().glass_mode, GlassMode::Frosted);
    }

    #[test]
    fn failed_replacement_removes_the_temporary_file_and_keeps_the_destination() {
        let directory = TestDir::new("prefs-failed-write");
        let store = store_in(&directory);
        let path = directory.path().join("preferences.json");
        // A directory squatting on the destination makes the final rename fail
        // after the temporary file has been fully written.
        fs::create_dir(&path).unwrap();

        let error = store.save(&Preferences::default()).unwrap_err();
        assert!(matches!(error, AppError::PreferencesWrite(_)));
        assert!(path.is_dir());
        assert!(
            file_names(directory.path())
                .iter()
                .all(|name| !name.contains(".tmp-"))
        );
    }

    #[test]
    fn unreadable_path_is_a_read_error_not_a_silent_default() {
        let directory = TestDir::new("prefs-read-error");
        let store = store_in(&directory);
        fs::create_dir(directory.path().join("preferences.json")).unwrap();

        assert!(matches!(store.load(), Err(AppError::PreferencesRead(_))));
    }
}
