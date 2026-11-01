use std::sync::{Arc, Mutex};

use tokio::{
    runtime::{Builder, Handle, Runtime},
    task::AbortHandle,
};

#[derive(Clone, Default)]
pub struct TaskCancellation(Arc<Mutex<Option<AbortHandle>>>);

impl TaskCancellation {
    pub fn replace(&self, task: AbortHandle) {
        let mut active = self.0.lock().unwrap_or_else(|poisoned| {
            log::error!("Task cancellation lock was poisoned; recovering its contents.");
            poisoned.into_inner()
        });
        if let Some(previous) = active.replace(task) {
            previous.abort();
        }
    }

    pub fn clear(&self) {
        *self.0.lock().unwrap_or_else(|poisoned| {
            log::error!("Task cancellation lock was poisoned; recovering its contents.");
            poisoned.into_inner()
        }) = None;
    }

    pub fn cancel(&self) {
        if let Some(task) = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| {
                log::error!("Task cancellation lock was poisoned; recovering its contents.");
                poisoned.into_inner()
            })
            .take()
        {
            task.abort();
        }
    }
}

pub struct AsyncRuntime {
    runtime: Option<Runtime>,
}

impl AsyncRuntime {
    pub fn new() -> std::io::Result<Self> {
        let runtime = Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .thread_name("onecenter-async")
            .build()?;
        Ok(Self {
            runtime: Some(runtime),
        })
    }

    pub fn handle(&self) -> Handle {
        self.runtime
            .as_ref()
            .expect("runtime is active until shutdown")
            .handle()
            .clone()
    }

    pub fn shutdown(mut self) {
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_timeout(std::time::Duration::from_secs(2));
        }
    }
}
