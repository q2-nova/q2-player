//! Explicit generated-script playback observations; no normal-path I/O.
#![forbid(unsafe_code)]
use crate::platform::system;
use crate::audio::player::{PlayState,Status};
use serde::Serialize;
use std::io::{Read,Write};
use std::os::unix::fs::{MetadataExt,OpenOptionsExt};
use std::path::{Path,PathBuf};
use std::ffi::CString;

pub const MARKER:&[u8]=b"Q2 GENERATED NUMERIC PLAYBACK PROBE 1\n";

/// Numeric metadata only: never serialize titles, URLs, error text or raw IDs.
#[derive(Clone,Debug,Serialize,PartialEq,Eq)]
pub struct Playback {
    schema:u8,
    pid:u32,
    generation:u64,
    seek_revision:u64,
    queue_index:usize,
    selection_tag:Option<u32>,
    position_ms:u64,
    play_state:u8,
    output_revision:u64,
    output_usb_card:Option<u32>,
    output_usb_device:Option<u32>,
    output_owned:bool,
    pause_admission_failed:bool,
    output_pause_kind:u8,
    output_pause_frames:u64,
    error:bool,
}

impl Playback {
    /// Desired route metadata, not a claim that a hardware PCM has opened.
    pub fn route_is(&self,route:&str)->bool {
        match route {"usb"=>self.output_usb_card.is_some(),"headphones"=>self.output_usb_card.is_none(),_=>false}
    }
    /// Explicit generated-script completion wait. A different selection/seek
    /// or operational fault is refusal, never another song's completion ACK.
    pub fn finished_for(&self,first:&Self)->Result<bool,String> {
        if first.selection_tag.is_none() {return Err("completion wait requires a generated selection".into());}
        if (self.generation,self.seek_revision,self.queue_index,self.selection_tag)
            !=(first.generation,first.seek_revision,first.queue_index,first.selection_tag) {
            return Err("selection changed during completion wait".into());
        }
        if self.error || self.pause_admission_failed {return Err("playback failed during completion wait".into());}
        Ok(self.play_state==0)
    }

    /// `await-play`: the play state is `state` (0 stopped, 1 playing, 2
    /// paused, 3 buffering) and, if `advance_ms` > 0, the position advanced by
    /// that much while it stayed so on the same playback generation.
    pub(crate) fn reached(&self,state:u8,advance_ms:u64,base:&mut Option<(u64,u64)>)->bool {
        if self.play_state!=state {*base=None;return false;}
        match *base {
            Some((generation,from)) if generation==self.generation && self.position_ms>=from =>
                self.position_ms-from>=advance_ms,
            _ => {*base=Some((self.generation,self.position_ms));advance_ms==0}
        }
    }

    pub fn from_status(status:&Status)->Self {
        let tag=status.current().map(|song|song.id.bytes().fold(2166136261u32,
            |value,byte|(value^u32::from(byte)).wrapping_mul(16777619)));
        Self {schema:1,pid:std::process::id(),generation:status.generation,seek_revision:status.seek_revision,
            queue_index:status.index,selection_tag:tag,position_ms:status.position_ms,
            play_state:match status.state {PlayState::Stopped=>0,PlayState::Playing=>1,PlayState::Paused=>2,PlayState::Buffering=>3},
            output_revision:status.output_revision,output_usb_card:status.output_usb_card,output_usb_device:status.output_usb_device,
            output_owned:status.output_owned,pause_admission_failed:status.pause_admission_failed,
            output_pause_kind:status.output_pause_kind,output_pause_frames:status.output_pause_frames,
            error:status.error.is_some()}
    }
}

/// Fresh ordinary-file cursor, produced only by the audio owner. The clock
/// bracket surrounds the strict output-delay observation. No identifiers or
/// strings from the library are serialized; selection tags bind generated media.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct MusicCursor {
    pub(crate) schema: u8,
    pub(crate) pid: u32,
    pub(crate) begin_us: u64,
    pub(crate) end_us: u64,
    pub(crate) generation: u64,
    pub(crate) seek_revision: u64,
    pub(crate) queue_revision: u64,
    pub(crate) queue_len: usize,
    pub(crate) queue_index: usize,
    pub(crate) selection_tag: u32,
    pub(crate) position_ms: u64,
    pub(crate) base_ms: u64,
    pub(crate) written_frames: u64,
    pub(crate) queued_frames: u64,
    pub(crate) sample_rate: u32,
    pub(crate) output_revision: u64,
}

/// Automatic policy intent, not an acknowledgement that the radio is off.
/// The estimate is retained for comparison; it is not verified cached content.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct DozeBoundary {
    pub(crate) ordinal: u64,
    pub(crate) requested_us: u64,
    pub(crate) confirmed_us: u64,
    pub(crate) estimated_ahead_ms: u64,
}

pub struct Output {directory:Option<std::fs::File>,name:String}
impl Drop for Output {
    fn drop(&mut self) {
        // Authorization pins the directory across async observation waits.
        // Timeout, task cancellation and abandoned authorization replies must
        // retain that anchor until its close can run outside the executor.
        crate::rt::drop_off_executor(self.directory.take());
    }
}
impl Output {
    pub fn authorize(blocking: &crate::rt::Blocking,name:&str)->Result<Self,String> {
        // Refuse before reading any file unless explicitly enabled by a
        // generated launcher with installed storage disabled.
        let enabled=std::env::var_os("Q2ND_NUMERIC_PROBE").is_some_and(|v|v=="1");
        let installed=std::env::var_os("Q2ND_INSTALLED_STORAGE");
        let runtime=std::env::var_os("Q2ND_RUNTIME").map(PathBuf::from);
        Self::guarded(blocking, name,enabled,installed.as_deref(),runtime.as_deref())
    }

    fn guarded(blocking: &crate::rt::Blocking,name:&str,enabled:bool,installed:Option<&std::ffi::OsStr>,runtime:Option<&Path>)->Result<Self,String> {
        if !enabled || installed!=Some(std::ffi::OsStr::new("0")) {
            return Err("numeric probe requires an explicit generated launcher".into());
        }
        Self::at(blocking, name,runtime.ok_or("numeric probe runtime missing")?)
    }

    fn at(blocking: &crate::rt::Blocking,name:&str,runtime:&Path)->Result<Self,String> {
        if name.is_empty() || name.len()>32 || !name.bytes().all(|b|b.is_ascii_alphanumeric() || b==b'_' || b==b'-')
            || !runtime.is_absolute() {return Err("invalid generated numeric probe path".into());}
        let directory=std::fs::OpenOptions::new().read(true)
            .custom_flags(libc::O_DIRECTORY|libc::O_NOFOLLOW|libc::O_CLOEXEC).open(runtime)
            .map_err(|_|"numeric probe runtime unavailable")?;
        let metadata=directory.metadata().map_err(|_|"numeric probe runtime metadata unavailable")?;
        if !metadata.is_dir() || metadata.uid()!=system::effective_uid() || metadata.mode()&0o022!=0 {
            return Err("numeric probe runtime must be privately owned".into());
        }
        let marker=CString::new("numeric-probe-enabled").unwrap();
        let file=system::open_at(blocking, &directory,&marker,
            libc::O_RDONLY|libc::O_NONBLOCK|libc::O_NOFOLLOW|libc::O_CLOEXEC,0)
            .map_err(|_|"generated numeric probe marker unavailable")?;
        let metadata=file.metadata().map_err(|_|"numeric probe marker metadata unavailable")?;
        if !metadata.is_file() || metadata.uid()!=system::effective_uid() || metadata.mode()&0o777!=0o600
            || metadata.nlink()!=1 || metadata.len()!=MARKER.len() as u64 {
            return Err("numeric probe marker is not private and regular".into());
        }
        let mut bytes=Vec::new();
        std::io::Read::take(file,MARKER.len() as u64+1).read_to_end(&mut bytes).map_err(|_|"numeric probe marker read failed")?;
        if bytes!=MARKER {return Err("numeric probe marker differs".into());}
        Ok(Self {directory:Some(directory),name:name.into()})
    }

    pub fn publish(self,blocking: &crate::rt::Blocking,value:&Playback)->Result<(),String> { self.publish_numeric(blocking, value) }

    /// Fault journeys: the strings on screen and the playback state, from a
    /// generated launcher only (generated media; no owner library reaches it).
    pub fn publish_text(self,blocking: &crate::rt::Blocking,value:&serde_json::Value)->Result<(),String> { self.publish_numeric(blocking, value) }

    pub fn publish_cursor(self,blocking: &crate::rt::Blocking,value:&MusicCursor,requested_us:u64,received_us:u64)->Result<(),String> {
        if requested_us == 0 || requested_us > value.begin_us || value.begin_us > value.end_us
            || value.end_us > received_us || received_us - requested_us > 2_000_000 {
            return Err("music cursor request/completion clock refused".into());
        }
        #[derive(Serialize)]
        struct Receipt<'a> {schema:u8,requested_us:u64,received_us:u64,cursor:&'a MusicCursor}
        self.publish_numeric(blocking, &Receipt {schema:1,requested_us,received_us,cursor:value})
    }

    pub fn publish_doze(self, blocking: &crate::rt::Blocking, cursor:&MusicCursor, boundary:&DozeBoundary,
        armed_us:u64, received_us:u64)->Result<(),String> {
        if armed_us == 0 || boundary.ordinal == 0 || armed_us > boundary.requested_us
            || boundary.requested_us > boundary.confirmed_us
            || boundary.confirmed_us > cursor.begin_us || cursor.begin_us > cursor.end_us
            || cursor.end_us > received_us || received_us - armed_us > 1_200_000_000
            || received_us - boundary.confirmed_us > 2_000_000 {
            return Err("doze observation request/completion clock refused".into());
        }
        #[derive(Serialize)]
        struct Receipt<'a> {schema:u8,armed_us:u64,received_us:u64,boundary:&'a DozeBoundary,cursor:&'a MusicCursor}
        self.publish_numeric(blocking, &Receipt {schema:1,armed_us,received_us,boundary,cursor})
    }

    fn publish_numeric(self,blocking: &crate::rt::Blocking,value:&impl Serialize)->Result<(),String> {
        let final_path=CString::new(format!("numeric-{}.json",self.name)).unwrap();
        let temporary=CString::new(format!(".numeric-{}-{}.tmp",self.name,std::process::id())).unwrap();
        let directory=self.directory.as_ref().expect("authorized directory");
        let mut file=system::open_at(blocking, directory,&temporary,
            libc::O_WRONLY|libc::O_CREAT|libc::O_EXCL|libc::O_NOFOLLOW|libc::O_CLOEXEC,0o600)
            .map_err(|_|"numeric probe temporary file refused")?;
        let result=(|| {
            let bytes=serde_json::to_vec(value).map_err(|_|"numeric probe serialization failed")?;
            file.write_all(&bytes).map_err(|_|"numeric probe write failed")?;
            file.write_all(b"\n").map_err(|_|"numeric probe write failed")?;
            drop(file);
            // Hard-link publication is atomic and refuses an existing final
            // path. A reader never sees an empty/partial published record.
            if system::link_at(blocking, directory,&temporary,&final_path).is_err() {
                Err("numeric probe output already exists or cannot publish")
            } else {Ok(())}
        })();
        let removed=system::unlink_at(blocking, directory,&temporary).is_ok();
        if !removed {return Err("owned numeric probe temporary cleanup failed".into());}
        result.map_err(str::to_string)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt,DirBuilderExt};

    struct Fixture {root:PathBuf}
    impl Fixture {
        fn new()->Self {
            let parent=std::env::var_os("Q2_GENERATED_TEST_ROOT").map(PathBuf::from)
                .unwrap_or_else(||PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../build"))
                .join("numeric-probe-tests");
            std::fs::create_dir_all(&parent).unwrap();
            let root=parent.join(format!("{}-{}",std::process::id(),fastrand::u64(..)));
            std::fs::DirBuilder::new().mode(0o700).create(&root).unwrap();
            let mut marker=std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600)
                .open(root.join("numeric-probe-enabled")).unwrap();
            marker.write_all(MARKER).unwrap();Self {root}
        }
        fn authorize(&self,name:&str)->Result<Output,String> {Output::at(&crate::rt::Blocking::dedicated_thread(), name,&self.root)}
    }
    impl Drop for Fixture {fn drop(&mut self) {std::fs::remove_dir_all(&self.root).unwrap();}}

    fn sample()->Playback {
        let status=Status {queue:std::sync::Arc::new(vec![subsonic::Song {id:"generated-media-1".into(),
            title:"synthetic private title".into(),..Default::default()}]),generation:7,position_ms:1234,
            error:Some("synthetic private error".into()),output_revision:9,output_usb_card:Some(2),
            output_owned:true,pause_admission_failed:true,output_pause_kind:3,output_pause_frames:4096,..Default::default()};
        Playback::from_status(&status)
    }

    #[test]
    fn doze_publication_retains_intent_bracket_and_refuses_invalid_boundaries() {
        let fixture = Fixture::new();
        let cursor = MusicCursor { schema:1,pid:42,begin_us:200,end_us:210,generation:3,
            seek_revision:4,queue_revision:5,queue_len:1,queue_index:0,selection_tag:99,
            position_ms:1750,base_ms:1000,written_frames:8000,queued_frames:2000,
            sample_rate:8000,output_revision:9 };
        let boundary = DozeBoundary {ordinal:2,requested_us:100,confirmed_us:150,estimated_ahead_ms:999};
        for change in 0..10 {
            let mut cursor = cursor.clone();
            let mut boundary = boundary.clone();
            let (mut armed, mut received) = (50, 220);
            match change {
                0 => armed = 0, 1 => armed = 101, 2 => boundary.ordinal = 0,
                3 => boundary.requested_us = 151, 4 => boundary.confirmed_us = 201,
                5 => cursor.begin_us = 211, 6 => received = 209,
                7 => received = 2_000_151, 8 => received = 1_200_000_051,
                _ => {boundary.requested_us = 0; boundary.confirmed_us = 0;},
            }
            assert!(fixture.authorize("doze").unwrap().publish_doze(&crate::rt::Blocking::dedicated_thread(), &cursor,&boundary,armed,received).is_err(), "mutation {change}");
            assert!(!fixture.root.join("numeric-doze.json").exists());
        }
        fixture.authorize("doze").unwrap().publish_doze(&crate::rt::Blocking::dedicated_thread(), &cursor,&boundary,50,220).unwrap();
        let raw = std::fs::read(fixture.root.join("numeric-doze.json")).unwrap();
        let value:serde_json::Value = serde_json::from_slice(&raw).unwrap();
        assert_eq!(value.as_object().unwrap().len(),5);
        assert_eq!(value["boundary"]["ordinal"],2);
        assert_eq!(value["boundary"]["requested_us"],100);
        assert_eq!(value["boundary"]["confirmed_us"],150);
        assert_eq!(value["cursor"]["begin_us"],200);
        for key in ["boundary","cursor"] {
            assert!(value[key].as_object().unwrap().values().all(serde_json::Value::is_number));
        }
        assert!(fixture.authorize("doze").unwrap().publish_doze(&crate::rt::Blocking::dedicated_thread(), &cursor,&boundary,50,220).is_err());
        assert_eq!(std::fs::read(fixture.root.join("numeric-doze.json")).unwrap(),raw);
    }

    #[test]
    fn fresh_cursor_publication_requires_clock_bounds_and_never_overwrites() {
        let fixture = Fixture::new();
        let cursor = MusicCursor { schema:1,pid:42,begin_us:200,end_us:210,generation:3,
            seek_revision:4,queue_revision:5,queue_len:1,queue_index:0,selection_tag:99,
            position_ms:1750,base_ms:1000,written_frames:8000,queued_frames:2000,
            sample_rate:8000,output_revision:9 };
        for (request, received) in [(0,220),(201,220),(100,209),(100,2_000_101)] {
            assert!(fixture.authorize("cursor").unwrap().publish_cursor(&crate::rt::Blocking::dedicated_thread(), &cursor,request,received).is_err());
            assert!(!fixture.root.join("numeric-cursor.json").exists());
        }
        fixture.authorize("cursor").unwrap().publish_cursor(&crate::rt::Blocking::dedicated_thread(), &cursor,100,220).unwrap();
        let raw = std::fs::read(fixture.root.join("numeric-cursor.json")).unwrap();
        let value:serde_json::Value = serde_json::from_slice(&raw).unwrap();
        assert_eq!(value["requested_us"],100); assert_eq!(value["received_us"],220);
        assert_eq!(value["cursor"]["position_ms"],1750);
        assert_eq!(value.as_object().unwrap().len(),4);
        assert!(value["cursor"].as_object().unwrap().values().all(serde_json::Value::is_number));
        assert!(fixture.authorize("cursor").unwrap().publish_cursor(&crate::rt::Blocking::dedicated_thread(), &cursor,100,220).is_err());
        assert_eq!(std::fs::read(fixture.root.join("numeric-cursor.json")).unwrap(),raw);
    }

    #[test]
    fn completion_wait_requires_same_selection_seek_and_successful_stop() {
        let mut first=sample();first.error=false;first.pause_admission_failed=false;first.play_state=1;
        assert!(!first.finished_for(&first).unwrap());
        let mut next=first.clone();next.play_state=2;assert!(!next.finished_for(&first).unwrap());
        next.play_state=0;assert!(next.finished_for(&first).unwrap());
        for change in 0..6 {
            let mut wrong=next.clone();match change {
                0=>wrong.generation+=1,1=>wrong.seek_revision+=1,2=>wrong.queue_index+=1,
                3=>wrong.selection_tag=Some(999),4=>wrong.error=true,_=>wrong.pause_admission_failed=true,
            }
            assert!(wrong.finished_for(&first).is_err());
        }
        let mut empty=first.clone();empty.selection_tag=None;assert!(empty.finished_for(&empty).is_err());
    }

    #[test]
    fn launcher_role_is_required_before_runtime_access() {
        let not_accessed=Path::new("/numeric-probe-must-not-access-this-path");
        for (enabled,storage) in [(false,Some("0")),(true,Some("1")),(true,None)] {
            let result=Output::guarded(&crate::rt::Blocking::dedicated_thread(), "role",enabled,storage.map(std::ffi::OsStr::new),Some(not_accessed));
            assert_eq!(result.err().unwrap(),"numeric probe requires an explicit generated launcher");
        }
        let fixture=Fixture::new();
        Output::guarded(&crate::rt::Blocking::dedicated_thread(), "role",true,Some(std::ffi::OsStr::new("0")),Some(&fixture.root)).unwrap().publish(&crate::rt::Blocking::dedicated_thread(), &sample()).unwrap();
    }

    #[test]
    fn receipts_contain_only_fixed_keys_and_numeric_metadata() {
        let value=serde_json::to_value(sample()).unwrap();
        assert_eq!(value["pid"],std::process::id());assert_eq!(value["generation"],7);assert_eq!(value["position_ms"],1234);
        assert_eq!(value["output_usb_card"],2);assert_eq!(value["output_revision"],9);
        assert_eq!(value["output_pause_kind"],3);assert_eq!(value["output_pause_frames"],4096);
        let mut keys:Vec<_>=value.as_object().unwrap().keys().map(String::as_str).collect();keys.sort_unstable();
        let mut expected=vec!["schema","pid","generation","seek_revision","queue_index","selection_tag","position_ms","play_state","output_revision","output_usb_card","output_usb_device","output_owned","pause_admission_failed","output_pause_kind","output_pause_frames","error"];
        expected.sort_unstable();assert_eq!(keys,expected);
        assert!(value.as_object().unwrap().values().all(|v|v.is_number() || v.is_boolean() || v.is_null()));
        let bytes=serde_json::to_string(&value).unwrap();
        for private in ["generated-media-1","synthetic private title","synthetic private error"] {assert!(!bytes.contains(private));}
        let mut other=Status::default();other.queue=std::sync::Arc::new(vec![subsonic::Song {id:"generated-media-2".into(),..Default::default()}]);
        assert_ne!(Playback::from_status(&other).selection_tag,sample().selection_tag);
        assert!(sample().route_is("usb"));assert!(!sample().route_is("headphones"));
        assert!(Playback::from_status(&other).route_is("headphones"));assert!(!sample().route_is("unknown"));
    }

    #[test]
    fn complete_exclusive_record_is_published_and_owned_temporary_removed() {
        let fixture=Fixture::new();fixture.authorize("before_unplug").unwrap().publish(&crate::rt::Blocking::dedicated_thread(), &sample()).unwrap();
        let path=fixture.root.join("numeric-before_unplug.json");
        let bytes=std::fs::read(&path).unwrap();assert!(bytes.ends_with(b"\n"));
        assert_eq!(serde_json::from_slice::<serde_json::Value>(&bytes).unwrap(),serde_json::to_value(sample()).unwrap());
        assert_eq!(std::fs::metadata(path).unwrap().mode()&0o777,0o600);
        assert!(!std::fs::read_dir(&fixture.root).unwrap().any(|p|p.unwrap().file_name().to_string_lossy().ends_with(".tmp")));
    }

    #[test]
    fn existing_regular_or_linked_output_is_never_replaced() {
        for symlink in [false,true] {
            let fixture=Fixture::new();let owned=fixture.root.join("retained");std::fs::write(&owned,b"retained bytes").unwrap();
            let final_path=fixture.root.join("numeric-after.json");
            if symlink {std::os::unix::fs::symlink(&owned,&final_path).unwrap();} else {std::fs::write(&final_path,b"prior receipt").unwrap();}
            assert!(fixture.authorize("after").unwrap().publish(&crate::rt::Blocking::dedicated_thread(), &sample()).is_err());
            assert_eq!(std::fs::read(&owned).unwrap(),b"retained bytes");
            assert_eq!(std::fs::read(&final_path).unwrap(),if symlink {b"retained bytes".as_slice()}else{b"prior receipt".as_slice()});
        }
    }

    #[test]
    fn preexisting_temporary_is_not_claimed_or_removed() {
        let fixture=Fixture::new();let temporary=fixture.root.join(format!(".numeric-reserved-{}.tmp",std::process::id()));
        std::fs::write(&temporary,b"not ours").unwrap();
        assert!(fixture.authorize("reserved").unwrap().publish(&crate::rt::Blocking::dedicated_thread(), &sample()).is_err());
        assert_eq!(std::fs::read(temporary).unwrap(),b"not ours");
        assert!(!fixture.root.join("numeric-reserved.json").exists());
    }

    #[test]
    fn marker_type_mode_content_and_link_count_are_required() {
        for variant in ["missing","mode","content","symlink","hardlink","fifo"] {
            let fixture=Fixture::new();let marker=fixture.root.join("numeric-probe-enabled");
            match variant {
                "mode"=>std::fs::set_permissions(&marker,std::fs::Permissions::from_mode(0o644)).unwrap(),
                "content"=>std::fs::write(&marker,b"unqualified marker").unwrap(),
                "hardlink"=>std::fs::hard_link(&marker,fixture.root.join("second-marker")).unwrap(),
                _=>{
                    std::fs::remove_file(&marker).unwrap();
                    if variant=="symlink" {std::os::unix::fs::symlink("absent",&marker).unwrap();}
                    if variant=="fifo" {system::create_fifo(&marker,0o600).unwrap();}
                }
            }
            assert!(fixture.authorize("refused").is_err(),"{variant}");
            assert!(!fixture.root.join("numeric-refused.json").exists());
        }
    }

    #[test]
    fn runtime_alias_and_unsafe_names_are_refused() {
        let fixture=Fixture::new();
        for name in ["","../other","a/b","a.b","with space",&"a".repeat(33)] {assert!(fixture.authorize(name).is_err());}
        let alias=fixture.root.join("runtime-alias");std::os::unix::fs::symlink(&fixture.root,&alias).unwrap();
        assert!(Output::at(&crate::rt::Blocking::dedicated_thread(), "refused",&alias).is_err());
        std::fs::set_permissions(&fixture.root,std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(fixture.authorize("refused").is_err());
    }

    #[test]
    fn authorized_directory_descriptor_survives_path_replacement() {
        let fixture=Fixture::new();let other=Fixture::new();let output=fixture.authorize("anchored").unwrap();
        let renamed=fixture.root.with_extension("moved");std::fs::rename(&fixture.root,&renamed).unwrap();
        std::os::unix::fs::symlink(&other.root,&fixture.root).unwrap();
        let result=output.publish(&crate::rt::Blocking::dedicated_thread(), &sample());
        std::fs::remove_file(&fixture.root).unwrap();std::fs::rename(&renamed,&fixture.root).unwrap();
        result.unwrap();assert!(fixture.root.join("numeric-anchored.json").is_file());
        assert!(!other.root.join("numeric-anchored.json").exists());
    }
}
