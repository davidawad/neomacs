# Shared-heap parallel Elisp: experiment report

Status: Experimental, opt-in, off by default
Audience: NeoMacs VM and runtime contributors evaluating real parallelism for GNU's thread primitives
Scope: What the `shared-heap` experiment builds, what it measures, and what it does not do

## Status

This is an experiment, not a change to the decision record of
[`neovm-rfc-modular-multithread.md`](neovm-rfc-modular-multithread.md). That
record stands: the default threading model is isolate-first, the default
compatibility mode does not run shared-heap multithreaded Elisp, and any
shared-heap model is experimental and opt-in. This experiment is that opt-in
model, and nothing in it is reachable unless both switches are set:

- the Cargo feature `shared-heap`, off by default, and
- the environment variable `NEOVM_THREADS=parallel` at startup.

The experiment was planned as seven slices. Two exist as delivered patch
series; the rest are designed and not implemented:

| Slice | Content | State on this branch |
|---|---|---|
| S2 | Parallel mode, the Baton lock, `neomacs-shared-heap-stats` | Integrated |
| S1 | `neomacs-perf` thread scenarios (`threads-numeric-loop`, `threads-json-parse`, `threads-stress`) | Delivered on another base; does not apply here (not integrated) |
| S3 | Detached execution of Scalar/Sealed-class code | Not delivered |
| S4 | Stop-the-world collection rendezvous at safepoints | Not delivered |
| S5 | Per-owner allocation regions and pages | Not delivered |
| S6 | ThreadSanitizer runner and stress suite | Not delivered |
| S7 | This document | — |

With only S2 integrated, a Parallel World gives every Lisp thread its own OS
thread, but **one thread runs Lisp at a time**, exactly as in Green mode. The
experiment's parallel speedup lives entirely in the slices that were not
delivered. Everything below says, for each part of the design, which state
it is in.

With the feature off the build is the default build: the only rows that
exist in every build are the registration-plan and effect-catalog rows for
the new subr, which the default build skips as an inactive plan row.

## Building and running

Build the editor with the feature:

```bash
cargo build -p neomacs --features shared-heap
```

The feature runs down the chain `neomacs` -> `neovm-core` -> `neovm-eval` ->
`neovm-value`, the same chain as `vm-profile`. A crate-level check of the
evaluator alone is enough to see the parallel runtime compile:

```bash
cargo check -p neovm-eval --features shared-heap
```

Select the mode at startup:

```bash
NEOVM_THREADS=parallel target/release/neomacs -Q
```

The editor needs the generated Lisp tree; build it with
`cargo xtask fresh-build --release --features shared-heap`.

`NEOVM_THREADS` takes `sim` (the default, also when unset), `green`, and,
with the feature, `parallel`
(`crates/neovm-eval/src/emacs_core/runtime/eval/admission.rs`,
`ThreadingMode::from_env`). In a build without the feature, `parallel` is an
unknown value: it warns once and selects `sim`.

After host startup a feature build provides `neomacs-shared-heap`, so
`(featurep 'neomacs-shared-heap)` is non-nil whatever `NEOVM_THREADS` says.

## Threads, owners and the Baton lock

A Green World already runs each Lisp thread as an owner: a `Context` of its
own over the shared World, with owner-local dynamic bindings, on its own OS
thread. Its safety argument (`eval/world/green_owner.rs`) is the Baton: one
`Send`, non-`Sync`, non-`Clone` token; only its holder runs Lisp or touches
a World view; handing it on is a channel send to the chosen next owner.

A Parallel World keeps that argument and changes only the Baton's transport
(`eval/world/parallel_owner.rs`). While no owner runs, the Baton rests in the
World's `BatonLock` (a mutex and two condition variables). An owner takes it
with `attach` and puts it back with `detach`; the mutex's release/acquire
pair is the cross-thread ordering edge that Green gets from the channel.

- `make-thread` starts one dedicated OS thread per Lisp thread for the
  thread's whole life (`eval/green/switch/parallel_thread.rs`). The new
  thread attaches, then builds its own owner with
  `Context::new_green_owner`, because that constructor installs thread-local
  state on the calling OS thread.
- An owner holds the Baton whenever it runs Lisp. It detaches only inside
  `Control::transfer_parallel` (`eval/green/scheduler/parallel_turn.rs`):
  `thread-join`, `mutex-lock` on a held mutex, `condition-wait`,
  `thread-yield`, `sleep-for` and `accept-process-output` waits, and at the
  thread's end.
- Scheduling records are Green's (`Control`: waits, wakes, ready turns, the
  deadlock verdict), and parking uses Green's park/restore path, so the
  thread primitives return what they return in Green mode.
- `thread-yield` waits for a peer's attach when a peer is waiting on the lock
  or runnable. Among several, the lock's wake order decides; GNU's FIFO
  order is not kept.
- A detached `sleep-for`/`accept-process-output` wait polls the process
  selector every 5 ms rather than blocking in it.
- A panic inside a thread's Lisp body ends the thread with
  `(error "Lisp thread panicked")` and returns the Baton on the ordinary
  path. A panic in runtime bookkeeping still aborts, as in Green.
- When main ends, every thread ends and main joins every dedicated OS thread.

The Baton lock is the only synchronization the experiment adds, and every
acquisition is counted.

## What runs in parallel and what attaches

The effect inventory classifies every registration
(`lisp/native/subr/effect_schema.rs`, `ParallelClass`):

- `World` — may touch any World state; the default for every registration
  without an evidence-bearing declaration;
- `Sealed(authority)` — touches one object under a named single-object
  authority (a cons field read or write, a fresh cons allocation, a sealed
  hash-table read);
- `Scalar` — reads and returns values only.

With S2 the canonical totals over 1,726 names are 1,651 World, 41 Sealed and
34 Scalar (`effect_parallel.rs`); the new subr is one World name.

**Implemented:** every class attaches. A running Lisp thread holds the Baton
for every primitive, whatever its class.

**Not implemented (S3):** detaching around Scalar- and Sealed-class code, the
override table that would refine the inventory's class for the parallel
executor (`parallel_classify.rs` in the design; no such file exists), and
per-reason counters for each attach. The stats plist below is the only
counter surface.

## Memory

The design is one heap and one collector shared by every owner:

- per-owner allocation regions and pages, so owners allocate without a lock;
- a stop-the-world rendezvous: a collection asks every detached owner to stop
  at its next safepoint, and counts the stops;
- the concurrent marker (`crates/neovm-core/src/tagged/CONCURRENT_GC.md`)
  excluded in a Parallel World: its handshakes and SATB barrier are written
  for one running mutator, and making them serve several is out of the
  experiment's scope.

**Implemented:** none of the three. In S2 only the Baton holder allocates or
collects, so a collection runs exactly as in Green: a parked owner's state is
published as roots through Green's park path, and owner binding slabs are
World-registered roots. The tests
`a_detached_threads_values_survive_another_threads_collection`,
`eight_threads_yield_and_collect_without_losing_values` and
`a_detached_threads_vm_frames_survive_other_threads_collections` check that a
collection run by one thread keeps a parked thread's values and VM frames.
`:safepoint-stops`, `:collections` and `:deferred-bytes` are always 0.

**Not implemented:** S4 (stop-the-world rendezvous) and S5 (per-owner
allocation).

## Dynamic binding

Parallel uses Green's owner-local bindings unchanged
(`eval/owner_bindings.rs`). A dynamic binding of a projected variable lives
in the binding owner's slab: one slot per projected variable and channel
(the default channel or one buffer's local channel). Reads and writes consult
the owner's slot first and the variable's canonical storage otherwise, so an
owner's binding is invisible to other owners and a write by an owner with no
binding is visible to all, as in GNU's threads. The World registers every
slab, and the collector traces them as roots. The JIT reads owner-local
bindings in a Parallel World too (`jit/compile/inline_vars.rs` asks
`uses_owner_threads()`).

## Buffers

Every buffer primitive is World-class and attaches. The stage-5 design, a
per-buffer owner that lets a thread edit its own buffer detached, with a
per-buffer mutex and its counter, is **not implemented**.

## The Lisp API

GNU's thread primitives keep their names and results: `make-thread`,
`thread-join`, `thread-yield`, `thread-signal`, `current-thread`,
`thread-name`, `thread-live-p`, `all-threads`, `make-mutex`, `mutex-lock`,
`mutex-unlock`, `make-condition-variable`, `condition-wait`,
`condition-notify`. The Parallel tests compare their results with neomacs's
Green mode, not with GNU Emacs 31.1; that comparison is UNATTESTED.

`(featurep 'neomacs-shared-heap)` is non-nil in a feature build after host
startup.

`(neomacs-shared-heap-stats)` takes no arguments and returns a plist. Outside
a Parallel World every counter is 0 except `:threads`.

| Key | Value |
|---|---|
| `:mode` | `sim`, `green` or `parallel`: the World's threading mode |
| `:threads` | live Lisp threads: the lock's count in a Parallel World, the thread manager's count otherwise |
| `:attach-count` | acquisitions of the Baton lock |
| `:attach-wait-ns` | nanoseconds spent blocked in `attach` waiting for the Baton |
| `:detached-ns` | nanoseconds the Baton rested in the lock with no holder |
| `:safepoint-stops` | owners stopped at a safepoint for a collection; 0 (S4 not implemented) |
| `:collections` | collections run with detached owners stopped; 0 (S4 not implemented) |
| `:deferred-bytes` | bytes allocated in owner regions awaiting the next collection; 0 (S5 not implemented) |

## Limitations

From the slices' verification records:

- **One thread runs at a time** (S2). Parallel mode serializes Lisp on the
  Baton lock exactly as Green does; there is no parallel speedup yet.
- **`thread-yield` order** (S2): the lock's wake order decides among several
  waiting peers; GNU's FIFO order is not kept.
- **Selector polling** (S2): a detached process wait polls every 5 ms.
- **Panics outside a thread body abort** (S2), as in Green.
- **No oracle run** (S2): thread primitive results are compared with Green
  mode, not GNU Emacs 31.1. UNATTESTED.
- **No subprocess test in a Parallel thread** (S2): `accept-process-output`
  takes the tested `sleep-for` wait path, but no subprocess test exists.
- **Editor not run with the feature** (S2): `crates/neomacs` with the
  feature was type-checked, not built or run; the GNU thread suite under
  `NEOVM_THREADS=parallel` has not been run.
- **`dbus-vendored` builds** (S2): one existing guard element,
  `(featurep 'dbusbind)`, is nil in a `dbus-vendored` build with and without
  the feature.
- **No Neomacs thread benchmark samples** (S1): the scenarios exist, and
  every Neomacs row of the baseline table is "not run".
- **S1 is not integrated**: its patches target a later worker-isolate base
  and do not apply on this branch.
- **No detached execution** (S3 not delivered).
- **No concurrent marking** in a Parallel World; collections run under the
  Baton.
- **No stop-the-world rendezvous or per-owner allocation** (S4, S5 not
  delivered).
- **No ThreadSanitizer run or stress runner** (S6 not delivered).
- **Interning attaches**: `intern` and the obarray are World state.
- **Display attaches**: redisplay and every frame and window primitive are
  World-class.
- **No channels**: the RFC's message-passing channels (§8.4.5) are not part
  of this experiment; threads communicate through shared Lisp data under
  GNU's mutexes and condition variables.

## Measurements

Every row below is copied from a slice's verification record, with the
processor count of the machine that produced it. A debug build is never a
performance measurement; it is reported for the counter values.

### S2: stats after a contended mutex run (Parallel, 4 processors, debug test build)

Two Lisp threads, 10000 `mutex-lock` rounds each, a `thread-yield` inside the
critical section every 1000 rounds, main joining both. One run, printed,
never asserted.

| counter | value |
|---|---:|
| `:mode` | `parallel` |
| `:threads` (live) | 1 |
| `:attach-count` | 24 |
| `:attach-wait-ns` | 4965254 |
| `:detached-ns` | 1356533 |
| `:safepoint-stops` | 0 |
| `:collections` | 0 |
| `:deferred-bytes` | 0 |

An uncontended `mutex-lock` never detaches, so 20000 rounds cost 24 lock
acquisitions: the thread starts, the yields, the contended locks they cause
and main's joins.

### S1: thread scenario baseline (4 processors)

| Scenario | `NEOVM_THREADS` | Samples | Parallel `elapsed_wall_us`, `instructions:u`, isolate values | nproc |
| --- | --- | ---: | --- | ---: |
| threads-numeric-loop | unset (`sim`) | 3 | Not run — release build: run by the orchestrator on a VM | 4 |
| threads-numeric-loop | `green` | 3 | Not run — release build: run by the orchestrator on a VM | 4 |
| threads-json-parse | unset (`sim`) | 3 | Not run — release build: run by the orchestrator on a VM | 4 |
| threads-json-parse | `green` | 3 | Not run — release build: run by the orchestrator on a VM | 4 |

No `parallel` column exists: S1's base has no Parallel mode. The fixture
smoke runs in S1 used a system GNU Emacs 30.1 as syntax and invariant
checks, not as a measurement.

### Worker isolates, for comparison

The worker-isolate executor is on the same branch with disjoint code. Its
rows come from the isolate series' own verification records.

Release build, 16 processors, `cargo xtask perf run`, `verdict = valid`:

| Scenario/configuration | Serial seconds | Warm parallel seconds | Serial/parallel ratio | Pool boot max/mean (µs) |
| --- | ---: | ---: | ---: | ---: |
| Loop, interpreted, 16 tasks | 16.367 | 1.656 | 9.88x | 245315 / 226911 |
| Loop, interpreted, 64 tasks | 63.474 | 5.286 | 12.01x | 245315 / 226911 |
| Loop, bytecode JIT on, 16 tasks | 0.158 | 0.025 | 6.40x | 245315 / 226911 |
| Loop, bytecode JIT on, 64 tasks | 0.602 | 0.090 | 6.66x | 245315 / 226911 |
| Loop, bytecode JIT off, 16 tasks | 1.697 | 0.211 | 8.03x | 232537 / 223295 |
| Loop, bytecode JIT off, 64 tasks | 5.969 | 0.638 | 9.35x | 232537 / 223295 |
| JSON, 16 tasks | 1.900 | 0.514 | 3.69x | 278861 / 265787 |

The isolate series' Lisp-library benchmark (`neomacs-worker-pmap` against a
serial main-thread loop, 16 pool owners), release build, 4 processors:

| Processors | Pool owners | Boot mean per owner (ms) | Boot max per owner (ms) | Workload | Tasks | Serial main thread (ms) | Parallel (ms) | Speedup |
| ---: | ---: | ---: | ---: | --- | ---: | ---: | ---: | ---: |
| 4 | 16 | 263.466 | 279.720 | interpreted numeric loop | 16 | 337.308 | 586.230 | 0.58x |
| 4 | 16 | 263.466 | 279.720 | interpreted numeric loop | 64 | 1345.285 | 2124.833 | 0.63x |
| 4 | 16 | 263.466 | 279.720 | byte-compiled numeric loop (per call) | 16 | 62.979 | 329.872 | 0.19x |
| 4 | 16 | 263.466 | 279.720 | byte-compiled numeric loop (per call) | 64 | 124.080 | 530.808 | 0.23x |
| 4 | 16 | 263.466 | 279.720 | json-parse-string (~200 KB) | 16 | 24.826 | 130.214 | 0.19x |
| 4 | 16 | 263.466 | 279.720 | json-parse-string (~200 KB) | 64 | 57.009 | 683.453 | 0.08x |

There is no shared-heap row to set beside these: with S2 alone the Parallel
column would equal Green, and no Green or Parallel release sample was taken.

## Running the tests

The Parallel tests are unit tests of the evaluator crate:

```bash
cargo nextest run -p neovm-eval --lib --features shared-heap --no-fail-fast \
  -E 'test(parallel_owner_tests::)'
```

Repeat them to shake out ordering bugs:

```bash
cargo nextest run -p neovm-eval --lib --features shared-heap --no-fail-fast \
  -E 'test(parallel_owner_tests::)' --stress-count 30
```

The default build must be unchanged; run the crate both ways:

```bash
cargo nextest run -p neovm-eval --lib --no-fail-fast
cargo nextest run -p neovm-eval --lib --features shared-heap --no-fail-fast
```

GNU's thread suite runs on a release editor built with the feature, once
`cargo xtask fresh-build --release --features shared-heap` has generated the
Lisp tree:

```bash
NEOVM_THREADS=parallel target/release/neomacs -Q --batch \
  -l ert -l test/src/thread-tests.el -f ert-run-tests-batch-and-exit
```

Run it with `NEOVM_THREADS=sim` and `NEOVM_THREADS=green` too, to compare the
three modes.

The thread benchmarks are `neomacs-perf` scenarios (S1, not integrated on
this branch); with S1 applied, `cargo xtask perf list` names
`threads-numeric-loop`, `threads-json-parse` and `threads-stress`, and
`cargo xtask perf run threads-stress --gc-stress` runs the stress scenario
with a small collection threshold.

The ThreadSanitizer runner (S6) was not delivered.

## What a real implementation still needs

- **A concurrent marker across mutators.** The concurrent collector's
  handshakes and SATB barrier are written for one running mutator. With
  several running owners, every owner's barrier and every owner's allocation
  during a cycle must reach the marker, and the handshakes must stop every
  owner.
- **Detached execution and safepoints** (S3, S4). Scalar- and Sealed-class
  code running detached, every detached owner stopping at a safepoint for a
  collection, and a counter for every reason an owner attaches.
- **Per-owner allocation** (S5), so allocation is not a reason to attach.
- **Buffer ownership.** Every buffer primitive attaches today; a per-buffer
  owner and its lock are designed and not built.
- **The `MustReset` thread-local audit.** Every `thread_local!` classified in
  `carrier_tls_class_test.rs` as reset-on-carrier-change must be shown
  correct for an owner that runs concurrently with others, not only after
  another owner has finished.
- **JIT-compiled code's attach discipline.** Compiled code calls primitives
  directly; it must attach before a World-class call exactly as the
  interpreter does, and its stack maps must be reachable from a stopped
  owner.
- **Display.** Redisplay and frame state are World state; a thread that
  touches them attaches, and a parallel redisplay is out of scope.
- **Measurements.** A release build with the feature, the GNU thread suite in
  every mode, and the S1 scenarios with sim, green, parallel and isolate
  columns on one machine.
