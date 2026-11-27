//! One owned download file across bounded reconnect attempts. Partial bytes
//! are retained only for this process; restart discards unverified state.
use super::{Library,mix_space::Ticket,transfer_io::{self,Cancel,Failure}};
use crate::audio::source::Progress;
use std::{future::Future,pin::Pin,sync::Arc,time::Duration};
use std::os::unix::fs::MetadataExt;
use std::io::{Seek,SeekFrom};
use std::path::PathBuf;
use subsonic::http::{Response,resume::{self,Admission,Validator}};

pub(super) type Fetch=Arc<dyn Fn(u64,Option<String>)->Pin<Box<dyn Future<Output=Result<Response,WireError>>+Send>>+Send+Sync>;
pub(super) enum WireError { Retry, Fatal(String) }
#[derive(Clone,Default)]
pub(super) struct State {pub validator:Option<Validator>,pub offset:u64}
pub(super) enum Error { Cancelled, Suspended(State), Fatal(String) }
struct Pinned(Option<Arc<std::fs::File>>);
impl Drop for Pinned {fn drop(&mut self) {crate::rt::drop_off_executor(self.0.take());}}

fn prepare_output(_blocking:&crate::rt::Blocking,file:Arc<std::fs::File>,named:PathBuf,offset:u64)->std::io::Result<transfer_io::FileOutput> {
    let fd=file.metadata()?;let path=std::fs::symlink_metadata(named)?;
    if !fd.is_file() || !path.file_type().is_file() || (fd.dev(),fd.ino())!=(path.dev(),path.ino()) || fd.len()<offset {return Err(std::io::Error::other("download ownership changed"));}
    let mut out=file.try_clone()?;
    out.set_len(offset)?;out.seek(SeekFrom::Start(offset))?;Ok(transfer_io::FileOutput::new(out))
}

/// The caller owns the pinned descriptor and must retain it until this future
/// finishes and every accepted card write has joined. This function never
/// publishes a file; only the caller may do so after a complete length check.
pub(super) async fn file(lib:Arc<Library>,path:PathBuf,pinned:Arc<std::fs::File>,expected:Option<u64>,progress:Arc<Progress>,cancel:Arc<Cancel>,retry:bool,initial:State,fetch:Fetch,storage:Arc<Ticket>)->Result<(u64,State),Error> {
    let pinned=Pinned(Some(pinned));
    let mut validator=initial.validator;let mut offset=initial.offset;
    progress.set_total(validator.as_ref().map(|v| v.total).or(expected));
    progress.advance(offset);
    // Revalidate a complete retained part with its last byte before reuse.
    if validator.as_ref().is_some_and(|v|offset==v.total && offset>0) {offset-=1;}
    loop {
        if cancel.is_cancelled() {return Err(Error::Cancelled);}
        if retry && (lib.offline_mode() || lib.downloads.is_paused()) {return Err(Error::Suspended(State {validator,offset}));}
        let opening=fetch(offset,validator.as_ref().map(|v|v.etag.clone()));tokio::pin!(opening);
        let response=loop {
            let changed=lib.downloads.changed();tokio::pin!(changed);changed.as_mut().enable();
            if retry && (lib.offline_mode() || lib.downloads.is_paused()) {break None;}
            tokio::select! {biased;
                ()=cancel.cancelled()=>return Err(Error::Cancelled),
                response=&mut opening=>break Some(response),
                ()=changed=>{},
            }
        };
        let Some(response)=response else {return Err(Error::Suspended(State {validator,offset}));};
        let response=match response {Ok(response)=>response,Err(WireError::Retry)=>{if !retry {return Err(Error::Fatal("Download connection interrupted".into()));}return Err(Error::Suspended(State {validator,offset}));},Err(WireError::Fatal(error))=>return Err(Error::Fatal(error))};
        if retry && (lib.offline_mode() || lib.downloads.is_paused()) {drop(response);return Err(Error::Suspended(State {validator,offset}));}
        let admission=match resume::admit(&response,offset,validator.as_ref()) {
            Ok(a)=>a,
            Err(()) if offset>0=>{validator=None;offset=0;continue},
            Err(())=>return Err(Error::Fatal("Unsupported download response".into())),
        };
        match admission {
            Admission::RetryFromZero=>{validator=None;offset=0;continue},
            Admission::Restart(next)=>{offset=0;validator=next;},
            Admission::Append=>{},
        }
        if expected.is_some_and(|size|validator.as_ref().is_some_and(|v|v.total!=size)) {return Err(Error::Fatal("Download length changed".into()));}
        let response_length=response.header("Content-Length").and_then(|v|v.parse::<u64>().ok());
        let total=validator.as_ref().map(|v|v.total).or(expected).or(response_length);
        if offset>0 && total.is_none() {return Err(Error::Fatal("Download length unavailable".into()));}
        let file=pinned.0.as_ref().expect("owned partial file").clone();let named=path.clone();
        let output=crate::rt::spawn_blocking(move |blocking|prepare_output(blocking,file,named,offset)).await.map_err(|_|Error::Fatal("Download worker stopped".into()))?.map_err(|_|Error::Fatal("Unable to retain download on the card".into()))?;
        let requested = total.map_or(0, |total| total.saturating_sub(offset));
        if let Err(error) = storage.ensure(requested, 1).await {
            if error.kind() == std::io::ErrorKind::Interrupted { return Err(Error::Cancelled); }
            return Err(normalize_space(error));
        }
        let local=Arc::new(Cancel::default());let copying=transfer_io::copy_file_resume(response.into_decoded(),output,progress.clone(),local.clone(),total,offset,lib.clone(),storage.clone());
        tokio::pin!(copying);let mut last=progress.snapshot().written;let mut since=tokio::time::Instant::now();
        let outcome=loop {
            let changed=lib.downloads.changed();tokio::pin!(changed);changed.as_mut().enable();
            tokio::select! {biased;
                result=&mut copying=>break result,
                ()=cancel.cancelled()=>{local.cancel();let _=copying.await;return Err(Error::Cancelled)},
                ()=changed=>{if lib.offline_mode() || lib.downloads.is_paused(){local.cancel();break copying.await;}},
                ()=tokio::time::sleep(Duration::from_secs(5))=>{
                    let current=progress.snapshot().written;
                    if lib.playback_transfer_running() || current!=last {last=current;since=tokio::time::Instant::now();}
                    else if since.elapsed()>=Duration::from_secs(60) {local.cancel();break copying.await;}
                },
            }
        };
        match outcome {
            Ok(size)=>return Ok((size,State {validator,offset:size})),
            Err(Failure::Cancelled) if cancel.is_cancelled()=>return Err(Error::Cancelled),
            Err(Failure::Cancelled|Failure::Network(_))=>{
                if !retry {return Err(Error::Fatal("Download connection interrupted".into()));}
                offset=if validator.is_some() {progress.snapshot().written} else {0};
                // A completed-length interruption is retried from zero rather
                // than trusting a 416 response without revalidating bytes.
                if validator.as_ref().is_some_and(|v|offset>=v.total) {offset=0;validator=None;}
                return Err(Error::Suspended(State {validator,offset}));
            },
            Err(Failure::Length)=>return Err(Error::Fatal("Incomplete downloaded audio".into())),
            Err(Failure::Card(error)|Failure::Sync(error)) if error.raw_os_error()==Some(libc::ENOSPC)=>return Err(Error::Fatal("Card full".into())),
            Err(Failure::Card(_)|Failure::Sync(_))=>return Err(Error::Fatal("Unable to save download on the card".into())),
        }
    }
}

pub(super) fn normalize_space(error: std::io::Error) -> Error {
    Error::Fatal(if error.raw_os_error() == Some(libc::ENOSPC) {
        "Card full".into()
    } else { "Unable to prepare space for download".into() })
}
