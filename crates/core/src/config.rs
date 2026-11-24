//! The engine's settings. One explicit value per thing (the quality
//! standard): the host passes every one; `Config::recommended` fills in the
//! values the plan sets, each named and explained here, so nothing is a
//! hidden default buried in code.

use crate::names::{VolumeRules, clean_device_name};

/// Every setting of one sync root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    /// The device's name as given at sign-in; it goes into conflict copies'
    /// names, cleaned and cut to 30 characters (3.5.1).
    pub device_name: String,
    /// What the root's volume does with names (probed by the fs crate).
    pub volume: VolumeRules,
    /// The ignore patterns (3.14), gitignore syntax.
    pub ignore: Vec<String>,
    /// The mass-change guard pauses a plan that deletes or overwrites more
    /// than this many files on one side of a mount (3.11)...
    pub mass_change_count: u64,
    /// ...and more than this many per thousand of the mount's items (both
    /// must be exceeded; 250 is the plan's quarter).
    pub mass_change_per_mille: u64,
    /// Days the holding area keeps what sync removed (3.7).
    pub held_days: u32,
    /// Bytes kept free on the volume besides a download (3.10).
    pub disk_reserve_bytes: u64,
    /// Files up to this size go in one request; larger ones in a session
    /// (3.6; also capped by the server's `bodyMax`).
    pub small_file_max: u64,
    /// The chunk size to start from (3.6), raised to keep within the
    /// server's chunk count and lowered to its `chunkMax`.
    pub chunk_size: u64,
    /// The size of one download range (3.6).
    pub range_size: u64,
    /// A new local file at least this big is first looked for on Hearth by
    /// its hash, to copy instead of upload (3.4, sync-api.md 12).
    pub copy_check_min: u64,
    /// The first retry delay after a failure, in milliseconds (3.10).
    pub backoff_base_ms: u64,
    /// The longest retry delay, in milliseconds (3.10: five minutes).
    pub backoff_cap_ms: u64,
    /// The safety-net full scan's interval, in seconds (3.3: six hours).
    pub rescan_interval_s: u64,
    /// How often items listed as "wait for a change" are tried anyway, in
    /// seconds (3.10's slow timer).
    pub problem_retry_s: u64,
    /// How often a file that keeps changing while hashed is tried before it
    /// is listed as still being written (3.3).
    pub hash_attempts: u32,
    /// How many operations may be planned and in flight at once: bounds
    /// memory on a first sync of a million files (3.12).
    pub max_ops_in_flight: usize,
    /// How long one `wait` request lasts, in seconds (capped by the
    /// server's `waitMax`).
    pub wait_s: u32,
}

/// A setting the engine refuses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfigError {
    /// The device name has nothing left after cleaning.
    DeviceName,
    /// A size or count is zero where it cannot be.
    Zero(&'static str),
}

impl Config {
    /// The values the plan sets, for a device name and a probed volume.
    pub fn recommended(device_name: &str, volume: VolumeRules) -> Config {
        Config {
            device_name: device_name.to_owned(),
            volume,
            ignore: crate::ignore::DEFAULT_RULES
                .iter()
                .map(|s| (*s).to_owned())
                .collect(),
            mass_change_count: 50,
            mass_change_per_mille: 250,
            held_days: 7,
            disk_reserve_bytes: 1 << 30,
            small_file_max: 8 << 20,
            chunk_size: 8 << 20,
            range_size: 8 << 20,
            copy_check_min: 1 << 20,
            backoff_base_ms: 1000,
            backoff_cap_ms: 300_000,
            rescan_interval_s: 6 * 3600,
            problem_retry_s: 3600,
            hash_attempts: 5,
            max_ops_in_flight: 1000,
            wait_s: 55,
        }
    }

    /// Checks the settings; the engine will not start on bad ones.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if clean_device_name(&self.device_name).is_none() {
            return Err(ConfigError::DeviceName);
        }
        for (v, name) in [
            (self.small_file_max, "small_file_max"),
            (self.chunk_size, "chunk_size"),
            (self.range_size, "range_size"),
            (self.backoff_base_ms, "backoff_base_ms"),
            (self.backoff_cap_ms, "backoff_cap_ms"),
            (self.max_ops_in_flight as u64, "max_ops_in_flight"),
            (u64::from(self.hash_attempts), "hash_attempts"),
        ] {
            if v == 0 {
                return Err(ConfigError::Zero(name));
            }
        }
        Ok(())
    }
}
