#[derive(Clone, Copy, Debug)]
pub struct CachePolicy {
    pub ttl: std::time::Duration,
}

impl CachePolicy {
    pub const STANDARD: Self = Self {
        ttl: std::time::Duration::from_secs(30 * 60),
    };
}
