use std::collections::BTreeMap;
use std::env;
use std::ffi::{OsStr, OsString};
use std::sync::{Mutex, MutexGuard, OnceLock};

fn env_lock() -> &'static Mutex<()> {
    static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    ENV_LOCK.get_or_init(|| Mutex::new(()))
}

pub(crate) struct TestEnv {
    _guard: MutexGuard<'static, ()>,
    saved: BTreeMap<String, Option<OsString>>,
}

impl TestEnv {
    pub(crate) fn new() -> Self {
        Self {
            _guard: env_lock().lock().expect("test env lock poisoned"),
            saved: BTreeMap::new(),
        }
    }

    pub(crate) fn set_var(&mut self, key: &str, value: impl AsRef<OsStr>) {
        self.snapshot(key);
        unsafe {
            env::set_var(key, value);
        }
    }

    fn snapshot(&mut self, key: &str) {
        self.saved
            .entry(key.to_string())
            .or_insert_with(|| env::var_os(key));
    }
}

impl Drop for TestEnv {
    fn drop(&mut self) {
        for (key, value) in &self.saved {
            unsafe {
                if let Some(value) = value {
                    env::set_var(key, value);
                } else {
                    env::remove_var(key);
                }
            }
        }
    }
}
