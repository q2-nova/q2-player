//! Fixed numeric, RAM-only startup actor identity/counters. No proc text escapes.
//!
//! `/proc/PID/stat` is thread-group CPU/fault accounting; `task/PID/stat` is
//! main-thread accounting. Group `/proc/PID/io` also includes waited-child I/O
//! on the qualified Linux kernel. Logical and storage-accounted bytes are not
//! physical NAND traffic. Initial cumulative counters include work before the
//! first snapshot; they do not reconstruct an earlier fine-grained timeline.
#![forbid(unsafe_code)]
use crate::platform::system;
use std::{fs::File, io::{self, Read, Write}, os::unix::fs::{MetadataExt, OpenOptionsExt}, path::Path, sync::atomic::{AtomicUsize, Ordering}};

const STAT_BYTES: u64 = 4096;
const IO_BYTES: u64 = 1024;
const OUTPUT_BYTES: u64 = 16 * 1024;
const RECORDS: usize = 8;
static ATTEMPTS: AtomicUsize = AtomicUsize::new(0);
pub(crate) const FIELDS: [&str;29] = ["schema","begin_us","end_us","actor_kind","app_pid","app_start_ticks","app_exe_dev","app_exe_ino",
    "pid","start_ticks","exe_dev","exe_ino","group_user_ticks","group_system_ticks","group_minor_faults","group_major_faults",
    "main_user_ticks","main_system_ticks","main_minor_faults","main_major_faults","main_available","io_available",
    "rchar","wchar","syscr","syscw","read_bytes","write_bytes","cancelled_write_bytes"];

#[derive(Clone, Copy)]
pub(crate) enum Actor { Daemon = 1, Up = 2 }
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Stat { parent: u32, start: u64, user: u64, system: u64, minor: u64, major: u64 }
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Identity { pid: u32, start: u64, dev: u64, ino: u64 }
#[derive(Clone, Copy, Debug, Default)]
struct Snapshot { app: Identity, actor: Identity, group: Stat, main: Stat, io: [u64;7], main_status: u8, io_status: u8 }

fn invalid() -> io::Error { io::ErrorKind::InvalidData.into() }
fn number(text: &str) -> io::Result<u64> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) { return Err(invalid()); }
    text.parse().map_err(|_| invalid())
}
fn stat(text: &str, pid: u32) -> io::Result<Stat> {
    if text.len()>STAT_BYTES as usize || !text.ends_with('\n') { return Err(invalid()); }
    let (head, tail)=text.rsplit_once(") ").ok_or_else(invalid)?;
    let (found,_comm)=head.split_once(" (").ok_or_else(invalid)?;
    if number(found)?!=u64::from(pid) { return Err(invalid()); }
    let fields: Vec<_>=tail.split_ascii_whitespace().collect();
    if fields.len()<20 { return Err(invalid()); }
    let parent=u32::try_from(number(fields[1])?).map_err(|_| invalid())?;
    let out=Stat {parent,start:number(fields[19])?,user:number(fields[11])?,system:number(fields[12])?,minor:number(fields[7])?,major:number(fields[9])?};
    if out.start==0 { return Err(invalid()); }
    Ok(out)
}
fn io_fields(text: &str) -> io::Result<[u64;7]> {
    const KEYS: [&str;7]=["rchar","wchar","syscr","syscw","read_bytes","write_bytes","cancelled_write_bytes"];
    if text.len()>IO_BYTES as usize || !text.ends_with('\n') { return Err(invalid()); }
    let mut values=[None;7];
    for line in text.lines() {
        let (key,value)=line.split_once(':').ok_or_else(invalid)?;
        let index=KEYS.iter().position(|k| *k==key).ok_or_else(invalid)?;
        if values[index].is_some() { return Err(invalid()); }
        values[index]=Some(number(value.trim())?);
    }
    if values.iter().any(Option::is_none) { return Err(invalid()); }
    Ok(values.map(Option::unwrap))
}
struct Proc { directory: File, pid: u32 }
impl Proc {
    fn open(_blocking: &crate::rt::Blocking, pid: u32) -> io::Result<Self> {
        let directory=std::fs::OpenOptions::new().read(true)
            .custom_flags(libc::O_DIRECTORY|libc::O_NOFOLLOW|libc::O_CLOEXEC)
            .open(format!("/proc/{pid}"))?;
        Ok(Self {directory,pid})
    }
    fn read(&self,blocking: &crate::rt::Blocking,name:&str,limit:u64) -> io::Result<String> {
        let name=std::ffi::CString::new(name).map_err(|_| invalid())?;
        let mut file=system::open_at(blocking, &self.directory,&name,
            libc::O_RDONLY|libc::O_NOFOLLOW|libc::O_NONBLOCK|libc::O_CLOEXEC,0)?;
        if !file.metadata()?.is_file() { return Err(invalid()); }
        let mut bytes=vec![0;limit as usize+1];let n=file.read(&mut bytes)?;
        if n==0 || n as u64>limit { return Err(invalid()); }
        bytes.truncate(n);
        String::from_utf8(bytes).map_err(|_| invalid())
    }
    fn stat(&self,blocking: &crate::rt::Blocking) -> io::Result<Stat> { stat(&self.read(blocking, "stat",STAT_BYTES)?,self.pid) }
    fn executable(&self,blocking: &crate::rt::Blocking) -> io::Result<(u64,u64)> {
        // Intentional kernel proc-exe link, relative to the retained proc dir.
        let file=system::open_at(blocking, &self.directory,c"exe",libc::O_PATH|libc::O_CLOEXEC,0)?;
        let meta=file.metadata()?;
        if !meta.is_file() { return Err(invalid()); }
        Ok((meta.dev(),meta.ino()))
    }
}
fn stable(before: Stat, after: Stat, first: (u64,u64), last: (u64,u64)) -> bool {
    stability_refusals(before,after,first,last)==0
}
fn stability_refusals(before: Stat, after: Stat, first: (u64,u64), last: (u64,u64)) -> u16 {
    u16::from(before.start!=after.start)
        | (u16::from(before.parent!=after.parent)<<1)
        | (u16::from(first!=last)<<2)
        | (u16::from(after.user<before.user)<<3)
        | (u16::from(after.system<before.system)<<4)
        | (u16::from(after.minor<before.minor)<<5)
        | (u16::from(after.major<before.major)<<6)
}
fn snapshot_pid(blocking: &crate::rt::Blocking,pid: u32, expected: &Path) -> io::Result<Snapshot> {
    snapshot_pid_observed(blocking, pid,expected,|_,_|{})
}
fn snapshot_pid_observed(blocking: &crate::rt::Blocking,pid: u32, expected: &Path, mut observe: impl FnMut(u8,u16)) -> io::Result<Snapshot> {
    // Fixed stage/predicate numbers let generated tests attribute a refusal
    // without retaining proc text, executable paths or command arguments.
    // Production supplies a no-op observer; every refusal remains unchanged.
    macro_rules! step { ($stage:expr,$value:expr) => {{observe($stage,0);$value?}}; }
    // The caller still owns the unreaped Child, preventing PID reuse during
    // this snapshot. Open proc dirs and start/executable checks guard both ends.
    let app=step!(1,Proc::open(blocking, std::process::id()));let app_stat=step!(2,app.stat(blocking));let app_exe=step!(3,app.executable(blocking));
    let actor=step!(4,Proc::open(blocking, pid));let before=step!(5,actor.stat(blocking));let first=step!(6,actor.executable(blocking));
    let wanted=step!(7,expected.metadata());
    let refused=u16::from(!wanted.is_file()) | (u16::from(first!=(wanted.dev(),wanted.ino()))<<1)
        | (u16::from(before.parent!=std::process::id())<<2);
    observe(8,refused);
    if refused!=0 { return Err(invalid()); }
    let main=actor.read(blocking, &format!("task/{}/stat",pid),STAT_BYTES).and_then(|s|stat(&s,pid));
    let counters=actor.read(blocking, "io",IO_BYTES).and_then(|s|io_fields(&s));
    let after=step!(9,actor.stat(blocking));let last=step!(10,actor.executable(blocking));
    observe(11,stability_refusals(before,after,first,last));
    if !stable(before,after,first,last) { return Err(invalid()); }
    let app_after=step!(12,app.stat(blocking));observe(13,u16::from(app_after.start!=app_stat.start));
    if app_after.start!=app_stat.start { return Err(invalid()); }
    let app_last=step!(14,app.executable(blocking));observe(15,u16::from(app_last!=app_exe));
    if app_last!=app_exe { return Err(invalid()); }
    let (main,main_status)=match main {Ok(s) if s.start==after.start=>(s,1),_=>(Stat::default(),0)};
    let (io,io_status)=match counters {Ok(s)=>(s,1),_=>([0;7],0)};
    Ok(Snapshot {app:Identity{pid:std::process::id(),start:app_stat.start,dev:app_exe.0,ino:app_exe.1},
        actor:Identity{pid:pid,start:after.start,dev:last.0,ino:last.1},group:after,main,io,main_status,io_status})
}
fn stamp() -> io::Result<u64> { system::boottime_us_checked() }

fn app_identity(blocking: &crate::rt::Blocking) -> io::Result<Identity> {
    let app=Proc::open(blocking, std::process::id())?;let before=app.stat(blocking)?;let first=app.executable(blocking)?;
    if app.stat(blocking)?.start!=before.start || app.executable(blocking)?!=first {return Err(invalid());}
    Ok(Identity{pid:std::process::id(),start:before.start,dev:first.0,ino:first.1})
}
fn cap_record(now:u64,app:Identity) -> String {
    format!("2 {now} {} {} {} {} {RECORDS}\n",app.pid,app.start,app.dev,app.ino)
}
fn encode(kind: Actor,begin:u64,end:u64,s:&Snapshot) -> String {
    let mut fields=vec![1,begin,end,kind as u64,u64::from(s.app.pid),s.app.start,s.app.dev,s.app.ino,
        u64::from(s.actor.pid),s.actor.start,s.actor.dev,s.actor.ino,s.group.user,s.group.system,s.group.minor,s.group.major,
        s.main.user,s.main.system,s.main.minor,s.main.major,u64::from(s.main_status),u64::from(s.io_status)];
    fields.extend(s.io);
    debug_assert_eq!(fields.len(),FIELDS.len());
    fields.iter().map(u64::to_string).collect::<Vec<_>>().join(" ")+"\n"
}
fn append(_blocking: &crate::rt::Blocking,runtime:&Path,line:&str) -> io::Result<()> {
    let count=line.split_ascii_whitespace().count();
    if line.len()>1024 || !line.ends_with('\n') || line.bytes().filter(|b|*b==b'\n').count()!=1
        || !line.bytes().all(|b|b.is_ascii_digit()||b==b' '||b==b'\n')
        || !((line.starts_with("1 ")&&count==FIELDS.len())||(line.starts_with("2 ")&&count==7)) {return Err(invalid());}
    let mut file=std::fs::OpenOptions::new().create(true).append(true).mode(0o600)
        .custom_flags(libc::O_NOFOLLOW|libc::O_NONBLOCK).open(runtime.join("process-starts"))?;
    if !file.metadata().is_ok_and(|m|m.is_file() && m.len().saturating_add(line.len() as u64)<=OUTPUT_BYTES) {return Err(invalid());}
    if file.write(line.as_bytes())?!=line.len() {return Err(io::ErrorKind::WriteZero.into());}
    Ok(())
}
/// Best-effort one startup snapshot, bounded in records/bytes. Kernel proc
/// reads are not a hard real-time guarantee; matched overhead must be measured.
/// No retry, fsync, raw proc text, response body, argv, environment or path output.
pub(crate) fn record_pid(blocking: &crate::rt::Blocking,kind: Actor,pid: u32,expected: &Path) {
    let Some(runtime)=std::env::var_os("Q2ND_RUNTIME") else {return};
    let attempt=ATTEMPTS.fetch_add(1,Ordering::Relaxed);
    if attempt>RECORDS {return;}
    let line=if attempt==RECORDS {
        let Ok(app)=app_identity(blocking) else {return};let Ok(now)=stamp() else {return};cap_record(now,app)
    } else {
        let Ok(begin)=stamp() else {return};let Ok(snapshot)=snapshot_pid(blocking,pid,expected) else {return};let Ok(end)=stamp() else {return};
        encode(kind,begin,end,&snapshot)
    };
    let _=append(blocking,Path::new(&runtime),&line);
}

#[cfg(test)] mod tests {
    use super::*;
    fn generated_stat(pid:u32,parent:u32,start:u64) -> String {
        let mut f=vec!["0".to_string();20];f[0]="S".into();f[1]=parent.to_string();f[7]="4".into();f[9]="5".into();f[11]="12".into();f[12]="13".into();f[19]=start.to_string();
        format!("{pid} (GENERATED_PRIVATE_SENTINEL ) nested) {}\n",f.join(" "))
    }
    #[test] fn stat_selects_only_numeric_fields_and_never_keeps_comm() {
        let text=generated_stat(41,7,99);let s=stat(&text,41).unwrap();
        assert_eq!(s,Stat{parent:7,start:99,user:12,system:13,minor:4,major:5});assert!(!format!("{s:?}").contains("SENTINEL"));
        for invalid in [text.replace("41 (","42 ("),text.trim_end().into(),text.replace(" 12 "," -1 "),text.replace(" 99\n"," 0\n"),"x".repeat(4097)] {assert!(stat(&invalid,41).is_err());}
    }
    #[test] fn io_requires_exact_fixed_keys_complete_numeric_values() {
        let good="rchar: 1\nwchar: 2\nsyscr: 3\nsyscw: 4\nread_bytes: 5\nwrite_bytes: 6\ncancelled_write_bytes: 7\n";
        assert_eq!(io_fields(good).unwrap(),[1,2,3,4,5,6,7]);
        for bad in [good.replace("rchar:","GENERATED_PRIVATE_SENTINEL:"),good.to_owned()+"rchar: 9\n",good.replace("rchar: 1\n",""),good.replace("rchar: 1","rchar: -1"),good.trim_end().into(),"x".repeat(1025)] {assert!(io_fields(&bad).is_err());}
    }
    #[test] fn changed_start_parent_executable_or_reversed_counters_refuse() {
        let s=stat(&generated_stat(41,7,99),41).unwrap();assert!(stable(s,s,(1,2),(1,2)));
        assert_eq!(stability_refusals(s,s,(1,2),(1,2)),0);
        for (after,bit) in [(Stat{start:100,..s},0),(Stat{parent:8,..s},1),(Stat{user:11,..s},3),(Stat{system:12,..s},4),(Stat{minor:3,..s},5),(Stat{major:4,..s},6)] {
            assert!(!stable(s,after,(1,2),(1,2)));
            assert_eq!(stability_refusals(s,after,(1,2),(1,2)),1<<bit);
        }
        assert!(!stable(s,s,(1,2),(1,3)));
        assert_eq!(stability_refusals(s,s,(1,2),(1,3)),1<<2);
        assert_eq!(stability_refusals(s,Stat{start:100,parent:8,user:11,system:12,minor:3,major:4},(1,2),(1,3)),127);
    }
    #[test] fn owned_child_snapshot_has_real_identity_and_separate_main_scope() {
        let exe=Path::new("/bin/sleep");let mut child=std::process::Command::new(exe).arg("30").spawn().unwrap();
        let mut attribution=(0,0);
        let sample=snapshot_pid_observed(&crate::rt::Blocking::dedicated_thread(), child.id(),exe,|stage,refused|attribution=(stage,refused));
        // Attempt both operations before asserting either result, including
        // when the snapshot refuses. Only this generated child's facts escape.
        let killed=child.kill();let waited=child.wait();
        let error=sample.as_ref().err();
        println!("Q2_PROCESS_PROFILE stage={} refused={} invalid_data={} errno={} killed={} reaped={}",
            attribution.0,attribution.1,u8::from(error.is_some_and(|e|e.kind()==io::ErrorKind::InvalidData)),
            error.and_then(io::Error::raw_os_error).unwrap_or(0),u8::from(killed.is_ok()),u8::from(waited.is_ok()));
        killed.unwrap();waited.unwrap();let s=sample.unwrap();
        assert_eq!(s.actor.pid,child.id());assert_eq!(s.app.pid,std::process::id());assert_eq!(s.main_status,1);assert_eq!(s.main.start,s.group.start);
        let row=encode(Actor::Daemon,1,2,&s);assert_eq!(row.split_whitespace().count(),29);assert!(row.bytes().all(|b|b.is_ascii_digit()||b==b' '||b==b'\n'));assert!(!row.contains("sleep"));
    }
    #[test] fn ram_writer_refuses_private_text_symlinks_and_oversize() {
        let root=crate::library::generated_test_directory("process-profile");
        std::fs::create_dir_all(&root).unwrap();let path=root.join("process-starts");
        assert!(append(&crate::rt::Blocking::dedicated_thread(), &root,"GENERATED_PRIVATE_SENTINEL\n").is_err());assert!(!path.exists());
        let row=encode(Actor::Up,1,2,&Snapshot::default());append(&crate::rt::Blocking::dedicated_thread(), &root,&row).unwrap();assert_eq!(std::fs::read_to_string(&path).unwrap(),row);
        std::fs::OpenOptions::new().write(true).open(&path).unwrap().set_len(OUTPUT_BYTES).unwrap();assert!(append(&crate::rt::Blocking::dedicated_thread(), &root,&row).is_err());
        std::fs::remove_file(&path).unwrap();let sentinel=root.join("owned-sentinel");std::fs::write(&sentinel,"unchanged").unwrap();
        std::os::unix::fs::symlink(&sentinel,&path).unwrap();assert!(append(&crate::rt::Blocking::dedicated_thread(), &root,&row).is_err());assert_eq!(std::fs::read_to_string(&sentinel).unwrap(),"unchanged");
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test] fn cap_is_bound_to_full_app_generation_without_truncation() {
        let app=app_identity(&crate::rt::Blocking::dedicated_thread()).unwrap();let text=cap_record(100,app);let fields:Vec<_>=text.split_ascii_whitespace().collect();
        assert_eq!(fields.len(),7);assert_eq!(fields[0],"2");assert_eq!(fields[2],app.pid.to_string());
        assert_eq!(fields[3],app.start.to_string());assert_eq!(fields[4],app.dev.to_string());assert_eq!(fields[5],app.ino.to_string());assert_eq!(fields[6],"8");
        assert_ne!(text,cap_record(100,Identity{start:app.start+1,..app}));
    }
}
