//! Generic persistent JSON cache with a time-to-live policy.
//!
//! The cache stores one serializable value per file together with the wall
//! clock time at which it was saved. [`PersistentJsonCache::load`] reports
//! whether the file is missing, a fresh or stale hit, or corrupted. Stale
//! values are returned on purpose so callers can use them as an offline
//! fallback. Corrupted files are never reported as fresh: they are moved aside
//! (quarantined) and reported separately.
//!
//! The cache is intentionally independent from the GitHub controller.

use std::{
    fmt, fs, io,
    marker::PhantomData,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use thiserror::Error;

use crate::persistence::preferences::{quarantine_corrupt_file, write_file_replacing};

#[derive(Clone, Copy, Debug)]
pub struct CachePolicy {
    pub ttl: std::time::Duration,
}

impl CachePolicy {
    pub const STANDARD: Self = Self {
        ttl: std::time::Duration::from_secs(30 * 60),
    };
}

/// Failures that are not explained by the cache simply being absent or
/// corrupted (those are regular [`CacheLookup`] outcomes).
#[derive(Debug, Error)]
pub enum CacheError {
    #[error("cache file {path:?} could not be read: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("cache file {path:?} could not be written: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("cache file {path:?} could not be removed: {source}")]
    Remove {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("cache value could not be serialized: {0}")]
    Serialize(#[source] serde_json::Error),
}

/// A value read from the cache together with its freshness.
#[derive(Clone, Debug, PartialEq)]
pub struct CachedValue<T> {
    pub value: T,
    /// Time elapsed since the value was saved. Zero when the stored timestamp
    /// lies in the future (see [`CachedValue::is_fresh`]).
    pub age: Duration,
    /// `true` only when the entry is younger than the policy TTL. An entry
    /// whose timestamp is in the future (the system clock moved backwards)
    /// is never fresh because its real age cannot be known.
    pub is_fresh: bool,
}

/// Details about a cache file that could not be decoded.
#[derive(Clone, Debug)]
pub struct CorruptedCache {
    /// Human readable decoding error.
    pub reason: String,
    /// Where the unreadable file was preserved, if it could be moved aside.
    pub quarantined_to: Option<PathBuf>,
}

/// Outcome of [`PersistentJsonCache::load`].
#[derive(Debug)]
pub enum CacheLookup<T> {
    /// No cache file exists.
    Missing,
    /// A readable entry; check [`CachedValue::is_fresh`] to tell fresh from
    /// stale. Stale entries remain usable as an offline fallback.
    Hit(CachedValue<T>),
    /// The file existed but could not be decoded. It has already been moved
    /// out of the way, so the next `load` reports `Missing`.
    Corrupted(CorruptedCache),
}

#[derive(Serialize)]
struct EnvelopeRef<'a, T> {
    saved_at_ms: u64,
    data: &'a T,
}

#[derive(Deserialize)]
struct Envelope<T> {
    saved_at_ms: u64,
    data: T,
}

/// File-backed JSON cache for one value of type `T`.
///
/// The handle only stores a path and a policy, so it is cheap to clone and is
/// `Send + Sync` regardless of `T`. All methods perform blocking file I/O;
/// call them from `spawn_blocking` (or another non-UI thread) in async code.
pub struct PersistentJsonCache<T> {
    path: PathBuf,
    policy: CachePolicy,
    _value: PhantomData<fn() -> T>,
}

impl<T> Clone for PersistentJsonCache<T> {
    fn clone(&self) -> Self {
        Self {
            path: self.path.clone(),
            policy: self.policy,
            _value: PhantomData,
        }
    }
}

impl<T> fmt::Debug for PersistentJsonCache<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PersistentJsonCache")
            .field("path", &self.path)
            .field("policy", &self.policy)
            .finish()
    }
}

impl<T> PersistentJsonCache<T> {
    pub fn new(path: impl Into<PathBuf>, policy: CachePolicy) -> Self {
        Self {
            path: path.into(),
            policy,
            _value: PhantomData,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn policy(&self) -> CachePolicy {
        self.policy
    }

    /// Removes the cache file. A missing file is not an error.
    pub fn clear(&self) -> Result<(), CacheError> {
        match fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(CacheError::Remove {
                path: self.path.clone(),
                source,
            }),
        }
    }

    fn quarantine(&self, reason: String) -> CorruptedCache {
        log::warn!(
            "Cache file {} is corrupted ({reason}); it will be ignored.",
            self.path.display()
        );
        let quarantined_to = match quarantine_corrupt_file(&self.path) {
            Ok(destination) => {
                log::warn!(
                    "Corrupted cache file was preserved at {}.",
                    destination.display()
                );
                Some(destination)
            }
            Err(error) => {
                log::error!(
                    "Could not set aside corrupted cache file {}: {error}",
                    self.path.display()
                );
                None
            }
        };
        CorruptedCache {
            reason,
            quarantined_to,
        }
    }
}

impl<T: Serialize> PersistentJsonCache<T> {
    /// Stores `value` stamped with the current time.
    ///
    /// The value is serialized by reference (no clone) into one buffer before
    /// the file system is touched, so a serialization failure leaves the
    /// existing cache untouched. The write goes through a temporary file that
    /// is flushed and then renamed over the cache file.
    pub fn save(&self, value: &T) -> Result<(), CacheError> {
        self.save_at(value, SystemTime::now())
    }

    fn save_at(&self, value: &T, now: SystemTime) -> Result<(), CacheError> {
        let envelope = EnvelopeRef {
            saved_at_ms: millis_since_epoch(now),
            data: value,
        };
        let bytes = serde_json::to_vec(&envelope).map_err(CacheError::Serialize)?;
        write_file_replacing(&self.path, &bytes).map_err(|source| CacheError::Write {
            path: self.path.clone(),
            source,
        })
    }
}

impl<T: DeserializeOwned> PersistentJsonCache<T> {
    /// Reads the cache and classifies it as missing, hit (fresh or stale) or
    /// corrupted. A corrupted file is quarantined as a side effect.
    pub fn load(&self) -> Result<CacheLookup<T>, CacheError> {
        self.load_at(SystemTime::now())
    }

    fn load_at(&self, now: SystemTime) -> Result<CacheLookup<T>, CacheError> {
        let bytes = match fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(CacheLookup::Missing);
            }
            Err(source) => {
                return Err(CacheError::Read {
                    path: self.path.clone(),
                    source,
                });
            }
        };

        let envelope = match serde_json::from_slice::<Envelope<T>>(&bytes) {
            Ok(envelope) => envelope,
            Err(error) => {
                return Ok(CacheLookup::Corrupted(self.quarantine(error.to_string())));
            }
        };

        let now_ms = millis_since_epoch(now);
        let (age, is_fresh) = if envelope.saved_at_ms > now_ms {
            log::warn!(
                "Cache file {} is stamped in the future; treating it as stale.",
                self.path.display()
            );
            (Duration::ZERO, false)
        } else {
            let age = Duration::from_millis(now_ms - envelope.saved_at_ms);
            (age, age < self.policy.ttl)
        };

        Ok(CacheLookup::Hit(CachedValue {
            value: envelope.data,
            age,
            is_fresh,
        }))
    }
}

/// Milliseconds since the Unix epoch; a clock before the epoch maps to zero.
fn millis_since_epoch(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH).map_or(0, |elapsed| {
        u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
    })
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        time::{Duration, SystemTime, UNIX_EPOCH},
    };

    use serde::{Deserialize, Serialize};

    use super::{CacheError, CacheLookup, CachePolicy, PersistentJsonCache, millis_since_epoch};
    use crate::persistence::preferences::test_support::{TestDir, file_names};

    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    struct Sample {
        name: String,
        count: u32,
    }

    fn samples() -> Vec<Sample> {
        vec![
            Sample {
                name: "alpha".to_owned(),
                count: 1,
            },
            Sample {
                name: "beta".to_owned(),
                count: 2,
            },
        ]
    }

    fn cache_in(directory: &TestDir, file_name: &str) -> PersistentJsonCache<Vec<Sample>> {
        PersistentJsonCache::new(directory.path().join(file_name), CachePolicy::STANDARD)
    }

    fn base_time() -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_800_000_000)
    }

    fn expect_hit(lookup: CacheLookup<Vec<Sample>>) -> super::CachedValue<Vec<Sample>> {
        match lookup {
            CacheLookup::Hit(hit) => hit,
            other => panic!("expected a cache hit, got {other:?}"),
        }
    }

    #[test]
    fn standard_policy_is_thirty_minutes() {
        assert_eq!(CachePolicy::STANDARD.ttl, Duration::from_secs(30 * 60));
    }

    #[test]
    fn cache_miss_when_no_file_exists() {
        let directory = TestDir::new("cache-miss");
        let cache = cache_in(&directory, "apks.json");

        assert!(matches!(cache.load().unwrap(), CacheLookup::Missing));
        assert!(file_names(directory.path()).is_empty());
    }

    #[test]
    fn fresh_hit_after_save_and_reload() {
        let directory = TestDir::new("cache-fresh");
        let cache = cache_in(&directory, "apks.json");
        cache.save_at(&samples(), base_time()).unwrap();

        let hit = expect_hit(
            cache
                .load_at(base_time() + Duration::from_secs(60))
                .unwrap(),
        );
        assert_eq!(hit.value, samples());
        assert_eq!(hit.age, Duration::from_secs(60));
        assert!(hit.is_fresh);
    }

    #[test]
    fn save_and_load_with_the_real_clock() {
        let directory = TestDir::new("cache-real-clock");
        let cache = cache_in(&directory, "apks.json");
        cache.save(&samples()).unwrap();

        let hit = expect_hit(cache.load().unwrap());
        assert_eq!(hit.value, samples());
        assert!(hit.is_fresh);

        // A second handle on the same path sees the same data.
        let other_handle = cache.clone();
        assert_eq!(expect_hit(other_handle.load().unwrap()).value, samples());
    }

    #[test]
    fn stale_entries_are_still_returned_but_not_fresh() {
        let directory = TestDir::new("cache-stale");
        let cache = cache_in(&directory, "apks.json");
        cache.save_at(&samples(), base_time()).unwrap();

        let later = base_time() + CachePolicy::STANDARD.ttl + Duration::from_secs(1);
        let hit = expect_hit(cache.load_at(later).unwrap());
        assert_eq!(hit.value, samples());
        assert!(!hit.is_fresh);
        assert_eq!(hit.age, CachePolicy::STANDARD.ttl + Duration::from_secs(1));
    }

    #[test]
    fn ttl_boundary_is_exclusive() {
        let directory = TestDir::new("cache-boundary");
        let policy = CachePolicy {
            ttl: Duration::from_secs(10),
        };
        let cache =
            PersistentJsonCache::<Vec<Sample>>::new(directory.path().join("apks.json"), policy);
        cache.save_at(&samples(), base_time()).unwrap();

        let just_inside = base_time() + Duration::from_millis(9_999);
        assert!(expect_hit(cache.load_at(just_inside).unwrap()).is_fresh);

        let exactly_ttl = base_time() + Duration::from_secs(10);
        assert!(!expect_hit(cache.load_at(exactly_ttl).unwrap()).is_fresh);
    }

    #[test]
    fn corrupted_json_is_quarantined_and_never_fresh() {
        let directory = TestDir::new("cache-corrupt");
        let cache = cache_in(&directory, "apks.json");
        let path = directory.path().join("apks.json");
        fs::write(&path, b"{ definitely not json").unwrap();

        match cache.load().unwrap() {
            CacheLookup::Corrupted(report) => {
                let quarantined = report.quarantined_to.expect("file should be set aside");
                assert!(quarantined.exists());
                assert_eq!(fs::read(&quarantined).unwrap(), b"{ definitely not json");
                assert!(!report.reason.is_empty());
            }
            other => panic!("expected Corrupted, got {other:?}"),
        }
        assert!(!path.exists());

        let names = file_names(directory.path());
        assert_eq!(names.len(), 1);
        assert!(names[0].starts_with("apks.json.corrupt-"));

        // Once quarantined, the cache simply reads as missing.
        assert!(matches!(cache.load().unwrap(), CacheLookup::Missing));
    }

    #[test]
    fn valid_json_with_the_wrong_shape_is_corrupted() {
        let directory = TestDir::new("cache-wrong-shape");
        let cache = cache_in(&directory, "apks.json");
        fs::write(
            directory.path().join("apks.json"),
            br#"{"saved_at_ms": 1, "data": "not a list"}"#,
        )
        .unwrap();

        assert!(matches!(cache.load().unwrap(), CacheLookup::Corrupted(_)));
    }

    #[test]
    fn future_timestamp_is_treated_as_stale_not_fresh() {
        let directory = TestDir::new("cache-future");
        let cache = cache_in(&directory, "apks.json");
        // Saved "one hour from now": the clock must have moved backwards.
        cache
            .save_at(&samples(), base_time() + Duration::from_secs(3_600))
            .unwrap();

        let hit = expect_hit(cache.load_at(base_time()).unwrap());
        assert_eq!(hit.value, samples());
        assert!(!hit.is_fresh);
        assert_eq!(hit.age, Duration::ZERO);
    }

    #[test]
    fn clock_before_the_epoch_maps_to_zero_and_never_panics() {
        assert_eq!(millis_since_epoch(UNIX_EPOCH - Duration::from_secs(1)), 0);
        assert_eq!(millis_since_epoch(UNIX_EPOCH), 0);
        assert_eq!(
            millis_since_epoch(UNIX_EPOCH + Duration::from_millis(1_234)),
            1_234
        );

        let directory = TestDir::new("cache-pre-epoch");
        let cache = cache_in(&directory, "apks.json");
        cache.save_at(&samples(), base_time()).unwrap();
        let hit = expect_hit(cache.load_at(UNIX_EPOCH - Duration::from_secs(1)).unwrap());
        assert!(!hit.is_fresh);
    }

    #[test]
    fn clear_removes_the_file_and_is_idempotent() {
        let directory = TestDir::new("cache-clear");
        let cache = cache_in(&directory, "apks.json");
        cache.save(&samples()).unwrap();
        assert_eq!(file_names(directory.path()), vec!["apks.json"]);

        cache.clear().unwrap();
        assert!(matches!(cache.load().unwrap(), CacheLookup::Missing));
        assert!(file_names(directory.path()).is_empty());

        cache
            .clear()
            .expect("clearing a missing cache is not an error");
    }

    #[test]
    fn independent_paths_do_not_interfere() {
        let directory = TestDir::new("cache-independent");
        let apks = cache_in(&directory, "apks.json");
        let other = PersistentJsonCache::<Vec<String>>::new(
            directory.path().join("other.json"),
            CachePolicy::STANDARD,
        );

        apks.save(&samples()).unwrap();
        other.save(&vec!["one".to_owned()]).unwrap();

        apks.clear().unwrap();
        assert!(matches!(apks.load().unwrap(), CacheLookup::Missing));
        match other.load().unwrap() {
            CacheLookup::Hit(hit) => assert_eq!(hit.value, vec!["one".to_owned()]),
            unexpected => panic!("expected a hit, got {unexpected:?}"),
        }
    }

    #[test]
    fn save_replaces_previous_contents_without_leaving_temporary_files() {
        let directory = TestDir::new("cache-replace");
        let cache = cache_in(&directory, "apks.json");

        cache.save(&samples()).unwrap();
        let replacement = vec![Sample {
            name: "gamma".to_owned(),
            count: 3,
        }];
        cache.save(&replacement).unwrap();

        assert_eq!(expect_hit(cache.load().unwrap()).value, replacement);
        assert_eq!(file_names(directory.path()), vec!["apks.json"]);
    }

    #[test]
    fn save_creates_missing_parent_directories() {
        let directory = TestDir::new("cache-nested");
        let cache = PersistentJsonCache::<Vec<Sample>>::new(
            directory.path().join("a").join("b").join("apks.json"),
            CachePolicy::STANDARD,
        );
        cache.save(&samples()).unwrap();
        assert_eq!(expect_hit(cache.load().unwrap()).value, samples());
    }

    #[test]
    fn failed_write_reports_an_error_and_cleans_up_temporary_files() {
        let directory = TestDir::new("cache-failed-write");
        let path = directory.path().join("apks.json");
        fs::create_dir(&path).unwrap();
        let cache = cache_in(&directory, "apks.json");

        let error = cache.save(&samples()).unwrap_err();
        assert!(matches!(error, CacheError::Write { .. }));
        assert!(path.is_dir());
        assert!(
            file_names(directory.path())
                .iter()
                .all(|name| !name.contains(".tmp-"))
        );
    }

    #[test]
    fn serialization_failure_does_not_touch_the_file_system() {
        struct Unserializable;

        impl Serialize for Unserializable {
            fn serialize<S: serde::Serializer>(&self, _serializer: S) -> Result<S::Ok, S::Error> {
                Err(<S::Error as serde::ser::Error>::custom(
                    "intentional serialization failure",
                ))
            }
        }

        let directory = TestDir::new("cache-serialize-error");
        let cache = PersistentJsonCache::<Unserializable>::new(
            directory.path().join("apks.json"),
            CachePolicy::STANDARD,
        );

        let error = cache.save(&Unserializable).unwrap_err();
        assert!(matches!(error, CacheError::Serialize(_)));
        assert!(file_names(directory.path()).is_empty());
    }

    #[test]
    fn unreadable_path_is_a_typed_read_error() {
        let directory = TestDir::new("cache-read-error");
        fs::create_dir(directory.path().join("apks.json")).unwrap();
        let cache = cache_in(&directory, "apks.json");

        assert!(matches!(cache.load(), Err(CacheError::Read { .. })));
    }
}
