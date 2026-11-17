//! A Parallel World's Lisp thread (feature `shared-heap`): one dedicated OS
//! thread for the Lisp thread's whole life, never pooled. It attaches to
//! the World's Baton lock, builds its owner, runs its function detached
//! (attaching only for `World`-class work, `world::parallel_classify`) and
//! records the outcome as a Green carrier job does, then detaches for good.

use super::super::super::world::{Baton, BatonLock, detached_gate, prepare_detached_table};
use super::super::carrier_tls;
use super::{
    AbortOnPanic, Context, Control, Flow, Lifecycle, ThreadKey, Value, Wake, signal,
    stow_carried_state, stow_owner_state, unstow_carried_state, unstow_owner_state,
};
use std::sync::{Arc, mpsc};

/// The stack of a Lisp thread's OS thread, as the worker owners have.
const STACK_SIZE: usize = 64 * 1024 * 1024;

/// Test hook: the next Parallel thread body panics as it starts.
#[cfg(test)]
pub(crate) static PANIC_NEXT_BODY: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Start KEY's OS thread. It waits for the Baton, which the caller holds.
pub(super) fn spawn(
    key: ThreadKey,
    control: Arc<Control>,
    receiver: mpsc::Receiver<Wake>,
    lock: Arc<BatonLock>,
) -> std::io::Result<()> {
    let for_thread = Arc::clone(&lock);
    // Counted before the thread can end, which needs the caller's Baton.
    lock.thread_started();
    let spawned = std::thread::Builder::new()
        .name(format!("neovm-lisp-{}", key.id()))
        .stack_size(STACK_SIZE)
        .spawn(move || run(key, &control, receiver, &for_thread));
    match spawned {
        Ok(handle) => {
            lock.adopt_os_thread(handle);
            Ok(())
        }
        Err(error) => {
            lock.thread_finished();
            Err(error)
        }
    }
}

/// Run FUNCTION on OWNER, a panic becoming an error the thread ends with:
/// the Baton then goes back to the lock on the ordinary path.
fn run_body(owner: &mut Context, function: Value) -> Result<Value, Flow> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        #[cfg(test)]
        if PANIC_NEXT_BODY.swap(false, std::sync::atomic::Ordering::Relaxed) {
            panic!("test hook: a Parallel thread body panics");
        }
        owner.apply(function, Vec::new())
    }))
    .unwrap_or_else(|_| Err(signal("error", vec![Value::string("Lisp thread panicked")])))
}

/// Run FUNCTION on OWNER detached: the prologue ran attached; the Baton
/// goes back to LOCK for the body and is seated again for the epilogue.
fn run_detached_body(
    owner: &mut Context,
    function: Value,
    lock: &BatonLock,
) -> Result<Value, Flow> {
    prepare_detached_table();
    // Counted before the Baton goes back, so no collection starts in between.
    lock.body_detached();
    lock.detach(owner.parallel_unseat_baton());
    neovm_value::detached::begin_detached(detached_gate(lock));
    let outcome = run_body(owner, function);
    // A body that unwound while attached for a primitive still holds the
    // Baton; every other one attaches for the epilogue.
    let detached = neovm_value::detached::is_detached();
    neovm_value::detached::end_detached();
    if detached {
        owner.parallel_seat_baton(lock.attach().into_baton());
    }
    lock.body_attached();
    outcome
}

impl Context {
    /// Seat BATON, which this Parallel thread has just attached for: the
    /// World's carried and warm state arrive with it, as on a resume.
    pub(crate) fn parallel_seat_baton(&mut self, baton: Baton) {
        unstow_carried_state(&baton);
        unstow_owner_state(self, &baton);
        let seat = self.green_seat().expect("a Parallel thread is seated");
        debug_assert!(seat.baton.is_none(), "the World has one Baton");
        seat.baton = Some(baton);
        let key = seat.key;
        // The thread manager's current thread is the Baton holder's, as
        // after a resume: `mutex-lock` and `current-thread` read it.
        self.threads.enter_thread(key.id());
    }

    /// Take the seated Baton, to put it back in the lock: the World's warm
    /// and carried state leave with it, as on a park.
    pub(crate) fn parallel_unseat_baton(&mut self) -> Baton {
        let Some((key, baton)) = self
            .green_seat()
            .and_then(|seat| Some((seat.key, seat.baton.take()?)))
        else {
            // Detaching without the Baton would lose it for every thread.
            std::process::abort();
        };
        // As at a park: the record keeps the buffer other threads may read.
        let current = self.edit.buffers.current_buffer_id();
        self.threads.set_thread_current_buffer(key.id(), current);
        stow_owner_state(self, &baton);
        stow_carried_state(&baton);
        baton
    }
}

/// KEY's whole life on its OS thread (Green's `run_carrier_job`, with the
/// lock in place of the carrier's channel and pool).
fn run(key: ThreadKey, control: &Arc<Control>, receiver: mpsc::Receiver<Wake>, lock: &BatonLock) {
    // Outside the Lisp body, a panic would lose the Baton with this
    // thread's owner and every other thread would wait for it forever.
    let _abort_on_panic = AbortOnPanic;
    let mut baton = lock.attach().into_baton();
    control.parallel_resumed(key);
    #[cfg(feature = "jit")]
    crate::emacs_core::jit::green_owner::begin_owner_turn();
    unstow_carried_state(&baton);
    let mut owner = Context::new_green_owner(&baton);
    unstow_owner_state(&mut owner, &baton);
    let completion_id = match owner.threads.get_thread(key.id()) {
        Some(thread) => thread.id(),
        None => std::process::abort(),
    };
    if control.lifecycle() == Lifecycle::Closing {
        // Main ended before this thread ever ran.
        let completion = match owner.threads.prepare_thread_completion(
            completion_id,
            crate::emacs_core::threads::CompletionExit::ClosingBeforeStart,
        ) {
            Ok(completion) => completion,
            Err(error) => error.fail_stop(),
        };
        if let Some(runtime) = baton.green_runtime().as_ref() {
            runtime.pending_starts.borrow_mut().remove(&key);
        }
        stow_owner_state(&mut owner, &baton);
        owner.retire_green_owner();
        stow_carried_state(&baton);
        carrier_tls::park_carrier_tls();
        let retired = baton.retire_thread_completion(completion);
        control.finish_parallel(lock, retired, baton);
        return;
    }
    let outgoing_error = baton
        .green_runtime()
        .as_ref()
        .and_then(|runtime| runtime.outgoing_constant_error.take())
        .filter(|(previous, _)| *previous != key)
        .map(|(_, symbol)| symbol);
    // Enter the thread before its buffer: a buffer switch records itself as
    // the current thread's buffer.
    owner.threads.enter_thread(key.id());
    if let Some(buffer) = owner.threads.thread_current_buffer(key.id()) {
        owner.restore_current_buffer_if_live(buffer);
    }
    owner.threads.start_thread(key.id());
    // GNU raises an error signalled into a thread that has not run yet at
    // its first switch, once it has handlers.
    let deferred = owner.threads.take_thread_error(key.id());
    let function = baton.green_runtime().as_ref().and_then(|runtime| {
        if let Some(error) = deferred {
            runtime.deferred_errors.borrow_mut().insert(key, error);
        }
        runtime.pending_starts.borrow_mut().remove(&key)
    });
    if let Some(runtime) = baton.green_runtime().as_ref() {
        runtime
            .parallel_quits
            .borrow_mut()
            .insert(key, owner.quit_requested.clone());
    }
    owner.take_green_seat(key, Arc::clone(control), receiver, baton);
    let outcome = match (outgoing_error, function) {
        (Some(symbol), _) => Err(signal(
            neovm_value::error::LispCondition::SettingConstant,
            vec![Value::from_sym_id(symbol)],
        )),
        (None, Some(function)) => {
            owner.push_vm_root_frame();
            owner.push_vm_frame_root(function);
            let outcome = run_detached_body(&mut owner, function, lock);
            owner.pop_vm_root_frame();
            outcome
        }
        (None, None) => Ok(Value::NIL),
    };
    let exit = outcome.as_ref().err().and_then(Flow::shutdown_request);
    // A kill-emacs is the process's, not this thread's: it is not recorded.
    let _ = crate::emacs_core::threads::finish_make_thread_result(
        &mut owner.threads,
        key.id(),
        outcome,
    );
    let completion = match owner.threads.prepare_thread_completion(
        completion_id,
        crate::emacs_core::threads::CompletionExit::Executed,
    ) {
        Ok(completion) => completion,
        Err(error) => error.fail_stop(),
    };
    let seat = owner.green_seat().expect("seated above");
    let mut baton = seat
        .baton
        .take()
        .expect("a finishing thread holds the Baton");
    owner.restore_deferred_error(&baton, key);
    if let Some(runtime) = baton.green_runtime().as_ref() {
        runtime.parallel_quits.borrow_mut().remove(&key);
        // GNU clears prev_thread when a thread exits: no outgoing unbind.
        runtime.outgoing_constant_error.set(None);
    }
    if let Some(request) = exit
        && control.lifecycle() == Lifecycle::Running
        && let Some(runtime) = baton.green_runtime().as_ref()
    {
        // GNU's `kill-emacs` exits the process from any thread: main runs
        // next, only to leave.
        runtime.exit.set(Some(request));
        control.prioritize(ThreadKey::Main);
    }
    stow_owner_state(&mut owner, &baton);
    owner.retire_green_owner();
    stow_carried_state(&baton);
    // This OS thread ends after detaching: its thread-local destructors must
    // then name no heap or World object.
    carrier_tls::park_carrier_tls();
    let retired = baton.retire_thread_completion(completion);
    control.finish_parallel(lock, retired, baton);
}
