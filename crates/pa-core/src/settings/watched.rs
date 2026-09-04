use super::{FileSettingsStorage, SettingsManager, SettingsScope, SettingsStorage};
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

/// A long-lived settings manager that reloads after external document changes.
pub struct WatchedSettingsManager {
    manager: Arc<Mutex<SettingsManager>>,
    cancellation: Arc<(Mutex<bool>, Condvar)>,
    worker: Option<JoinHandle<()>>,
}

impl WatchedSettingsManager {
    pub fn create(cwd: impl AsRef<Path>, agent_dir: impl AsRef<Path>) -> Self {
        let storage: Arc<dyn SettingsStorage> =
            Arc::new(FileSettingsStorage::new(cwd.as_ref(), agent_dir.as_ref()));
        let manager = Arc::new(Mutex::new(SettingsManager::from_storage(storage.clone())));
        let weak = Arc::downgrade(&manager);
        let cancellation = Arc::new((Mutex::new(false), Condvar::new()));
        let signal = cancellation.clone();
        let snapshot = || {
            [SettingsScope::Global, SettingsScope::Project]
                .map(|scope| storage.read(scope).map_err(|error| error.to_string()))
        };
        let mut previous = snapshot();
        let worker = std::thread::spawn(move || loop {
            let (lock, ready) = &*signal;
            let stopped = lock
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let (stopped, _) = ready
                .wait_timeout_while(stopped, Duration::from_millis(100), |stopped| !*stopped)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if *stopped {
                break;
            }
            drop(stopped);
            let current = [SettingsScope::Global, SettingsScope::Project]
                .map(|scope| storage.read(scope).map_err(|error| error.to_string()));
            if current == previous {
                continue;
            }
            previous = current;
            let Some(manager) = weak.upgrade() else {
                break;
            };
            let _ = manager
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .reload();
        });
        Self {
            manager,
            cancellation,
            worker: Some(worker),
        }
    }

    pub fn with<T>(&self, read: impl FnOnce(&SettingsManager) -> T) -> T {
        read(
            &self
                .manager
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }
    pub fn with_mut<T>(&self, update: impl FnOnce(&mut SettingsManager) -> T) -> T {
        update(
            &mut self
                .manager
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }
    pub fn dispose(&mut self) {
        let (lock, ready) = &*self.cancellation;
        *lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
        ready.notify_all();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
impl Drop for WatchedSettingsManager {
    fn drop(&mut self) {
        self.dispose();
    }
}
