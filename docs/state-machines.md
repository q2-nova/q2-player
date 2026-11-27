# State machines

The player's state machines are in [`crates/machines`](../crates/machines/)
(`q2-machines`). Each is an explicit state type and a pure function
`transition(state, event) -> (state, effect)`: no I/O, clock, lock,
allocation or atomic. The code in `q2nd` that calls a machine (its shell)
builds the event from what it observed, stores the returned state and
carries out the returned effect. The threads and runtime rules the shells
run under are in the [architecture](architecture.md#execution).

```sh
cargo kani -p q2-machines
cargo test --workspace --locked
```

The first command runs the Kani proof harnesses (`#[cfg(kani)]` in each
module). CI does not run Kani: run it after changing a machine. The second
runs the exhaustive model checks in `crates/machines`, the player-thread
property tests and the UI property test with the other tests.

## Inventory

| Machine | Module | Shell | State |
| --- | --- | --- | --- |
| Queue cursor and play state | `queue` | `audio/player.rs` `Engine::handle`, `seek_at`; `audio/player/queue.rs` `run_queue`, `ended`, `failed` | `Queue { len, index, play, failures }` |
| Power/storage gate | `handoff::transition` | `audio/player/output.rs` `Engine::gate` | `Open`, `Frozen` |
| Barrier claim | `handoff::claim` | `PlayerHandle::prepare_barrier_async`, `cancel_handoff` (modelled; the handle keeps a pending flag and a revision) | `Free`, `Held` |
| Output sink lifecycle | `output` | `audio/player/output.rs` `ensure_sink_format`, `park_with`, `idle`, `idle_wait`, `resume` | `Closed`, `Open { params, flow }`, `Parked { params }` |
| Cache transfer slot | `transfer` | `library/streaming.rs` `start_fetch`, `TransferUse::drop`, `cancel_orphans`; `cache_lookup.rs` (join); `cache_storage.rs` (thread exit) | `Option<Slot { users, orphaned, cancelled, playback }>` |
| Wi-Fi radio intent | `radio` | `net/radio.rs` `Radio`, owned by the Wi-Fi monitor task (`net/owner.rs`), or by the handle when Wi-Fi is simulated | `Intent { generation, requested, offline, doze, error, fault }` |
| Doze policy | `doze` | `ui/doze.rs` `step`, called from `ui/mod.rs` `doze_tick` | dozing or not (carried in the inputs) |
| Doze telemetry cycle | `doze_cycle` | `ui/doze_log.rs` `DozeLog` | `Idle`, `Dozing`, `Awake` |
| Sleep/power-off preparation | `power` | `ui/mod.rs` `prepare_power`, `UiMsg::PowerPrepared`, the tick, `finish_power` | `Idle`, `Preparing`, `Ready { off }`, `Entering`, `AwaitingOff` |
| USB drive request | `usb` | `usb.rs` `UsbDrive::start_async` | `Idle`, `Opening` |
| Navidrome reachability | `server` | `library/health.rs`, one atomic word | `Health { status, failures, probing, transport_down }` |

Each machine has one caller of `transition`, or its state lives in one
atomic word that each event updates with a compare-and-swap (`server`,
`usb`). The USB handoff's later phases (quiesce, unmount, export, return)
run in the OS helper `/sbin/q2-usb-storage`; the player's part is the request
above and the player's `PrepareHandoff` barrier.

## How a property is established

| Method | What it covers | Bound |
| --- | --- | --- |
| Kani | every state and event of one transition, symbolically | none unless stated (`usize`, `u32`, `u64` in full) |
| Kani, sequence | every sequence of N events from the stated start | N as stated |
| Model check | every state reachable in a finite abstraction, by breadth-first enumeration in a `#[test]` (`model::explore`) | the abstraction's bound as stated |
| Enumeration | every input of a small domain in a `#[test]` | as stated |
| Property test | generated command sequences through the real threads | cases and length as stated; fixed seed |

Kani proves the transition functions. It does not prove that the shell
builds the right event or carries out the effect: debug-build conformance
assertions (`Engine::conforms`, `Engine::output_view`) and the property
tests cover that link by testing.

## Queue cursor and play state (`queue`)

States: `len`, `index`, `play` in `Stopped`, `Playing`, `Paused`,
`Buffering`, and `failures` (consecutive songs that failed).

| Event | From | To | Effect |
| --- | --- | --- | --- |
| `Load { len, index, autoplay }` | any | index clamped; `Stopped` if empty, else `Buffering` (autoplay) or `Paused` | `Start` or none |
| `PlayPause` | active | `Paused` | `Pause` |
| `PlayPause` | `Paused`, non-empty | `Buffering` | `Resume` |
| `PlayPause` | `Stopped`, non-empty | `Buffering` | `Start` |
| `PlayPause`, `Resume` | empty queue | unchanged | none |
| `Pause` | active | `Paused` | `Pause` |
| `Resume { current }` | `Paused`, current instance, non-empty | `Buffering` | `Resume` |
| `Next { repeat }` | not last | next index, `Buffering` | `Start` |
| `Next { repeat: All }` | last | index 0, `Buffering` | `Start` |
| `Prev { restart }` | restart or index 0, non-empty | unchanged (the seek decides) | `Restart` |
| `Prev` | otherwise | previous index, `Buffering` | `Start` |
| `Jump(i)` | `i < len` | index `i`, `Buffering` | `Start` |
| `Insert { count, next }` | empty | index 0, `Paused` | `Insert { at: 0, arm }` |
| `Insert` | non-empty | `len + count` | `Insert { at }` |
| `Remove(i)` | `i < len` | index shifted, or stop/start/reload when it was the selection | `Remove(Keep / Stop / Start / Reload)` |
| `Ended` | empty queue | `Stopped` | `StopAtEnd` |
| `Ended { repeat, track, scrubbing }` | book's last file | `Stopped` | `FinishBook` |
| `Ended` | station | `Paused` | `RadioEnded` |
| `Ended` | scrub gesture active | unchanged | `HoldScrub` |
| `Ended` | last song, repeat off | `Stopped` | `StopAtEnd` |
| `Ended` | otherwise | same, first or next index, `Buffering` | `Advance` |
| `Failed { track }` | book or station | `Paused` | `PauseWithError` |
| `Failed` | third failure in a row, last song, or empty queue | `Stopped` | `StopFailed` |
| `Failed` | otherwise | next index | `SkipFailed` |
| `Seek { radio }` | not paused, non-empty, not a station | `Buffering` | none |
| `Opening`, `Opened`, `Accepted` | active | `Buffering`, failures 0, `Playing` | none |
| `Held` | any | `Paused` | none |

An event that does not apply leaves the state unchanged. `Held` is the sleep
barrier, an output park, an output fault or a failed position save pausing
the player from outside the queue; it can pause a stopped or empty player.

`Opening`, `Opened`, `Accepted` and `Held` are modelled so the machine is
closed under everything the player does; the player assigns those states
directly (`open_current_inner`, `step`, `pause_output`, `output_failed`).
The other events are wired: the player calls `transition` and acts on the
effect.

| Invariant | Method | Bound |
| --- | --- | --- |
| `index < len`, or `len == 0 && index == 0`; an empty queue is never active; `failures < 3` (preserved by every transition; no arithmetic overflow) | Kani `transition_preserves_validity_and_never_overflows` | none |
| A transition into `Stopped` sets `publish` | Kani `entering_stopped_is_always_published` | none |
| The last song ending with repeat off stops, with `StopAtEnd` and `publish` | Kani `the_last_song_ending_with_repeat_off_stops_and_publishes` | none |
| An empty queue is never made active | Kani `an_empty_queue_is_never_made_active` | none |
| A station or book never skips on failure; a station never advances | Kani `a_station_or_book_never_skips_on_failure_and_a_station_never_advances` | none |
| All of the above on every reachable state; every valid state is reachable; an ignored event changes nothing; insert position within the queue | Model check `every_reachable_queue_state_is_valid_and_stops_are_published` | queues up to 5 entries |
| The player thread's queue contents, selection and play state equal the machine's after every command | Property test `the_player_thread_refines_the_queue_machine_under_generated_commands` | 24 cases of up to 21 commands |
| Queue end is published as `Stopped`; no audio after it; positions do not go back during undisturbed playback | Property test `short_songs_under_skip_storms_end_stopped_and_silent` | 10 cases of up to 13 commands |

## Power/storage gate and claim (`handoff`)

The gate belongs to the player thread. While it is `Frozen`, every command
except `Quit` and the barrier commands is dropped, so nothing reopens the
card or the PCM during sleep or USB export.

| Event | From | To | Effect |
| --- | --- | --- | --- |
| `Prepare` | `Open` | `Open` | `Attempt` |
| `Prepare` | `Frozen` | `Frozen` | `Refuse` |
| `Prepared { ok: true }` | any | `Frozen` | none |
| `Prepared { ok: false }` | any | unchanged | none |
| `AckLost`, `Cancel` | any | `Open` | none |
| `Command { quit }` | `Frozen`, not quit | `Frozen` | `Ignore` |
| `Command` | otherwise | unchanged | `Run` |

The claim belongs to the handle: `Free`, `Held`. `Acquire` from `Free`
sends the prepare; from `Held` it answers busy. A failed answer, a timeout
and a release each send `CancelHandoff` before the claim is free again, so a
late success cannot leave the player frozen.

| Invariant | Method | Bound |
| --- | --- | --- |
| The gate freezes only on a successful prepare; cancel and a lost acknowledgement open it; a frozen gate runs only `Quit` | Kani `the_gate_freezes_only_on_a_successful_prepare_and_cancel_opens_it` | none |
| No command runs between a delivered freeze and its cancel | Kani sequence `no_command_runs_between_a_delivered_freeze_and_its_cancel` | 6 events |
| Giving up a claim always queues a cancel | Kani `giving_up_a_claim_always_queues_a_cancel` | none |
| No stranded freeze (quiescent and unclaimed implies `Open`); the barrier is never granted twice; a holder's player is frozen; a prepare never reaches a frozen gate | Model check `no_interleaving_strands_a_frozen_player_or_grants_the_barrier_twice` | 2 callers, 2 attempts each, 1 other command, FIFO channel of 5, every timeout point |
| Commands behind a granted sleep barrier change nothing; a second claim is refused | Property test `the_player_thread_refines_the_queue_machine_under_generated_commands` | as above |

## Output sink lifecycle (`output`)

States: `Closed`; `Open { params, flow }` with `flow` in `Running`,
`Retained` (paused, unheard queue held), `Unverified` (paused, the device
queue's ownership unproven); `Parked { params }` (no PCM, the unheard queue
kept in RAM). A PCM opens only for 1 to 768,000 Hz and 1 to 8 channels
(`Params::valid`).

| Event | From | To | Effect |
| --- | --- | --- | --- |
| `Ensure(p)` | `Open` with `p` | unchanged | `Reuse` |
| `Ensure(p)` | `Open` with other parameters | `Closed` | `DrainThenOpen(p)` |
| `Ensure(p)` | `Closed`, `Parked` | unchanged | `Open(p)` |
| `Opened { params, ok }` | no PCM, ok | `Open { params, Running }` | none |
| `Write` | `Open`, `Running` | unchanged | `Accept` |
| `Write` | otherwise | unchanged | `Refuse` |
| `Paused { retained, unverified }` | `Open` | `Unverified`, `Retained` or `Running` (discarded) | none |
| `Flushed` | `Open` | `Running` | none |
| `Flushed` | `Parked` | `Closed` | none |
| `Park { paused, retainable }` | `Open`, verified, paused and retainable | `Parked { params }` | `Park` |
| `Park` | `Open`, verified, otherwise | `Closed` | `Release` |
| `Park`, `Idle` (20 s reached) | `Open { Unverified }`, player not paused (stopped after a failure) | `Closed` | `Release` |
| `Park`, `Idle` | `Open { Unverified }` with the player paused, `Closed`, `Parked` | unchanged | none |
| `Idle { idle_ms, .. }` | `Open`, 20 s reached | as `Park` | as `Park` |
| `Resume { ok }` | `Parked { p }` | `Open { p, Running }` if ok | `Replay(p)` |
| `Resume { ok }` | `Open`, paused | `Running` if ok | `ResumeSink` |
| `Release` | not `Closed` | `Closed` | `Release` |

Every open PCM has an idle deadline (`idle_wait_ms`, 20 s after playback
stops). At it, a paused unverified queue is revalidated
(`Sink::revalidate_paused`, a new delay observation): established, it parks
like any other; otherwise it stays open and is retried a deadline later. A
stopped player's PCM closes at the deadline whatever its flow, so the DAC
powers down. The `Sink`'s own pause bookkeeping in `audio/alsa.rs` (hardware
pause, software snapshot, PREPARED transfer) is the source of `flow`; it is
not a machine.

| Invariant | Method | Bound |
| --- | --- | --- |
| Park then resume reopens with the parked parameters | Kani `park_then_resume_reopens_with_the_parked_parameters` | none (all `u32` rates and channel counts) |
| Only a running PCM accepts audio | Kani `only_a_running_pcm_accepts_audio` | none |
| No write is accepted after a release until a PCM opens | Kani sequence `no_write_is_accepted_after_a_release_until_a_pcm_opens` | 5 events |
| A paused unverified queue is never closed by park or idle | Kani `a_paused_unverified_queue_is_never_closed_by_park_or_idle` | none |
| Every open PCM has a deadline; at it a player that is not paused closes it, and a paused one closes every verified flow | Kani `every_open_pcm_has_a_deadline_and_a_stopped_player_closes_it` | none |
| Held parameters change only through an open from a state with no PCM | Kani sequence `held_parameters_change_only_through_an_open` | 4 events |
| Whether a park acts does not depend on retainability (the player probes before copying the queue) | Kani `whether_a_park_acts_does_not_depend_on_retainability` | none |
| All of the above on every reachable state | Model check `every_reachable_output_state_refuses_writes_unless_running_and_keeps_parked_params` | 2 parameter sets |
| The open PCM's rate is the playing song's; no frames reach the PCM while stopped or paused; asleep implies no PCM | Property tests (queue section) | as stated there |

An open PCM and a parked queue never coexist: the state type cannot express
it, and `Engine::output_view` asserts it of the engine in debug builds.

## Cache transfer slot (`transfer`)

The library keeps one slot per (song, variant). The player's streams and the
fetch-ahead worker each take a hold on the slot's transfer. When the last
hold drops, the transfer is orphaned and keeps downloading. A sweep (the
player opened another song, or fetch-ahead moved on) cancels orphans that
are not wanted and are under 80 % downloaded (`RETAIN_PERCENT`). A cancelled
transfer's thread stops at its next read and removes the slot if the slot is
still its own; a new open meanwhile replaces the slot with a fresh transfer
writing its own partial file.

| Event | From | To | Effect |
| --- | --- | --- | --- |
| `Open { playback }` | slot, not cancelled | `users + 1`, not orphaned | `Join { rejoined }` |
| `Open` | none, or cancelled | unchanged | `StartNew` |
| `Started { playback }` | none, or cancelled | fresh slot, `users` 1 | `Spawn` |
| `Released { own: true }` | slot | `users - 1`; orphaned at 0 | `Orphaned` at 0 |
| `Released { own: false }` | any | unchanged | none |
| `Sweep { keep, retain }` | orphan, not kept, not retained, not cancelled | cancelled | `Cancel` |
| `Finished { own: true }` | slot | none | `Remove` |
| `Finished { own: false }` | any | unchanged | none |

`own` is identity: the hold or the thread belongs to the transfer now in the
slot. The shell reads and applies the slot under the library's `inflight`
lock (`Transfer::slot`, `Transfer::apply`).

| Invariant | Method | Bound |
| --- | --- | --- |
| `orphaned` exactly when `users == 0`; a cancelled slot has no holds | Kani `transitions_preserve_slot_validity` | none |
| A cancelled transfer is never joined | Kani `a_cancelled_transfer_is_never_joined` | none |
| Only an unwanted, incomplete orphan is cancelled; the cancel flag is never cleared | Kani `only_an_unwanted_incomplete_orphan_is_cancelled` | none |
| A new transfer starts only when none is joinable | Kani `a_new_transfer_starts_only_when_none_is_joinable` | none |
| At most one live (uncancelled, running) transfer per key; `users` equals the holds on the slot's transfer; no reader holds a cancelled transfer; no slot outlives its thread | Model check `one_key_never_has_two_live_transfers_and_no_reader_holds_a_cancelled_one` | 1 key, up to 4 transfers, 3 concurrent holds, every interleaving of open, drop, sweep and thread exit |
| Over the real library, its stream threads and a local server: every slot stays valid, a slot never counts more holds than there are readers, and with all readers gone no transfer or partial file remains | Property test `generated_opens_drops_and_sweeps_keep_slots_valid_and_leak_nothing` | 8 cases of up to 15 operations on 3 songs |

## Wi-Fi radio intent (`radio`)

Every change of intent takes a new generation. Helper operations carry the
generation they were started for; a completion or failure for an older
generation changes nothing, so a slow helper never undoes a newer choice. A
failure never turns the saved Wi-Fi choice off: it rolls back only the
temporary suppression (doze, else Offline). A failed doze rolls back
silently.

| Event | Condition | Result | Effect |
| --- | --- | --- | --- |
| `Request { requested, offline }` | any | next generation; doze, error and fault cleared | `Apply` |
| `Doze(on)` | changes doze; to start: requested and not offline | next generation | `Apply` |
| `Doze(on)` | otherwise | unchanged | none |
| `Failed(g)` | `g` current, dozing | doze off | `RolledBack` |
| `Failed(g)` | `g` current, not dozing | offline off, fault set | `RolledBack` |
| `RestoreFailed(g)` | `g` current | fault set | none |
| `Failed(g)`, `RestoreFailed(g)` | `g` stale | unchanged | none |
| `TakeFault` | fault unseen | fault seen | `Show { offline, fault }` |

| Invariant | Method | Bound |
| --- | --- | --- |
| `doze` implies requested and not offline; a fault always has its message; generations never decrease | Kani `transitions_preserve_validity_and_generations_never_decrease` | none (all `u64` generations) |
| A stale generation changes nothing | Kani `a_stale_generation_changes_nothing` | none |
| Doze starts only on a saved-on radio; any request ends it; a failure never changes the saved choice or the generation | Kani `doze_starts_only_on_a_saved_on_radio_and_any_request_ends_it` | none |
| A shown fault always has its message, once | Kani `a_shown_fault_always_has_its_message` | none |
| The same on every reachable intent; an intent change without rollback takes a new generation | Model check `every_reachable_intent_is_valid_and_stale_events_change_nothing` | 6 generations |

## Doze policy (`doze`)

With the screen off and enough complete on-card audio queued, the radio goes
down (POWER-006), and comes back when that audio runs low or the listener
needs the network. Inputs per UI tick: enabled (`power.radio_doze`), Wi-Fi
wanted, connected, dozing, playing, stopped, screen on, idle time, local
audio ahead, fetching, and paused doze (the screen dark and untouched for
`power.doze_paused_minutes`, default 1; 0 turns it off). The machine also
takes an AirPlay input, which the shell always passes as false.

Thresholds `doze_ms` and `wake_ms` come from the configuration. With fetch
by duration (`storage.fetch_minutes` above 0, the default), they are
`Thresholds::after_fetch(storage.fetch_low_minutes)`: wake at the fetch
margin plus a minute's reconnect allowance (`RECONNECT_MS`), doze with at
least four minutes more than that; the defaults give 4 and 8 minutes.
Otherwise they are `power.doze_ahead_minutes` and `power.doze_wake_minutes`
(30 and 5). A wake margin of zero, or one not below the doze threshold,
falls back to 30 and 5.

| From | Condition | Effect |
| --- | --- | --- |
| awake | enabled, wanted, connected, playing, screen off, no AirPlay, not fetching, idle 30 s, ahead at least `doze_ms` | `Doze` |
| awake | as above but paused (not playing, not stopped) with paused doze, ahead at least `wake_ms` | `Doze` |
| dozing | disabled, Wi-Fi off or Offline, screen on, AirPlay, stopped, or ahead below `wake_ms` (reported in that order) | `Wake(reason)` |
| either | otherwise | none |

| Invariant | Method | Bound |
| --- | --- | --- |
| Every thresholds constructor gives a wake margin of at least a minute, below the doze threshold | Kani `every_constructor_keeps_a_wake_margin_of_a_minute_below_the_doze_threshold` | minutes up to 65,535; `u32` extremes by enumeration |
| A doze never starts under a fetch, with the screen on, under AirPlay, unconnected, disabled, unsettled or short of audio (the doze threshold while playing; the wake margin while paused, only with paused doze and never stopped) | Kani `a_doze_never_starts_under_a_fetch_or_while_the_owner_needs_the_radio` | none, for all valid thresholds |
| A dozing radio wakes exactly for the listener's causes or low audio; a radio left dozing has at least the wake margin ahead | Kani `a_dozing_radio_wakes_before_the_audio_runs_out_and_for_the_owner` | none, for all valid thresholds |
| A doze that just started is not woken by the same inputs | Kani `a_doze_that_just_started_is_not_woken_by_the_same_inputs` | none |
| The full policy table | Enumeration `the_policy_table_holds_for_every_flag_combination_and_boundary_value` | 10 flags × 7 audio values × 4 idle values × 8 thresholds |
| The UI's radio state equals the policy's after every generated event | Property test `the_app_follows_the_doze_policy_and_power_machine_under_generated_events` | 24 cases of up to 29 events, paused doze off |
| A pause dozes only after the delay of dark, untouched screen; a lit screen never dozes and wakes a doze; stopped never dozes; playing is unchanged by the paused delay | Tests `paused_dozes_only_dark_and_playing_keeps_its_rules` (`ui/doze.rs`) and `by_default_a_pause_dozes_only_after_a_minute_of_dark_screen` (`ui/doze_tests.rs`) | the default delay and its boundaries |

The UI recomputes the audio ahead at most 10 s apart (`RECHECK_MS`) and the
wake margin is at least 60 s, so a wake is requested with at least 50 s of
cached audio left, by the player's own estimate. Whether the radio
reconnects and refills in that time depends on the network and is not
proved.

## Doze telemetry cycle (`doze_cycle`)

`Idle` -> `Dozing` on `Dozed`; `Dozing` -> `Awake` on `Woke`; `Awake`
measures on each `Sample` (time to reconnect, time to the end of the latest
fetch burst, least cached audio while playing). A cycle closes, emitting one
record, on the next `Dozed`, on `Finish`, or on the first sample 30 minutes
after the wake. Times are monotonic nanoseconds; differences saturate, so a
clock that steps back yields zero.

| Invariant | Method | Bound |
| --- | --- | --- |
| A record is emitted exactly when an open cycle closes; `ran_out` exactly when the least cached audio while awake was zero | Kani `a_record_is_emitted_exactly_when_an_open_cycle_closes` | none |
| Reconnect is measured once; refill reopens only on a fetch; the minimum never rises | Kani `reconnect_is_taken_once_refill_reopens_only_on_fetch_and_the_minimum_never_rises` | none |
| A woken cycle does not stay open past the close deadline | Kani `a_woken_cycle_does_not_stay_open_past_the_close_deadline` | none |

## Sleep/power-off preparation (`power`)

A power transaction saves settings, asks the player and the library for a
durable barrier in a task on the I/O runtime, waits for the keys to be
released, enters suspend or power-off, and afterwards releases the barrier
it took. While the state is not `Idle`, input is observed but not acted on.

| Event | From | To | Effect |
| --- | --- | --- | --- |
| `Request` | `Idle` | `Preparing` | `Prepare` |
| `SpawnFailed` | `Preparing` | `Idle` | `Unavailable` |
| `Prepared { off, ok: true }` | any | `Ready { off }` | none |
| `Prepared { ok: false }` | any | unchanged | `Failed` (the shell then sends `Finished`) |
| `Tick { keys_held: false }` | `Ready { off }` | `Entering` | `Enter { off }` |
| `AwaitOff` | any | `AwaitingOff` | none |
| `Finished` | any | `Idle` | `Restore { cancel_barrier }` |

`cancel_barrier` is true in `Ready`, `Entering` and `AwaitingOff`, so a
refused preparation never cancels a barrier that belongs to a USB export.
The shell starts the preparation as a task and never sends `SpawnFailed`.

| Invariant | Method | Bound |
| --- | --- | --- |
| Power is entered only from `Ready` with keys released; `Ready` only after a successful preparation; a preparation starts only from `Idle` | Kani `power_is_entered_only_from_ready_with_keys_released` | none |
| Finishing returns to `Idle` and cancels exactly a held barrier | Kani `finishing_returns_to_idle_and_cancels_exactly_a_held_barrier` | none |
| A barrier is claimed only after a successful preparation | Kani sequence `a_barrier_is_claimed_only_after_a_successful_preparation` | 6 events |
| One preparation at a time under the UI loop's protocol | Model check `suspend_is_entered_only_after_a_successful_preparation_with_keys_released` | every reachable state |

## USB drive request (`usb`)

`Idle` -> `Opening` on `Start { available: true }` (`Spawn`); a start while
`Opening` is refused busy; an unavailable start is refused in either state;
`WorkerDone` and `SpawnFailed` return to `Idle`. The shell keeps the state
in one `AtomicBool`: a test-and-set claims the request, and the request's
guard clears it when the task ends. The task checks that a computer has
configured the port before it takes the player's storage barrier and runs
the helper; on success the supervisor stops the player, so there is no
exported state.

| Invariant | Method | Bound |
| --- | --- | --- |
| Every step keeps the request invariants | Kani `any_step_from_any_state_keeps_the_request_invariants` | none |
| Two handoff workers never run together; a refusal changes nothing | Kani sequence `two_handoff_workers_never_run_together` | 6 events |

## Navidrome reachability (`server`)

[Server status](server-status.md) has the signals, thresholds and bounds.
Any answer is `Online`. A foreground or background failure from `Online` is
`Checking` and starts the probe; the probe's failure, the third foreground
failure in a row or a transport going down is `Offline`. Low-priority
failures change nothing. A transport coming back, or the offline cadence,
starts a probe from `Offline`. Opening Settings › Connections (`Recheck`)
starts a probe unless one runs or the transport is down; from `Online` it
shows `Checking` until the probe ends. A reset (server or account changed)
is the initial `Offline`. The shell keeps the state and the reset generation
in one `AtomicU32` and applies each event with a compare-and-swap; a probe's
result counts only for its generation.

| Invariant | Method | Bound |
| --- | --- | --- |
| No single failure takes an online server offline | Kani `one_failure_never_goes_offline_from_online` | none |
| Low-priority failures change nothing | Kani `low_priority_failures_change_nothing` | none |
| Any answer is online with nothing counted | Kani `any_success_is_online` | none |
| Offline only on a failed probe, the third foreground failure, transport down or reset | Kani `offline_only_on_evidence`; enumeration `every_state_and_event_follows_the_rules` | none; every valid state and event |
| At most one probe runs; `Checking` always has one; the invariants hold | Kani sequence `one_probe_at_a_time_and_checking_always_has_one`; model check `every_reachable_state_keeps_the_invariants` | 8 events; every reachable state |

## Validation at the boundaries

| Input | Rule | Test |
| --- | --- | --- |
| `Cmd::Play`/`Restore` index, `Jump`, `Remove`, `Insert` | clamped or ignored by the queue machine | Kani (queue); `out_of_range_indices_positions_and_volume_are_clamped_or_ignored` |
| A book's saved position index (`TrackSource::spoken_resume`) | clamped to the queue | queue machine `Load` |
| `Cmd::SeekBy` offset | saturating arithmetic | same test |
| `Cmd::Restore` position | at most the song's reported length plus a second; the decoder's exact length applies at open | same test |
| Decoder sample rate and channels | 1 to 768,000 Hz, 1 to 8 channels, else "Unsupported audio format" and no PCM is opened | `only_openable_parameters_are_valid`; `a_format_no_pcm_can_open_ends_playback_with_an_error_and_never_reaches_the_output` |
| ReplayGain gain and peak from the server | a non-finite gain or factor leaves the song unchanged; a non-finite or non-positive peak is ignored | `replay_gain_from_the_server_never_yields_a_non_finite_or_clipping_factor` |
| Transfer sizes | the 80 % rule computed without overflow; a raw stream shorter than its Content-Length is not cached | `transfer_sizes_from_the_server_are_validated_without_overflow`; `a_raw_transfer_shorter_than_its_declared_length_is_never_cached` |
| `[storage] stream_cache_mb`, `reserve_mb` | saturating conversion to bytes | `transfer_sizes_from_the_server_are_validated_without_overflow` |
| `[playback] volume`, `max_volume`, `[display] brightness` | limit within 1 to 100, volume within the limit, brightness within 1 to 100, at load | `out_of_range_numbers_are_clamped_when_settings_load` |
| `[power] doze_ahead_minutes`, `doze_wake_minutes`, `[storage] fetch_low_minutes` | thresholds always keep a wake margin of at least a minute below the doze threshold | Kani (doze) |
| Outputs: every `save_queue` index, every published `Status` | index within the queue; empty queue never active; generation never decreases; open PCM at the playing song's rate | property tests |

## Effect owners

Each resource that a machine's effects touch, and each device or file the
player keeps open, has one owner. Other code reaches it through the owner's
channel or handle and awaits the reply; it never takes the resource itself.

| Resource | Owner | How other code reaches it |
| --- | --- | --- |
| SQLite connection | `db` thread (`library/db_worker.rs`) | Requests with owned arguments, run in queue order; callers await the reply. A request never waits for another task. |
| Decoder, PCM sink, mixer, queue | `player` thread (`audio/player.rs` `Engine`) | `PlayerHandle` commands; the UI reads the published `Status`. |
| Radio intent, supplicant socket, network helper | Wi-Fi monitor task on `io` (`net/owner.rs`) | `Net` commands; the UI and running operations read the published copy (`net/radio.rs` `Shared`). |
| Tailscale CLI and daemon processes | Supervisor task on `io` (`tailscale.rs`); fork and exec on the `ts-spawn` thread, which exists only while Tailscale is enabled (`tailscale/spawn.rs`) | Supervisor requests. Stop retires the CLI, then the daemon, then the spawn thread; a daemon gets SIGTERM and 3 s before it is killed. |
| Cache transfer slots | Library `inflight` map, under its lock | `start_fetch` and the transfer machine (above). |
| Streamed song read-ahead | One `read-ahead` lane per reader (`audio/readahead.rs`) | The decoder's reader; an obsolete reader's card syscall never delays a new song. |
| Download preparation and publication | `downloads` lane (`library/downloads.rs`); network waits on `io` | The download queue. An empty queue waits on a notification; queued work retries every 15 s. |
| Buttons and wheel | One readiness task each on `io`, at most 64 reads per batch; discovery and `SYN_DROPPED` snapshots on the `input-scan` lane (`platform/evdev.rs`) | `UiMsg` input messages. |
| USB audio hotplug | Netlink task on `io`; sysfs scans on the `usb-audio-scan` lane (`audio/output.rs` `Monitor`) | Route changes queued to the player. |
| Cover art memory cache | Two artwork tasks on `io` (`library/art.rs`), holding a weak `Library` reference; file I/O and decoding in `spawn_blocking` jobs | Cover requests; the decoded cache holds up to 3 MiB. |
| Log sinks | Logger drain task on `io`; sink writes are finite jobs (`runtime_log.rs`) | Nonblocking admission to a queue of 64 records (48 for ordinary records, the rest for warnings and errors), written in batches of 8; records are cut at 2 KiB. |
| Numeric metrics and boot markers | Metrics recorder (`metrics.rs`); file writes are finite jobs | Nonblocking admission to a queue of 256 entries, written in batches of 32. |

A lane is a capacity-one blocking pool (`rt::Jobs::serial`) with no thread
until first use; its worker exits after 10 s idle. It keeps one kind of card
or sysfs work from waiting behind the shared job pool.

## Not covered

- The decoder, tempo, EQ and gain path, and the `Sink`'s pause and retain
  internals, are tested but not modelled.
- The generation counters, scrub leases and spoken (book) position
  arithmetic stay in the player.
- The transfer model is one key. Keys do not interact in the code (each has
  its own slot), but that independence is by inspection.
- The library's fetch-ahead scheduling and the Wi-Fi supplicant monitor are
  not machines.
- The reachability shell's request classification (priority scopes, failure
  classes) and the transport input are tested (`library::health_tests`),
  not proved.
- The ownership table is maintained by review and the lint rules in the
  [architecture](architecture.md#runtime-rules); no tool checks it.
