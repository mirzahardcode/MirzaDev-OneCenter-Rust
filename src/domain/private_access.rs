#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExpiryStatus {
    Valid,
    Expired,
    MissingExpiry,
    Inactive,
    DeviceMismatch,
    UserNotFound,
}

pub fn evaluate_expiry(
    user_exists: bool,
    active: Option<bool>,
    registered_device_id: Option<&str>,
    current_device_id: &str,
    expires_at_ms: Option<i64>,
    current_time_ms: i64,
) -> ExpiryStatus {
    if !user_exists {
        return ExpiryStatus::UserNotFound;
    }
    if active == Some(false) {
        return ExpiryStatus::Inactive;
    }
    if registered_device_id
        .filter(|id| !id.trim().is_empty())
        .is_some_and(|id| id != current_device_id)
    {
        return ExpiryStatus::DeviceMismatch;
    }
    let Some(expires_at) = expires_at_ms else {
        return ExpiryStatus::MissingExpiry;
    };

    let normalized_expiry = if expires_at < 100_000_000_000 {
        expires_at.saturating_mul(1000)
    } else {
        expires_at
    };

    if current_time_ms >= normalized_expiry {
        ExpiryStatus::Expired
    } else {
        ExpiryStatus::Valid
    }
}

impl ExpiryStatus {
    pub const fn error_message(self) -> Option<&'static str> {
        match self {
            Self::Valid => None,
            Self::Expired => Some("Akses Private telah berakhir. Silakan perbarui akses Anda."),
            Self::MissingExpiry => {
                Some("Akses Private telah berakhir atau belum diatur. Silakan perbarui akses Anda.")
            }
            Self::Inactive => Some("Akses akun telah dinonaktifkan."),
            Self::DeviceMismatch => Some("Device tidak diizinkan untuk akun ini."),
            Self::UserNotFound => Some("Akun belum terdaftar di sistem."),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ExpiryStatus, evaluate_expiry};

    const NOW_MS: i64 = 1_000_000_000_000;

    #[test]
    fn enforces_user_active_device_and_expiry_rules() {
        assert_eq!(
            evaluate_expiry(
                false,
                Some(true),
                Some("device"),
                "device",
                Some(NOW_MS + 1),
                NOW_MS
            ),
            ExpiryStatus::UserNotFound
        );
        assert_eq!(
            evaluate_expiry(
                true,
                Some(false),
                Some("device"),
                "device",
                Some(NOW_MS + 1),
                NOW_MS
            ),
            ExpiryStatus::Inactive
        );
        assert_eq!(
            evaluate_expiry(
                true,
                None,
                Some("other"),
                "device",
                Some(NOW_MS + 1),
                NOW_MS
            ),
            ExpiryStatus::DeviceMismatch
        );
        assert_eq!(
            evaluate_expiry(true, None, None, "device", None, NOW_MS),
            ExpiryStatus::MissingExpiry
        );
        assert_eq!(
            evaluate_expiry(true, None, None, "device", Some(NOW_MS), NOW_MS),
            ExpiryStatus::Expired
        );
        assert_eq!(
            evaluate_expiry(
                true,
                None,
                Some("device"),
                "device",
                Some(NOW_MS + 1),
                NOW_MS
            ),
            ExpiryStatus::Valid
        );
    }

    #[test]
    fn normalizes_seconds_and_preserves_millisecond_timestamps() {
        assert_eq!(
            evaluate_expiry(
                true,
                None,
                None,
                "device",
                Some(1_790_592_000),
                1_740_000_000_000
            ),
            ExpiryStatus::Valid
        );
        assert_eq!(
            evaluate_expiry(true, None, None, "device", Some(i64::MAX), 1),
            ExpiryStatus::Valid
        );
    }
}
