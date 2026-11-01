//! Shared Tokio runtime and per-category task bookkeeping.
//!
//! [`AsyncRuntime`] owns the single multi-thread Tokio runtime of the
//! application. [`TaskRegistry`] tracks at most one active task per
//! [`TaskKind`] and hands out [`TaskTicket`]s so that a slow, superseded
//! request can never clear or overwrite the state of a newer request.

use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use tokio::{
    runtime::{Builder, Handle, Runtime},
    task::AbortHandle,
};

/// Independent categories of background work.
///
/// Each category owns one slot in the [`TaskRegistry`]; starting a new task of
/// the same kind replaces (and aborts) the previous one, while tasks of other
/// kinds are never affected.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TaskKind {
    HomeFetch,
    ApkFetch,
    PrivateAuthentication,
    PrivateSessionCheck,
    PrivateDocumentFetch,
}

/// Proof that a request was registered for a given [`TaskKind`].
///
/// Tickets are cheap `Copy` values. Every ticket has a process-unique,
/// monotonically increasing identifier, so a ticket that was replaced or
/// finished can never be mistaken for a newer one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TaskTicket {
    kind: TaskKind,
    id: u64,
}

impl TaskTicket {
    pub const fn kind(self) -> TaskKind {
        self.kind
    }

    /// Unique request identifier (never zero).
    pub const fn id(self) -> u64 {
        self.id
    }
}

#[derive(Debug)]
struct TaskSlot {
    id: u64,
    /// `None` while the ticket is reserved but the spawned task has not been
    /// attached yet.
    abort: Option<AbortHandle>,
    /// Only the first guard for a ticket may finish the registry entry.
    guard_claimed: bool,
}

type SlotMap = HashMap<TaskKind, TaskSlot>;

#[derive(Debug, Default)]
struct RegistryShared {
    next_id: AtomicU64,
    slots: Mutex<SlotMap>,
}

/// Cheaply clonable registry of active background tasks.
///
/// Typical flow:
///
/// 1. `try_begin` (or `begin`) reserves a ticket *before* spawning.
/// 2. `guard` wraps the ticket in an RAII [`TaskGuard`] that is moved into the
///    task (and later into the UI completion closure).
/// 3. `attach` stores the `AbortHandle` of the spawned task.
/// 4. The completion code asks `TaskGuard::is_current` before touching shared
///    state, and dropping the guard finishes the ticket.
///
/// The internal mutex is never held while aborting a task or running any
/// foreign code, and a poisoned mutex is recovered and logged.
#[derive(Clone, Debug, Default)]
pub struct TaskRegistry {
    shared: Arc<RegistryShared>,
}

impl TaskRegistry {
    /// Reserves a new ticket for `kind`.
    ///
    /// Any task previously registered for the same kind is replaced and its
    /// `AbortHandle` (if already attached) is aborted.
    pub fn begin(&self, kind: TaskKind) -> TaskTicket {
        let id = self.next_id();
        let previous = self.lock_slots().insert(
            kind,
            TaskSlot {
                id,
                abort: None,
                guard_claimed: false,
            },
        );
        if let Some(handle) = previous.and_then(|slot| slot.abort) {
            handle.abort();
        }
        TaskTicket { kind, id }
    }

    /// Reserves a ticket only if no task of `kind` is currently registered.
    ///
    /// The check and the reservation happen under one lock, so concurrent
    /// callers cannot both succeed. Returns `None` for a duplicate request.
    pub fn try_begin(&self, kind: TaskKind) -> Option<TaskTicket> {
        let id = self.next_id();
        let mut slots = self.lock_slots();
        if slots.contains_key(&kind) {
            return None;
        }
        slots.insert(
            kind,
            TaskSlot {
                id,
                abort: None,
                guard_claimed: false,
            },
        );
        Some(TaskTicket { kind, id })
    }

    /// Stores the abort handle of the task spawned for `ticket`.
    ///
    /// If the ticket is no longer current, or already has an attached task,
    /// the new task is aborted immediately and `false` is returned. The first
    /// attached handle remains registered.
    pub fn attach(&self, ticket: TaskTicket, handle: AbortHandle) -> bool {
        let rejected = {
            let mut slots = self.lock_slots();
            match slots.get_mut(&ticket.kind) {
                Some(slot) if slot.id == ticket.id => {
                    if slot.abort.is_some() {
                        Some(handle)
                    } else {
                        slot.abort = Some(handle);
                        None
                    }
                }
                _ => Some(handle),
            }
        };

        match rejected {
            Some(handle) => {
                handle.abort();
                false
            }
            None => true,
        }
    }

    /// Returns `true` while `ticket` is the registered request of its kind.
    pub fn is_current(&self, ticket: TaskTicket) -> bool {
        self.lock_slots()
            .get(&ticket.kind)
            .is_some_and(|slot| slot.id == ticket.id)
    }

    /// Returns `true` if any task (reserved or attached) exists for `kind`.
    pub fn is_active(&self, kind: TaskKind) -> bool {
        self.lock_slots().contains_key(&kind)
    }

    /// Releases `ticket`. Only removes the registry entry if the ticket is
    /// still current, so a stale request can never remove a newer one.
    ///
    /// Returns `true` if the entry was removed. The task is not aborted.
    pub fn finish(&self, ticket: TaskTicket) -> bool {
        let mut slots = self.lock_slots();
        let is_current = slots
            .get(&ticket.kind)
            .is_some_and(|slot| slot.id == ticket.id);
        if is_current {
            slots.remove(&ticket.kind);
        }
        is_current
    }

    /// Aborts and forgets the task registered for `kind`, if any.
    ///
    /// This only touches the registry. State owned by the caller (for example
    /// a loading indicator) must be reset by the caller.
    pub fn cancel(&self, kind: TaskKind) -> bool {
        let slot = self.lock_slots().remove(&kind);
        match slot {
            Some(slot) => {
                if let Some(handle) = slot.abort {
                    handle.abort();
                }
                true
            }
            None => false,
        }
    }

    /// Aborts and forgets every registered task. Returns how many entries were
    /// cancelled. The Tokio runtime itself is untouched.
    pub fn cancel_all(&self) -> usize {
        let drained = std::mem::take(&mut *self.lock_slots());
        let count = drained.len();
        for (_, slot) in drained {
            if let Some(handle) = slot.abort {
                handle.abort();
            }
        }
        count
    }

    /// Creates an RAII guard that finishes `ticket` when dropped.
    ///
    /// Only the first guard created for the current ticket owns it. Further
    /// guards are inert, so dropping a duplicate cannot finish the owner's
    /// active registry entry.
    pub fn guard(&self, ticket: TaskTicket) -> TaskGuard {
        let owns_ticket = {
            let mut slots = self.lock_slots();
            match slots.get_mut(&ticket.kind) {
                Some(slot) if slot.id == ticket.id && !slot.guard_claimed => {
                    slot.guard_claimed = true;
                    true
                }
                _ => false,
            }
        };
        TaskGuard {
            registry: self.clone(),
            ticket,
            owns_ticket,
        }
    }

    fn next_id(&self) -> u64 {
        // Relaxed is sufficient: only uniqueness matters, not ordering with
        // other memory operations. Identifiers start at 1.
        self.shared.next_id.fetch_add(1, Ordering::Relaxed) + 1
    }

    fn lock_slots(&self) -> MutexGuard<'_, SlotMap> {
        self.shared.slots.lock().unwrap_or_else(|poisoned| {
            log::error!("Task registry lock was poisoned; recovering its contents.");
            self.shared.slots.clear_poison();
            poisoned.into_inner()
        })
    }
}

/// Finishes its ticket when dropped.
///
/// The guard is moved into the spawned future and then into the UI completion
/// closure. Wherever it ends up being dropped (normal completion, abort,
/// panic, failed event-loop delivery, runtime shutdown) the registry entry is
/// released, but only if the ticket is still the current one.
#[derive(Debug)]
#[must_use = "dropping an owning TaskGuard immediately finishes its ticket"]
pub struct TaskGuard {
    registry: TaskRegistry,
    ticket: TaskTicket,
    owns_ticket: bool,
}

impl TaskGuard {
    pub fn ticket(&self) -> TaskTicket {
        self.ticket
    }

    /// `true` only for the owning guard while its ticket is current.
    pub fn is_current(&self) -> bool {
        self.owns_ticket && self.registry.is_current(self.ticket)
    }
}

impl Drop for TaskGuard {
    fn drop(&mut self) {
        if self.owns_ticket {
            self.registry.finish(self.ticket);
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
            runtime.shutdown_timeout(Duration::from_secs(2));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::{
        runtime::{Builder, Runtime},
        task::JoinHandle,
    };

    use super::{AsyncRuntime, TaskKind, TaskRegistry};

    fn test_runtime() -> Runtime {
        Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime")
    }

    fn pending_task(runtime: &Runtime) -> JoinHandle<()> {
        runtime.spawn(std::future::pending::<()>())
    }

    fn settle(runtime: &Runtime) {
        runtime.block_on(async {
            tokio::time::sleep(Duration::from_millis(25)).await;
        });
    }

    fn assert_cancelled(runtime: &Runtime, task: JoinHandle<()>) {
        let error = runtime
            .block_on(task)
            .expect_err("task should have been aborted");
        assert!(error.is_cancelled());
    }

    #[test]
    fn tickets_are_unique_nonzero_and_increasing() {
        let registry = TaskRegistry::default();
        let a = registry.begin(TaskKind::ApkFetch);
        let b = registry.begin(TaskKind::HomeFetch);
        let c = registry.begin(TaskKind::ApkFetch);
        assert!(a.id() > 0);
        assert!(a.id() < b.id());
        assert!(b.id() < c.id());
        assert_eq!(a.kind(), TaskKind::ApkFetch);
        assert_eq!(b.kind(), TaskKind::HomeFetch);
    }

    #[test]
    fn replacement_makes_the_older_ticket_stale() {
        let registry = TaskRegistry::default();
        let a = registry.begin(TaskKind::ApkFetch);
        let b = registry.begin(TaskKind::ApkFetch);
        assert!(!registry.is_current(a));
        assert!(registry.is_current(b));
    }

    #[test]
    fn finishing_a_stale_ticket_never_removes_the_newer_entry() {
        let registry = TaskRegistry::default();
        let a = registry.begin(TaskKind::ApkFetch);
        let b = registry.begin(TaskKind::ApkFetch);

        assert!(!registry.finish(a));
        assert!(registry.is_current(b));
        assert!(registry.is_active(TaskKind::ApkFetch));

        assert!(registry.finish(b));
        assert!(!registry.is_active(TaskKind::ApkFetch));
        assert!(!registry.finish(b));
    }

    #[test]
    fn try_begin_rejects_duplicates_until_the_ticket_is_finished() {
        let registry = TaskRegistry::default();
        let first = registry
            .try_begin(TaskKind::ApkFetch)
            .expect("first request");
        assert!(registry.try_begin(TaskKind::ApkFetch).is_none());
        assert!(registry.try_begin(TaskKind::HomeFetch).is_some());

        assert!(registry.finish(first));
        assert!(registry.try_begin(TaskKind::ApkFetch).is_some());
    }

    #[test]
    fn try_begin_is_exclusive_across_threads() {
        let registry = TaskRegistry::default();
        let results: Vec<bool> = std::thread::scope(|scope| {
            let mut handles = Vec::new();
            for _ in 0..8 {
                handles.push(scope.spawn(|| registry.try_begin(TaskKind::ApkFetch).is_some()));
            }
            handles
                .into_iter()
                .map(|handle| handle.join().expect("worker thread"))
                .collect()
        });
        assert_eq!(results.iter().filter(|won| **won).count(), 1);
    }

    #[test]
    fn replacing_a_ticket_aborts_the_previous_attached_task() {
        let runtime = test_runtime();
        let registry = TaskRegistry::default();

        let first = registry.begin(TaskKind::ApkFetch);
        let first_task = pending_task(&runtime);
        assert!(registry.attach(first, first_task.abort_handle()));

        let second = registry.begin(TaskKind::ApkFetch);
        assert_cancelled(&runtime, first_task);
        assert!(registry.is_current(second));
        assert!(!registry.is_current(first));
    }

    #[test]
    fn attaching_to_a_replaced_ticket_aborts_the_task_immediately() {
        let runtime = test_runtime();
        let registry = TaskRegistry::default();

        let first = registry.begin(TaskKind::ApkFetch);
        let second = registry.begin(TaskKind::ApkFetch);

        let late_task = pending_task(&runtime);
        assert!(!registry.attach(first, late_task.abort_handle()));
        assert_cancelled(&runtime, late_task);
        assert!(registry.is_current(second));
    }

    #[test]
    fn attaching_to_a_finished_ticket_is_rejected() {
        let runtime = test_runtime();
        let registry = TaskRegistry::default();

        let ticket = registry.begin(TaskKind::HomeFetch);
        assert!(registry.finish(ticket));

        let task = pending_task(&runtime);
        assert!(!registry.attach(ticket, task.abort_handle()));
        assert_cancelled(&runtime, task);
    }

    #[test]
    fn duplicate_attachment_rejects_and_aborts_only_the_new_task() {
        let runtime = test_runtime();
        let registry = TaskRegistry::default();
        let ticket = registry.begin(TaskKind::ApkFetch);

        let first_task = pending_task(&runtime);
        assert!(registry.attach(ticket, first_task.abort_handle()));

        let second_task = pending_task(&runtime);
        assert!(!registry.attach(ticket, second_task.abort_handle()));
        assert_cancelled(&runtime, second_task);
        assert!(registry.is_current(ticket));

        assert!(registry.cancel(TaskKind::ApkFetch));
        assert_cancelled(&runtime, first_task);
        assert!(!registry.is_active(TaskKind::ApkFetch));
    }

    #[test]
    fn finish_does_not_abort_the_running_task() {
        let runtime = test_runtime();
        let registry = TaskRegistry::default();

        let ticket = registry.begin(TaskKind::ApkFetch);
        let task = pending_task(&runtime);
        assert!(registry.attach(ticket, task.abort_handle()));
        assert!(registry.finish(ticket));

        settle(&runtime);
        assert!(!task.is_finished());
        task.abort();
        assert_cancelled(&runtime, task);
    }

    #[test]
    fn cancel_only_affects_the_requested_kind() {
        let runtime = test_runtime();
        let registry = TaskRegistry::default();

        let apk = registry.begin(TaskKind::ApkFetch);
        let apk_task = pending_task(&runtime);
        assert!(registry.attach(apk, apk_task.abort_handle()));

        let home = registry.begin(TaskKind::HomeFetch);
        let home_task = pending_task(&runtime);
        assert!(registry.attach(home, home_task.abort_handle()));

        assert!(registry.cancel(TaskKind::ApkFetch));
        assert!(!registry.cancel(TaskKind::ApkFetch));
        assert_cancelled(&runtime, apk_task);

        assert!(registry.is_current(home));
        settle(&runtime);
        assert!(!home_task.is_finished());

        assert_eq!(registry.cancel_all(), 1);
        assert_cancelled(&runtime, home_task);
    }

    #[test]
    fn cancel_all_aborts_every_task_and_invalidates_every_ticket() {
        let runtime = test_runtime();
        let registry = TaskRegistry::default();

        let kinds = [
            TaskKind::HomeFetch,
            TaskKind::ApkFetch,
            TaskKind::PrivateDocumentFetch,
        ];
        let mut tickets = Vec::new();
        let mut tasks = Vec::new();
        for kind in kinds {
            let ticket = registry.begin(kind);
            let task = pending_task(&runtime);
            assert!(registry.attach(ticket, task.abort_handle()));
            tickets.push(ticket);
            tasks.push(task);
        }

        assert_eq!(registry.cancel_all(), 3);
        for task in tasks {
            assert_cancelled(&runtime, task);
        }
        for ticket in tickets {
            assert!(!registry.is_current(ticket));
            assert!(!registry.is_active(ticket.kind()));
        }
        assert_eq!(registry.cancel_all(), 0);
    }

    #[test]
    fn guard_finishes_only_its_own_ticket() {
        let registry = TaskRegistry::default();
        let first = registry.begin(TaskKind::ApkFetch);
        let first_guard = registry.guard(first);
        assert!(first_guard.is_current());

        let second = registry.begin(TaskKind::ApkFetch);
        assert!(!first_guard.is_current());
        drop(first_guard);
        assert!(registry.is_current(second));

        let second_guard = registry.guard(second);
        assert_eq!(second_guard.ticket(), second);
        assert!(second_guard.is_current());
        drop(second_guard);
        assert!(!registry.is_active(TaskKind::ApkFetch));
    }

    #[test]
    fn duplicate_guard_is_inert_and_cannot_finish_the_owner_ticket() {
        let runtime = test_runtime();
        let registry = TaskRegistry::default();
        let ticket = registry.try_begin(TaskKind::ApkFetch).expect("ticket");

        let owner_guard = registry.guard(ticket);
        let duplicate_guard = registry.guard(ticket);
        assert!(owner_guard.is_current());
        assert!(!duplicate_guard.is_current());

        let owner_task = runtime.spawn(async move {
            let _guard = owner_guard;
            std::future::pending::<()>().await;
        });
        assert!(registry.attach(ticket, owner_task.abort_handle()));

        let duplicate_task = runtime.spawn(async move {
            let _guard = duplicate_guard;
            std::future::pending::<()>().await;
        });
        assert!(!registry.attach(ticket, duplicate_task.abort_handle()));
        assert_cancelled(&runtime, duplicate_task);
        assert!(registry.is_current(ticket));
        assert!(registry.is_active(TaskKind::ApkFetch));

        assert!(registry.cancel(TaskKind::ApkFetch));
        assert_cancelled(&runtime, owner_task);
        assert!(!registry.is_active(TaskKind::ApkFetch));
    }

    #[test]
    fn guard_releases_the_entry_when_its_task_is_aborted() {
        let runtime = test_runtime();
        let registry = TaskRegistry::default();

        let ticket = registry.try_begin(TaskKind::ApkFetch).expect("ticket");
        let guard = registry.guard(ticket);
        let task = runtime.spawn(async move {
            let _guard = guard;
            std::future::pending::<()>().await;
        });
        assert!(registry.attach(ticket, task.abort_handle()));
        settle(&runtime);
        assert!(registry.is_active(TaskKind::ApkFetch));

        task.abort();
        assert_cancelled(&runtime, task);
        assert!(!registry.is_active(TaskKind::ApkFetch));
    }

    #[test]
    fn poisoned_lock_is_recovered_and_cleared() {
        let registry = TaskRegistry::default();
        let poisoner = registry.clone();
        let outcome = std::thread::spawn(move || {
            let _slots = poisoner.shared.slots.lock().expect("lock before poisoning");
            panic!("intentional panic to poison the registry lock");
        })
        .join();
        assert!(outcome.is_err());
        assert!(registry.shared.slots.is_poisoned());

        let ticket = registry.begin(TaskKind::ApkFetch);
        assert!(registry.is_current(ticket));
        assert!(!registry.shared.slots.is_poisoned());
    }

    #[test]
    fn async_runtime_provides_a_working_handle_and_shuts_down() {
        let runtime = AsyncRuntime::new().expect("runtime");
        let handle = runtime.handle();
        let task = handle.spawn(async { 40 + 2 });
        assert_eq!(handle.block_on(task).expect("task result"), 42);
        runtime.shutdown();
    }
}
