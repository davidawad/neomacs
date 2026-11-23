//! Turns in a Parallel World (feature `shared-heap`): the waits, wakes and
//! deadlock verdict are Green's, but a thread that gives the Baton up puts
//! it back in the World's [`BatonLock`] instead of sending it to a chosen
//! owner, and a woken thread attaches when the Baton is free.

use super::{Baton, Control, ControlState, Handoff, OwnerLocation, ThreadKey, Wait, Wake};
use crate::emacs_core::eval::world::BatonLock;
use std::sync::PoisonError;
use std::time::{Duration, Instant};

/// How often a detached `Select` wait polls the process selector. Green's
/// idle scheduler blocks in the selector instead; here several owners wait
/// at once, each detached on its own OS thread.
const SELECT_POLL: Duration = Duration::from_millis(5);
/// Let the Main owner service timers and host events while blocked on
/// another parallel Lisp thread.
const OWNER_SERVICE_POLL: Duration = Duration::from_millis(10);

impl Control {
    /// Whether a thread other than ME can run now or at a deadline (Green's
    /// `pick_next` finding an owner).
    fn parallel_peer_can_run(state: &ControlState, me: ThreadKey) -> bool {
        Self::service_ready(state)
            || Self::runnable_excluding(state, me) > 0
            || Self::selects_excluding(state, me) > 0
    }

    /// ME, holding BATON, waits for WAIT: detach, wait detached until WAIT
    /// is over, and attach again. The waiting thread is at a safe point: a
    /// collection's stop does not wait for it, and it attaches again only
    /// outside a stop (`BatonLock::detach_parked`, `attach_unparked`). A `Runnable` wait is a yield: a runnable
    /// peer attaches first (GNU's FIFO order is not kept: the lock's wake
    /// order decides among several). Like Green's `transfer`, a
    /// blocking wait no other thread could ever end returns the Baton as
    /// `NoRunnable` without detaching, and once main is ending a blocking
    /// wait returns it as a wake.
    pub(crate) fn transfer_parallel(
        &self,
        lock: &BatonLock,
        me: ThreadKey,
        wait: Wait,
        baton: Baton,
        service_host_events: bool,
    ) -> Result<Wake, Handoff> {
        if matches!(wait, Wait::HostRequest(_)) {
            let invariant = Self::verify_invariants(&self.state());
            return Err(Handoff::NoRunnable { baton, invariant });
        }
        let peer_runnable = {
            let mut state = self.state();
            Self::note_continuation_release(&mut state, me);
            Self::set_wait(&mut state, me, wait);
            match wait {
                Wait::Runnable => Self::enqueue_ready(&mut state, me),
                // Main is ending: `close` woke every thread waiting then.
                // This one ran detached meanwhile and waits only now, so it
                // is woken at once too and ends as it resumes; detached, it
                // would wait for a wake that never comes, and main's join
                // of its OS thread with it.
                _ if state.closing => {
                    Self::set_wait(&mut state, me, Wait::Runnable);
                    state.physical.insert(me, OwnerLocation::Running);
                    return Ok(Wake::Resume(baton));
                }
                Wait::Select { .. } => {}
                _ if !Self::parallel_peer_can_run(&state, me) => {
                    let invariant = Self::verify_invariants(&state);
                    Self::set_wait(&mut state, me, Wait::Runnable);
                    state.physical.insert(me, OwnerLocation::Running);
                    return Err(Handoff::NoRunnable { baton, invariant });
                }
                _ => {}
            }
            state.physical.insert(me, OwnerLocation::Receiving);
            Self::ready_valid_excluding(&state, me) > 0
        };
        #[cfg(any(test, feature = "test-support"))]
        if let Wait::Select { wait_end } = wait {
            self.observe_select_park_for_test(me, wait_end);
        }
        // The waits this owner ended while it ran are published; owners
        // waiting detached re-check theirs, then queue on the lock.
        self.metadata_changed.notify_all();
        let baton = if wait == Wait::Runnable {
            // The collector's test of detached execution runs its section
            // here, between this park's detach and attach.
            #[cfg(test)]
            if let Some(section) =
                crate::emacs_core::eval::world::parallel_gc::detached::take_pending_section()
            {
                // It runs, detached: a yielding peer must not wait for its
                // attach.
                self.parallel_resumed(me);
                let baton = section.run(lock, baton);
                self.parallel_resumed(me);
                return Ok(Wake::Resume(baton));
            }
            lock.yield_parked(baton, peer_runnable)
        } else {
            lock.detach_parked(baton);
            self.await_parallel_wake(me, service_host_events);
            lock.attach_unparked()
        };
        self.parallel_resumed(me);
        Ok(Wake::Resume(baton))
    }

    /// Block, detached, until ME's wait is over: another thread woke it, or
    /// its `Select` deadline passed or its selector reported activity.
    fn await_parallel_wake(&self, me: ThreadKey, service_host_events: bool) {
        let select = self.select.get().and_then(Option::as_ref).copied();
        let mut state = self.state();
        loop {
            let now = Instant::now();
            Self::expire_deadlines(&mut state, now);
            let waiting = state.waits.get(&me).copied();
            let timeout = match waiting {
                Some(Wait::Runnable) => return,
                Some(Wait::Select { wait_end }) => {
                    if let Some(select) = select
                        && crate::emacs_core::eval::process_hooks::select_wait_until(select, now)
                    {
                        Self::wake_selects_at(&mut state, now);
                        self.metadata_changed.notify_all();
                        continue;
                    }
                    let left = wait_end.saturating_duration_since(now);
                    Some(if select.is_some() {
                        left.min(SELECT_POLL)
                    } else {
                        left
                    })
                }
                Some(Wait::Join(_) | Wait::Mutex(_) | Wait::Condition(_))
                    if me == ThreadKey::Main && service_host_events => Some(OWNER_SERVICE_POLL),
                Some(Wait::Join(_) | Wait::Mutex(_) | Wait::Condition(_)) => None,
                // Only this owner ends or removes its own record, and host
                // waits never detach.
                Some(Wait::HostRequest(_) | Wait::Exited) | None => std::process::abort(),
            };
            state = match timeout {
                Some(timeout) => {
                    let (next, result) = self.metadata_changed
                        .wait_timeout(state, timeout)
                        .unwrap_or_else(PoisonError::into_inner);
                    state = next;
                    if result.timed_out()
                        && me == ThreadKey::Main
                        && service_host_events
                        && matches!(waiting, Some(Wait::Join(_) | Wait::Mutex(_) | Wait::Condition(_)))
                        && state.waits.get(&me).copied() == waiting
                    {
                        state.owner_service_wakes.insert(me);
                        Self::wake_locked(&mut state, me);
                        self.metadata_changed.notify_all();
                    }
                    state
                }
                None => self
                    .metadata_changed
                    .wait(state)
                    .unwrap_or_else(PoisonError::into_inner),
            };
        }
    }

    /// ME attached: it runs, and its ready turn is used.
    pub(crate) fn parallel_resumed(&self, me: ThreadKey) {
        let mut state = self.state();
        state.ready.discard(me);
        state.physical.insert(me, OwnerLocation::Running);
        Self::check_invariants(&state);
    }

    /// ME ended, holding BATON: publish its exit as Green's `finish` does,
    /// close the runtime when nothing else can run, and put the Baton back.
    /// The calling OS thread touches no World state afterwards.
    pub(crate) fn finish_parallel(
        &self,
        lock: &BatonLock,
        completion: crate::emacs_core::threads::RetiredCompletion,
        baton: Baton,
    ) {
        let thread = completion.into_thread_id();
        if !baton.held_here() || thread.heap_identity() != baton.parallel_heap_identity() {
            std::process::abort();
        }
        let me = ThreadKey::from_id(thread.native_key());
        self.publish_owner_exit(me);
        {
            let mut state = self.state();
            Self::remove_key(&mut state, me);
            let alive = Self::service_ready(&state)
                || state.waits.runnable_count() > 0
                || state.waits.select_len() > 0;
            if !alive {
                // Green's `finish` with no owner to pick: every waiting
                // thread is woken, to signal and end.
                state.closing = true;
                let blocked: Vec<ThreadKey> = state
                    .waits
                    .iter()
                    .filter(|(_, wait)| !matches!(wait, Wait::Exited))
                    .map(|(key, _)| *key)
                    .collect();
                for key in blocked {
                    Self::wake_locked(&mut state, key);
                }
            }
        }
        lock.thread_finished();
        self.metadata_changed.notify_all();
        // The ended thread holds no root: no stop waits for it.
        lock.detach_parked(baton);
    }
}
