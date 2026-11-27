#![forbid(unsafe_code)]
//! Headless diagnostic commands on the same async runtime as the player I/O.
//! Explicit input cadences and observation deadlines retain their values.
//! Readiness is published by the UI owner, without a periodic probe.
use crate::{audio, input, numeric_probe, playtrace, ui::{self, UiMsg}};
use std::{path::PathBuf, time::Duration};

pub(crate) fn run(path:PathBuf,tx:ui::UiTx)->tokio::task::JoinHandle<()> {
    crate::rt::io().spawn(async move {
        let text=crate::rt::spawn_blocking(move |_|std::fs::read_to_string(&path).unwrap_or_else(|error| {
            eprintln!("{}: {error}",path.display());std::process::exit(2);
        })).await;
        match text {
            Ok(text)=>execute(text,tx).await,
            _=>{let _=tx.send(UiMsg::ScriptFailed("Unable to read headless script".into()));},
        }
    })
}
async fn receive<T>(receiver:tokio::sync::oneshot::Receiver<T>,limit:Duration)->Result<T,()> {
    tokio::time::timeout(limit,receiver).await.map_err(|_|())?.map_err(|_|())
}
/// Subscribe once, inspect the current value, then wait for owner changes.
/// The deadline includes subscription delivery and is never renewed by updates.
async fn observe<T: Clone + std::fmt::Debug>(
    tx: &ui::UiTx,
    request: impl FnOnce(tokio::sync::oneshot::Sender<tokio::sync::watch::Receiver<T>>) -> UiMsg,
    deadline: tokio::time::Instant,
    mut ready: impl FnMut(&T) -> Result<bool, String>,
) -> Result<(), String> {
    let (reply, received) = tokio::sync::oneshot::channel();
    tx.send(request(reply)).map_err(|_| "UI owner stopped".to_string())?;
    let mut states = tokio::time::timeout_at(deadline, received).await
        .map_err(|_| "subscription timed out".to_string())?
        .map_err(|_| "UI owner stopped".to_string())?;
    loop {
        let state = states.borrow_and_update().clone();
        if tokio::time::Instant::now() >= deadline { return Err(format!("timed out; last state {state:?}")); }
        if ready(&state)? { return Ok(()); }
        tokio::time::timeout_at(deadline, states.changed()).await
            .map_err(|_| format!("timed out; last state {state:?}"))?
            .map_err(|_| "UI observer closed".to_string())?;
    }
}
async fn authorize(name:&str)->Result<numeric_probe::Output,String> {
    let name=name.to_owned();
    crate::rt::spawn_blocking(move |blocking|numeric_probe::Output::authorize(blocking, &name)).await
        .unwrap_or_else(|_|Err("observation authorization stopped".into()))
}
async fn execute(text:String,tx:ui::UiTx) {
    use input::{Key,Raw};
        let key = |k: &str| match k {
            "center" => Some(Key::Center),
            "back" => Some(Key::Back),
            "play" => Some(Key::PlayPause),
            "prev" => Some(Key::Prev),
            "next" => Some(Key::Next),
            _ => None,
        };
        let send = |m: UiMsg| {
            let m=match m {
                UiMsg::Input(raw) => UiMsg::TimedInput {raw,received:std::time::Instant::now()},
                m=>m,
            };
            let _ = tx.send(m);
        };
        // main starts the script only after the first successful presentation.
        let mut last_input = std::time::Instant::now();
        for line in text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
            let started = std::time::Instant::now();
            let (cmd, arg) = line.split_once(' ').unwrap_or((line, ""));
            match cmd {
                "await" => {
                    let Some((milliseconds, target)) = arg.split_once(' ') else { send(UiMsg::ScriptFailed("await requires milliseconds and a screen title".into())); return; };
                    let Some(milliseconds) = milliseconds.parse::<u64>().ok().filter(|ms| (1..=60_000).contains(ms)) else { send(UiMsg::ScriptFailed("await timeout must be 1–60000 ms".into())); return; };
                    let (require_idle,target)=if let Some(title)=target.strip_prefix("idle:") {(true,title)} else {(false,target)};
                    let (overlay, target) = if let Some(title) = target.strip_prefix("overlay:") { (Some(true), title) }
                        else if let Some(title) = target.strip_prefix("screen:") { (Some(false), title) } else { (None, target) };
                    let deadline = tokio::time::Instant::now() + Duration::from_millis(milliseconds);
                    let result = observe(&tx, UiMsg::ScriptSubscribe, deadline, |state| {
                        let ready = match target {
                            "shuffle:on" => state.shuffle,
                            "shuffle:off" => !state.shuffle,
                            "repeat:off" => state.repeat == audio::player::Repeat::Off,
                            "repeat:one" => state.repeat == audio::player::Repeat::One,
                            "repeat:all" => state.repeat == audio::player::Repeat::All,
                            _ => state.ready && state.title == target && overlay.is_none_or(|expected| state.overlay == expected),
                        };
                        Ok(ready && (!require_idle || state.idle))
                    }).await;
                    if let Err(error) = result { send(UiMsg::ScriptFailed(format!("await {target:?} {error}"))); return; }
                }
                "await-route" => {
                    let Some((milliseconds,route))=arg.split_once(' ') else {
                        send(UiMsg::ScriptFailed("await-route requires milliseconds and usb/headphones".into()));return;
                    };
                    let Some(milliseconds)=milliseconds.parse::<u64>().ok().filter(|ms|(1..=60_000).contains(ms)) else {
                        send(UiMsg::ScriptFailed("await-route timeout must be 1–60000 ms".into()));return;
                    };
                    if !matches!(route,"usb"|"headphones") {
                        send(UiMsg::ScriptFailed("await-route requires usb/headphones".into()));return;
                    }
                    let _authority=match authorize("route-wait").await {
                        Ok(authority)=>authority,Err(error)=>{send(UiMsg::ScriptFailed(error));return;}
                    };
                    let deadline = tokio::time::Instant::now() + Duration::from_millis(milliseconds);
                    if let Err(error) = observe(&tx, UiMsg::NumericScriptSubscribe, deadline, |value| Ok(value.route_is(route))).await {
                        send(UiMsg::ScriptFailed(format!("await-route {error}"))); return;
                    }
                }
                "await-play" => {
                    // await-play MS playing|paused|stopped|buffering [ADVANCE_MS]
                    let mut args=arg.split_whitespace();
                    let milliseconds=args.next().and_then(|ms|ms.parse::<u64>().ok()).filter(|ms|(1..=60_000).contains(ms));
                    let state=match args.next() {Some("stopped")=>Some(0),Some("playing")=>Some(1),Some("paused")=>Some(2),Some("buffering")=>Some(3),_=>None};
                    let advance=args.next().map(|ms|ms.parse::<u64>().ok()).unwrap_or(Some(0));
                    let (Some(milliseconds),Some(state),Some(advance))=(milliseconds,state,advance) else {
                        send(UiMsg::ScriptFailed("await-play requires 1–60000 ms, a play state and optional advance ms".into()));return;
                    };
                    let _authority=match authorize("play-wait").await {
                        Ok(authority)=>authority,Err(error)=>{send(UiMsg::ScriptFailed(error));return;}
                    };
                    let deadline = tokio::time::Instant::now() + Duration::from_millis(milliseconds);
                    let mut base=None;
                    if let Err(error) = observe(&tx, UiMsg::NumericScriptSubscribe, deadline, |value| Ok(value.reached(state,advance,&mut base))).await {
                        send(UiMsg::ScriptFailed(format!("await-play {arg} {error}"))); return;
                    }
                }
                "record-text" => send(UiMsg::ScriptRecordText),
                "text" => {
                    // Written as numeric-NAME.json in the generated runtime directory.
                    let output=match authorize(arg).await {
                        Ok(output)=>output,Err(error)=>{send(UiMsg::ScriptFailed(error));return;}
                    };
                    let (reply,text)=tokio::sync::oneshot::channel();send(UiMsg::ScriptText(reply));
                    match receive(text,Duration::from_secs(2)).await {
                        Ok(value)=>if let Err(error)=crate::rt::spawn_blocking(move |blocking|output.publish_text(blocking,&value)).await.unwrap_or_else(|_|Err("text publication stopped".into())) {send(UiMsg::ScriptFailed(error));return;},
                        Err(_)=>{send(UiMsg::ScriptFailed("text record unavailable (record-text first)".into()));return;}
                    }
                }
                "memory" | "memory-trim" => {
                    // Numbers only, to the player log: the memory telemetry
                    // sample (platform/memory.rs), and with `memory-trim` a
                    // heap trim and what it took.
                    let trim = cmd == "memory-trim";
                    let name = arg.to_owned();
                    let _ = crate::rt::spawn_blocking(move |blocking| {
                        // For host heap accounting (tests/perf/heapcount.c),
                        // which reports by boot time.
                        log::info!("script mark {name} boottime_us={}", crate::playtrace::boottime_us());
                        let sample = crate::platform::memory::sample(blocking);
                        log::info!("script memory {name}: {}", serde_json::to_string(&sample).unwrap_or_default());
                        if trim {
                            let started = std::time::Instant::now();
                            let result = crate::platform::resident::trim(blocking);
                            log::info!("script memory-trim {name}: {result:?} in {} us", started.elapsed().as_micros());
                        }
                    }).await;
                }
                "fault-descriptor" => {
                    // Generated fault journeys only (UXI-006/022).
                    if let Err(error)=authorize("fault-descriptor").await {send(UiMsg::ScriptFailed(error));return;}
                    crate::audio::readahead::inject_descriptor_fault();
                }
                "await-finished" => {
                    let Some(milliseconds)=arg.parse::<u64>().ok().filter(|ms|(1..=60_000).contains(ms)) else {
                        send(UiMsg::ScriptFailed("await-finished timeout must be 1–60000 ms".into()));return;
                    };
                    let _authority=match authorize("finish-wait").await {
                        Ok(authority)=>authority,Err(error)=>{send(UiMsg::ScriptFailed(error));return;}
                    };
                    let deadline = tokio::time::Instant::now() + Duration::from_millis(milliseconds);
                    let mut first = None;
                    if let Err(error) = observe(&tx, UiMsg::NumericScriptSubscribe, deadline, |value| {
                        value.finished_for(first.get_or_insert_with(|| value.clone()))
                    }).await {
                        send(UiMsg::ScriptFailed(format!("await-finished {error}"))); return;
                    }
                }
                "probe-cursor" | "probe-doze" => {
                    let (timeout, name) = if cmd == "probe-doze" {
                        let Some((milliseconds, name)) = arg.split_once(' ') else {
                            send(UiMsg::ScriptFailed("probe-doze requires milliseconds and name".into())); return;
                        };
                        let Some(milliseconds) = milliseconds.parse::<u64>().ok().filter(|ms| (1..=1_200_000).contains(ms)) else {
                            send(UiMsg::ScriptFailed("probe-doze timeout must be 1–1200000 ms".into())); return;
                        };
                        (Duration::from_millis(milliseconds), name)
                    } else { (Duration::from_secs(2), arg) };
                    let output = match authorize(name).await {
                        Ok(output) => output,
                        Err(error) => { send(UiMsg::ScriptFailed(error)); return; }
                    };
                    let requested_us = playtrace::boottime_us();
                    let deadline = std::time::Instant::now() + timeout;
                    let at_doze = (cmd == "probe-doze").then(|| std::sync::Arc::new(std::sync::OnceLock::new()));
                    let (reply, received) = tokio::sync::oneshot::channel();
                    send(UiMsg::MusicCursorProbe { deadline, reply, at_doze:at_doze.clone() });
                    // Await the existing UI/audio owner directly, with the same
                    // deadline and observation timestamps.
                    match receive(received,deadline.saturating_duration_since(std::time::Instant::now())).await {
                        Ok(Ok(cursor)) => {
                            let received_us = playtrace::boottime_us();
                            let result=crate::rt::spawn_blocking(move |blocking| {
                                if let Some(context)=at_doze {
                                    match context.get() {
                                        Some(boundary)=>output.publish_doze(blocking, &cursor,boundary,requested_us,received_us),
                                        None=>Err("doze observation context missing".into()),
                                    }
                                } else {output.publish_cursor(blocking, &cursor,requested_us,received_us)}
                            }).await.unwrap_or_else(|_|Err("observation publication stopped".into()));
                            if let Err(error) = result {
                                send(UiMsg::ScriptFailed(error)); return;
                            }
                        }
                        Ok(Err(error)) => { send(UiMsg::ScriptFailed(error.into())); return; }
                        Err(_) => { send(UiMsg::ScriptFailed("music cursor owner unavailable or timed out".into())); return; }
                    }
                }
                "probe" => {
                    let output=match authorize(arg).await {
                        Ok(output)=>output,
                        Err(error)=>{send(UiMsg::ScriptFailed(error));return;}
                    };
                    let (reply,state)=tokio::sync::oneshot::channel();send(UiMsg::NumericScriptProbe(reply));
                    match receive(state,Duration::from_secs(2)).await {
                        Ok(value)=>if let Err(error)=crate::rt::spawn_blocking(move |blocking|output.publish(blocking, &value)).await.unwrap_or_else(|_|Err("observation publication stopped".into())) {send(UiMsg::ScriptFailed(error));return;},
                        Err(_)=>{send(UiMsg::ScriptFailed("numeric playback probe timed out".into()));return;}
                    }
                }
                "present" => {
                    let Some(milliseconds)=arg.parse::<u64>().ok().filter(|ms|(1..=60_000).contains(ms)) else {
                        send(UiMsg::ScriptFailed("present timeout must be 1–60000 ms".into()));return;
                    };
                    let (reply,completed)=tokio::sync::oneshot::channel();send(UiMsg::PresentBarrier(reply));
                    if receive(completed,Duration::from_millis(milliseconds)).await.is_err() {
                        send(UiMsg::ScriptFailed("normal presentation did not complete".into()));return;
                    }
                }
                "scroll" => send(UiMsg::Input(Raw::WheelTicks(arg.parse().unwrap_or(1)))),
                "wheel-end" => send(UiMsg::Input(Raw::WheelEnd)),
                "wheel-trace" => {
                    // Bounded generated input, exercising the actual UI queue
                    // and message batching at rates above the frame cadence.
                    let mut args=arg.split_whitespace();
                    let count=args.next().and_then(|s|s.parse::<usize>().ok()).unwrap_or(120).min(512);
                    let ms=args.next().and_then(|s|s.parse::<u64>().ok()).unwrap_or(16).clamp(5,250);
                    for i in 0..count {
                        send(UiMsg::Input(Raw::WheelTicks(if i%40<20 {1} else {-1})));
                        tokio::time::sleep(Duration::from_millis(ms)).await;
                    }
                }
                "wait" => tokio::time::sleep(Duration::from_millis(arg.parse().unwrap_or(100))).await,
                "shot" => send(UiMsg::Screenshot(PathBuf::from(arg))),
                "hold" => {
                    if let Some(k) = key(arg) {
                        send(UiMsg::Input(Raw::Key { key: k, down: true }));
                        tokio::time::sleep(Duration::from_millis(800)).await;
                        send(UiMsg::Input(Raw::Key { key: k, down: false }));
                    }
                }
                k => {
                    if let Some(k) = key(k) {
                        send(UiMsg::Input(Raw::Key { key: k, down: true }));
                        send(UiMsg::Input(Raw::Key { key: k, down: false }));
                    }
                }
            }
            // Wall time of each completed wait (screen ready, presentation,
            // playback state): the step itself, and since the last input,
            // which includes the 150 ms pause after that input's step.
            match cmd {
                "await" | "await-route" | "await-play" | "await-finished" | "present" => log::info!(
                    "script timing {line}: step {:.1} ms, since input {:.1} ms",
                    started.elapsed().as_secs_f64() * 1e3, last_input.elapsed().as_secs_f64() * 1e3),
                "scroll" | "wheel-trace" | "wheel-end" | "hold" | "center" | "back" | "play" | "prev" | "next" => last_input = std::time::Instant::now(),
                _ => {}
            }
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
        send(UiMsg::Quit);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused=true)]
    async fn command_cadence_and_hold_edges_are_preserved() {
        let (tx,mut messages)=tokio::sync::mpsc::unbounded_channel();
        let start=tokio::time::Instant::now();
        let task=tokio::spawn(execute("hold center\nwait 25\nscroll 2\npresent 50\n".into(),tx));
        let mut times=Vec::new();
        while let Some(message)=messages.recv().await {
            times.push(start.elapsed().as_millis());
            match (times.len(),message) {
                (1,UiMsg::TimedInput {raw:input::Raw::Key {key:input::Key::Center,down:true},..})=>{},
                (2,UiMsg::TimedInput {raw:input::Raw::Key {key:input::Key::Center,down:false},..})=>{},
                (3,UiMsg::TimedInput {raw:input::Raw::WheelTicks(2),..})=>{},
                (4,UiMsg::PresentBarrier(reply))=>reply.send(()).unwrap(),
                (5,UiMsg::Quit)=>break,
                _=>panic!("unexpected script command"),
            }
        }
        task.await.unwrap();
        assert_eq!(times,[0,800,1125,1275,1725]);
    }

    fn ready_state(ready: bool, idle: bool, overlay: bool) -> ui::ScriptState {
        ui::ScriptState { title: "Ready".into(), ready, idle, overlay,
            shuffle: false, repeat: audio::player::Repeat::Off }
    }

    #[tokio::test(start_paused=true)]
    async fn ready_barrier_observes_changes_without_polling_and_keeps_idle_overlay_contract() {
        let (tx, mut messages) = tokio::sync::mpsc::unbounded_channel();
        let task = tokio::spawn(execute("await 5000 idle:overlay:Ready".into(), tx));
        let Some(UiMsg::ScriptSubscribe(reply)) = messages.recv().await else { panic!("missing subscription") };
        let (states, receiver) = tokio::sync::watch::channel(ready_state(false, true, true));
        reply.send(receiver).unwrap();
        for (ready, idle, overlay) in [(false, true, true), (true, false, true), (true, true, false)] {
            states.send_replace(ready_state(ready, idle, overlay));
            tokio::task::yield_now().await;
            tokio::time::advance(Duration::from_millis(600)).await;
            tokio::task::yield_now().await;
            assert!(messages.try_recv().is_err(), "no probe polling or premature completion");
        }
        states.send_replace(ready_state(true, true, true));
        assert!(matches!(messages.recv().await, Some(UiMsg::Quit)));
        task.await.unwrap();
        assert!(states.is_closed(), "completed wait releases its subscription");
    }

    #[tokio::test(start_paused=true)]
    async fn observation_updates_do_not_renew_deadline_and_abort_releases_subscription() {
        for abort in [false, true] {
            let (tx, mut messages) = tokio::sync::mpsc::unbounded_channel();
            let task = tokio::spawn(execute("await 1000 Ready".into(), tx));
            let Some(UiMsg::ScriptSubscribe(reply)) = messages.recv().await else { panic!("missing subscription") };
            let (states, receiver) = tokio::sync::watch::channel(ready_state(false, true, true));
            reply.send(receiver).unwrap();
            tokio::task::yield_now().await;
            for index in 0..4 {
                tokio::time::advance(Duration::from_millis(200)).await;
                states.send_replace(ready_state(false, index % 2 == 0, true));
                tokio::task::yield_now().await;
                assert!(messages.try_recv().is_err());
            }
            if abort {
                task.abort();
                assert!(task.await.unwrap_err().is_cancelled());
            } else {
                tokio::time::advance(Duration::from_millis(200)).await;
                let Some(UiMsg::ScriptFailed(error)) = messages.recv().await else { panic!("missing timeout") };
                assert!(error.contains("timed out"));
                task.await.unwrap();
            }
            assert!(states.is_closed());
        }
    }

    #[tokio::test(start_paused=true)]
    async fn observation_refuses_closed_owner_and_accepts_current_ready_state() {
        for ready in [false, true] {
            let (tx, mut messages) = tokio::sync::mpsc::unbounded_channel();
            let task = tokio::spawn(execute("await 1000 Ready".into(), tx));
            let Some(UiMsg::ScriptSubscribe(reply)) = messages.recv().await else { panic!("missing subscription") };
            let (states, receiver) = tokio::sync::watch::channel(ready_state(ready, true, false));
            reply.send(receiver).unwrap();
            drop(states);
            let message = messages.recv().await;
            if ready { assert!(matches!(message, Some(UiMsg::Quit))); }
            else { assert!(matches!(message, Some(UiMsg::ScriptFailed(error)) if error.contains("observer closed"))); }
            task.await.unwrap();
        }
    }

    #[tokio::test(start_paused=true)]
    async fn presentation_timeout_and_script_abort_close_the_reply() {
        for cancel in [false,true] {
            let (tx,mut messages)=tokio::sync::mpsc::unbounded_channel();
            let task=tokio::spawn(execute("present 50".into(),tx));
            let Some(UiMsg::PresentBarrier(reply))=messages.recv().await else {panic!("missing presentation barrier")};
            if cancel {
                task.abort();assert!(task.await.unwrap_err().is_cancelled());
                assert!(messages.recv().await.is_none());
            } else {
                let Some(UiMsg::ScriptFailed(error))=messages.recv().await else {panic!("missing presentation timeout")};
                assert_eq!(error,"normal presentation did not complete");
                task.await.unwrap();
            }
            assert!(reply.is_closed());
        }
    }
}
