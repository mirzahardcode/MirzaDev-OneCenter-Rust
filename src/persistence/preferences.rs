use std::{fs, path::PathBuf};

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

    pub fn load(&self) -> AppResult<Preferences> {
        match fs::read(&self.path) {
            Ok(data) => Ok(serde_json::from_slice(&data)?),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Ok(Preferences::default())
            }
            Err(error) => Err(AppError::PreferencesRead(error)),
        }
    }

    pub fn save(&self, preferences: &Preferences) -> AppResult<()> {
        let parent = self
            .path
            .parent()
            .ok_or(AppError::DataDirectoryUnavailable)?;
        fs::create_dir_all(parent).map_err(AppError::PreferencesWrite)?;
        let data = serde_json::to_vec_pretty(preferences)?;
        fs::write(&self.path, data).map_err(AppError::PreferencesWrite)
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf};

    use super::{GlassMode, Preferences, PreferencesStore};

    fn test_store() -> (PreferencesStore, PathBuf) {
        let directory = std::env::temp_dir().join(format!(
            "mirzadev-onecenter-preferences-{}",
            std::process::id()
        ));
        let path = directory.join("preferences.json");
        (PreferencesStore { path: path.clone() }, directory)
    }

    #[test]
    fn missing_preferences_use_defaults_and_saved_mode_round_trips() {
        let (store, directory) = test_store();
        assert_eq!(store.load().unwrap().glass_mode, GlassMode::Off);

        store
            .save(&Preferences {
                glass_mode: GlassMode::Frosted,
            })
            .unwrap();
        assert_eq!(store.load().unwrap().glass_mode, GlassMode::Frosted);

        fs::remove_dir_all(directory).unwrap();
    }
}
