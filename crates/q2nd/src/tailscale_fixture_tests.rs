//! Source-identical generated lifecycle probe for matched old/new target builds.
use super::*;
use std::sync::atomic::{AtomicBool, Ordering};
use std::os::unix::fs::PermissionsExt;

pub(super) fn until(mut predicate: impl FnMut() -> bool) {
    let deadline=Instant::now()+Duration::from_secs(4);
    while !predicate() {
        assert!(Instant::now()<deadline,"owned lifecycle gate timed out");
        std::thread::sleep(Duration::from_millis(1));
    }
}
pub(super) struct Fixture {
    pub(super) root: PathBuf,
    stop: Arc<AtomicBool>,
    server: Option<std::thread::JoinHandle<()>>,
}
impl Fixture {
    pub(super) fn new() -> Self {
        let root=tests::fixture();
        let listener=std::os::unix::net::UnixListener::bind(root.join("s")).unwrap();
        listener.set_nonblocking(true).unwrap();
        let stop=Arc::new(AtomicBool::new(false));let stopping=stop.clone();
        let server=std::thread::spawn(move || {
            while !stopping.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((mut stream,_)) => {
                        stream.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
                        stream.set_write_timeout(Some(Duration::from_secs(1))).unwrap();
                        let mut request=[0;512];let _=stream.read(&mut request);
                        let _=stream.write_all(b"HTTP/1.0 200 OK\r\n\r\n{\"BackendState\":\"Starting\"}");
                    }
                    Err(error) if error.kind()==io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(1)),
                    Err(error)=>panic!("generated LocalAPI accept: {error}"),
                }
            }
        });
        let daemon=r#"#!/bin/sh
set -eu
d=$(dirname "$0")
if ! mkdir "$d/live";then : > "$d/overlap";exit 7;fi
printf '%s\n' "$$" >> "$d/daemon-pids"
printf '%s\n' "$$" > "$d/current.next"
mv "$d/current.next" "$d/current"
finish() {
    : > "$d/terminating"
    while test ! -f "$d/release";do sleep .01;done
    rmdir "$d/live"
    exit 0
}
trap finish TERM
while :;do sleep .01;done
"#;
        let cli=r#"#!/bin/sh
set -eu
d=$(dirname "$0")
printf '%s\n' "$$" >> "$d/cli-pids"
exec sleep 30
"#;
        for (name,text) in [("tailscaled",daemon),("tailscale",cli)] {
            let path=root.join(name);std::fs::write(&path,text).unwrap();
            std::fs::set_permissions(path,std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        Self {root,stop,server:Some(server)}
    }
    pub(super) fn owner(&self) -> Tailscale {
        Tailscale::at_socket(self.root.clone(),self.root.join("state"),false,true,self.root.join("s"))
    }
    pub(super) fn pids(&self,name:&str)->Vec<i32> {
        std::fs::read_to_string(self.root.join(name)).unwrap_or_default().lines().map(|line|line.parse().unwrap()).collect()
    }
    pub(super) fn release(&self) {std::fs::write(self.root.join("release"),b"").unwrap();}
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.release();self.stop.store(true,Ordering::Release);
        if let Some(server)=self.server.take(){server.join().unwrap();}
        std::fs::remove_dir_all(&self.root).unwrap();
    }
}

fn identity(pid:u32)->u64 {
    let stat=std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    stat.rsplit_once(") ").unwrap().1.split_whitespace().nth(19).unwrap().parse().unwrap()
}
fn census(phase:&str, children:&[i32]) {
    let begin=std::fs::read_dir("/proc/self/task").unwrap().count();
    let mut roles=std::collections::BTreeMap::<String,usize>::new();
    for task in std::fs::read_dir("/proc/self/task").unwrap() {
        let comm=std::fs::read_to_string(task.unwrap().path().join("comm")).unwrap();
        *roles.entry(comm.trim().to_owned()).or_default()+=1;
    }
    let end=std::fs::read_dir("/proc/self/task").unwrap().count();
    assert_eq!(begin,end);assert_eq!(end,roles.values().sum::<usize>());
    let status=std::fs::read_to_string("/proc/self/status").unwrap();
    let numeric=|key:&str|status.lines().find_map(|line|line.strip_prefix(key))
        .unwrap().split_whitespace().next().unwrap().parse::<u64>().unwrap();
    let children:Vec<_>=children.iter().map(|&pid|serde_json::json!({"pid":pid,"start_ticks":identity(pid as u32)})).collect();
    println!("TAILSCALE_CENSUS {}",serde_json::json!({"phase":phase,"pid":std::process::id(),
        "start_ticks":identity(std::process::id()),"t_us":crate::playtrace::boottime_us(),
        "roles":roles,"rss_kib":numeric("VmRSS:"),"hwm_kib":numeric("VmHWM:"),"children":children}));
}
fn joined(pids:impl IntoIterator<Item=i32>) {
    for pid in pids {
        assert_eq!(crate::platform::process::generated::wait_nonblocking(pid).unwrap_err().raw_os_error(),Some(libc::ECHILD));
    }
}

// This diagnostic distinguishes pthread completion from procfs disappearance.
// It does not classify a visible task as dead merely because stop() returned.
#[derive(Clone, Debug, serde::Serialize)]
pub(super) struct TaskSample {
    tid: u32,
    start_ticks: Option<u64>,
    state: Option<String>,
    error: Option<i32>,
    t_us: u64,
}
fn task_sample(tid: u32) -> TaskSample {
    let result=std::fs::read_to_string(format!("/proc/self/task/{tid}/stat"));
    let mut sample=TaskSample {tid,start_ticks:None,state:None,error:None,t_us:crate::playtrace::boottime_us()};
    match result {
        Ok(stat)=>{
            let fields:Vec<_>=stat.rsplit_once(") ").expect("owned task stat delimiter").1.split_whitespace().collect();
            sample.state=Some(fields[0].to_owned());
            sample.start_ticks=Some(fields[19].parse().expect("owned task start"));
        }
        Err(error)=>sample.error=Some(error.raw_os_error().unwrap_or(-1)),
    }
    sample
}
pub(super) fn owned_task_sample(tid: u32) -> TaskSample {
    let sample=task_sample(tid);
    assert!(sample.start_ticks.is_some() && sample.error.is_none(),"capture live owned task before retirement");
    sample
}
fn absent(sample:&TaskSample)->bool {
    sample.start_ticks.is_none() && matches!(sample.error,Some(libc::ENOENT)|Some(libc::ESRCH))
}
fn captured_owner()->TaskSample {
    let mut owners=Vec::new();
    for task in std::fs::read_dir("/proc/self/task").unwrap() {
        let task=task.unwrap();
        if std::fs::read_to_string(task.path().join("comm")).unwrap().trim()=="tailscale" {
            owners.push(task_sample(task.file_name().to_str().unwrap().parse().unwrap()));
        }
    }
    assert_eq!(owners.len(),1,"one re-enabled owner required");
    let owner=owners.pop().unwrap();assert!(owner.start_ticks.is_some() && owner.error.is_none());owner
}
#[derive(Debug,serde::Serialize)]
pub(super) struct Disappearance {
    owner:TaskSample,
    initial:TaskSample,
    final_sample:TaskSample,
    samples:u32,
    pub(super) passed:bool,
}
pub(super) fn disappearance(owner:TaskSample, timeout:Duration)->Disappearance {
    let started=Instant::now();let initial=task_sample(owner.tid);let mut current=initial.clone();let mut samples=1;
    loop {
        if absent(&current) {let passed=started.elapsed()<=timeout;return Disappearance {owner,initial,final_sample:current,samples,passed};}
        // An unexpected read error or different incarnation fails explicitly.
        if current.error.is_some() || current.start_ticks!=owner.start_ticks || started.elapsed()>=timeout {
            return Disappearance {owner,initial,final_sample:current,samples,passed:false};
        }
        std::thread::sleep(Duration::from_millis(1));current=task_sample(owner.tid);samples+=1;
    }
}
#[test]
fn disappearance_refuses_a_stuck_live_thread() {
    let (send,receive)=std::sync::mpsc::channel();let (release,gate)=std::sync::mpsc::channel();
    let thread=std::thread::Builder::new().name("owner-probe".into()).spawn(move||{
        send.send(crate::platform::process::generated::current_thread_id()).unwrap();gate.recv().unwrap();
    }).unwrap();
    let tid=receive.recv_timeout(Duration::from_secs(1)).unwrap();let owner=owned_task_sample(tid);
    let observation=disappearance(owner.clone(),Duration::from_millis(20));
    release.send(()).unwrap();thread.join().unwrap();
    assert!(!observation.passed,"live owner must not be accepted");
    assert_eq!(observation.initial.start_ticks,owner.start_ticks);
    assert_eq!(observation.final_sample.start_ticks,owner.start_ticks);
    assert!(observation.initial.error.is_none() && observation.final_sample.error.is_none());
    let final_observation=disappearance(owner,Duration::from_secs(1));assert!(final_observation.passed);
}

#[test]
fn generated_owner_lifecycle_census() {
    if std::env::var_os("Q2_TAILSCALE_CENSUS").as_deref()!=Some(std::ffi::OsStr::new("1")) {
        // The workspace suite runs unrelated owners concurrently. Measure one
        // fresh owned process so their exits cannot race its /proc census.
        // Target runners set this flag on their already isolated invocation.
        let child=std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact","tailscale::fixture_tests::generated_owner_lifecycle_census","--test-threads","1","--nocapture"])
            .env("Q2_TAILSCALE_CENSUS","1").output().unwrap();
        assert!(child.status.success(),"generated census child failed: {} {}",String::from_utf8_lossy(&child.stdout),String::from_utf8_lossy(&child.stderr));
        assert!(String::from_utf8_lossy(&child.stdout).contains("1 passed; 0 failed; 0 ignored"));
        return;
    }
    let f=Fixture::new();
    let mut t=Tailscale::at_socket(f.root.clone(),f.root.join("state"),false,false,f.root.join("s"));
    census("never-enabled",&[]);
    let first=Instant::now();t.set_enabled(true);until(||f.pids("cli-pids").len()==1);
    let first_us=first.elapsed().as_micros();
    let first_children=[f.pids("daemon-pids")[0],f.pids("cli-pids")[0]];
    let first_starts=first_children.map(|pid|identity(pid as u32));
    // The enabled worker must remain the daemon's live parent through idle.
    std::thread::sleep(Duration::from_millis(150));
    for (&pid,&start) in first_children.iter().zip(&first_starts){assert_eq!(identity(pid as u32),start);}
    census("active",&first_children);
    f.release();t.set_enabled(false);until(||t.state().status=="Off");
    joined(first_children);assert!(!f.root.join("live").exists());
    // A fixed post-join observation in BOTH variants; baseline is permitted to
    // retain its parked owner. The external matched analyser checks the change.
    std::thread::sleep(Duration::from_millis(200));census("disabled",&[]);
    let again=Instant::now();t.set_enabled(true);until(||f.pids("cli-pids").len()==2);
    let again_us=again.elapsed().as_micros();
    let next_children=[f.pids("daemon-pids")[1],f.pids("cli-pids")[1]];
    assert_ne!(first_children[0],next_children[0]);assert!(!f.root.join("overlap").exists());
    census("reenabled",&next_children);
    let owner=t.worker.inner.lock().unwrap().thread.as_ref().unwrap().id();
    t.stop();
    // Capture immediately after the actual Rust owner join, before child checks.
    assert!(t.worker.inner.lock().unwrap().thread.is_none());
    println!("TAILSCALE_TASK_JOIN owner={owner:?} passed=1");
    joined(next_children);assert!(!f.root.join("live").exists());census("stopped",&[]);
    println!("TAILSCALE_LIFECYCLE {}",serde_json::json!({"passed":true,"first_cli_us":first_us,
        "reenable_cli_us":again_us,"joined_children":4,"overlap":false}));
}
