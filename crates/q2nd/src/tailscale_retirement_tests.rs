//! Real owner and generated child lifetimes. No daemon/network account data.
use super::*;
use std::sync::mpsc;
use super::fixture_tests::{Fixture,until};

fn retired(t: &Tailscale) -> bool {
    let owner=t.worker.inner.lock().unwrap();
    !owner.running && owner.thread.as_ref().is_some_and(|thread|thread.is_finished())
}
fn reaped(pid: i32) {
    assert_eq!(crate::platform::process::generated::wait_nonblocking(pid).unwrap_err().raw_os_error(),Some(libc::ECHILD));
}

#[test]
fn real_children_join_before_retirement_and_reenable_cannot_overlap_them() {
    let f=Fixture::new();let mut t=f.owner();
    until(||f.pids("cli-pids").len()==1);
    let first_owner=t.worker.inner.lock().unwrap().thread.as_ref().unwrap().id();
    let first_daemon=f.pids("daemon-pids")[0];let first_cli=f.pids("cli-pids")[0];
    t.set_enabled(false);
    until(||f.root.join("terminating").exists());
    reaped(first_cli);
    assert!(crate::platform::process::generated::exists(first_daemon));
    assert!(t.worker.inner.lock().unwrap().running);
    assert_ne!(t.state().status,"Off","Off must follow child join");
    // Both commands arrive while actual daemon teardown is stalled. They stay
    // with this owner; login never starts a parallel child or an implicit owner.
    t.login();t.set_enabled(true);
    assert_eq!(f.pids("daemon-pids").len(),1);
    f.release();until(||f.pids("cli-pids").len()==2);
    reaped(first_daemon);
    assert_eq!(t.worker.inner.lock().unwrap().thread.as_ref().unwrap().id(),first_owner);
    assert!(!f.root.join("overlap").exists());
    t.set_enabled(false);until(||retired(&t));
    assert_eq!(t.state().status,"Off");assert!(!f.root.join("live").exists());
    for pid in f.pids("daemon-pids").into_iter().chain(f.pids("cli-pids")){reaped(pid);}
    let start=Instant::now();t.set_enabled(true);
    until(||f.pids("cli-pids").len()==3);
    println!("TAILSCALE_RETIRE reenable_cli_us={} previous_children_joined=1",start.elapsed().as_micros());
    assert_ne!(t.worker.inner.lock().unwrap().thread.as_ref().unwrap().id(),first_owner);
    assert!(!f.root.join("overlap").exists());
    t.stop();t.set_enabled(true);t.login();
    assert!(t.worker.inner.lock().unwrap().thread.is_none());
    assert_eq!(f.pids("daemon-pids").len(),3);
    for pid in f.pids("daemon-pids").into_iter().chain(f.pids("cli-pids")){reaped(pid);}
}

#[test]
fn stop_during_disable_joins_both_children_and_discards_queued_reenable() {
    let f=Fixture::new();let mut t=f.owner();until(||f.pids("cli-pids").len()==1);
    t.set_enabled(false);until(||f.root.join("terminating").exists());
    t.set_enabled(true);t.login();let control=t.worker.clone();
    let (done,finished)=mpsc::channel();
    let stopping=std::thread::spawn(move||{t.stop();assert!(done.send(t).is_ok());});
    until(||control.inner.lock().unwrap().start.is_none());
    assert!(finished.try_recv().is_err());
    f.release();let mut t=finished.recv_timeout(crate::library::TEST_LIVENESS).unwrap();stopping.join().unwrap();
    t.set_enabled(true);t.stop();assert!(t.worker.inner.lock().unwrap().thread.is_none());
    assert_eq!(f.pids("daemon-pids").len(),1);assert!(!f.root.join("live").exists());
    for pid in f.pids("daemon-pids").into_iter().chain(f.pids("cli-pids")){reaped(pid);}
}

#[test]
fn successor_joins_retired_completion_without_blocking_the_request_caller() {
    let root=tests::fixture();let mut t=Tailscale::at_socket(root.clone(),root.join("state"),true,false,root.join("s"));
    let (release,gate)=mpsc::channel();let (entered,waiting)=mpsc::channel();
    let predecessor=crate::rt::io().spawn(async move {entered.send(()).unwrap();crate::rt::spawn_blocking(move |_|gate.recv().unwrap()).await.unwrap();});
    waiting.recv_timeout(crate::library::TEST_LIVENESS).unwrap();
    // Model a retired owner's final epilogue. Its business work is complete,
    // but the successor must retain/join the actual thread completion token.
    t.worker.inner.lock().unwrap().thread=Some(predecessor);
    t.set_enabled(true);t.login();t.set_enabled(false);t.set_enabled(true);
    assert_eq!(t.state().status,"Off");assert!(t.worker.inner.lock().unwrap().running);
    release.send(()).unwrap();until(||t.state().status=="Available on Q2");
    t.stop();std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn repeated_retirement_preserves_single_completion_owner_and_prompt_restart() {
    let root=tests::fixture();let mut t=Tailscale::at_socket(root.clone(),root.join("state"),true,false,root.join("s"));
    for _ in 0..32 {
        t.set_enabled(true);until(||t.state().status=="Available on Q2");
        t.set_enabled(false);until(||retired(&t));
        let owner=t.worker.inner.lock().unwrap();assert!(owner.requests.is_empty());assert!(owner.thread.is_some());
        drop(owner);t.login();assert!(!t.worker.inner.lock().unwrap().running);
    }
    t.stop();std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn failed_helper_spawn_and_early_exit_can_retire_then_restart() {
    for failure in ["missing","early-exit"] {
        let f=Fixture::new();let path=f.root.join("tailscaled");
        let original=std::fs::read(&path).unwrap();
        if failure=="missing" {std::fs::remove_file(&path).unwrap();}
        else {std::fs::write(&path,b"#!/bin/sh\nexit 7\n").unwrap();}
        let mut t=f.owner();until(||t.state().status=="Could Not Start");
        t.set_enabled(false);until(||retired(&t));assert_eq!(t.state().status,"Off");
        std::fs::write(&path,&original).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path,std::fs::Permissions::from_mode(0o755)).unwrap();
        t.set_enabled(true);until(||f.pids("cli-pids").len()==1);
        assert!(!f.root.join("overlap").exists());
        f.release();t.stop();
        for pid in f.pids("daemon-pids").into_iter().chain(f.pids("cli-pids")){reaped(pid);}
    }
}
