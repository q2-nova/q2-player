//! Whitelisted process-exit attribution. The shell's wait status is intentionally
//! not decoded as a signal: exit(128 + N) has the same shell status as signal N.
//! This helper reads only the generated context marker, never player config.
use super::{Context, ProcessIdentity};
use crate::{
    config::telemetry::{Stream, valid_uuid},
    rt::Blocking,
};
use serde::{Deserialize, Serialize};
use std::{io, path::Path};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(super) enum Reason {
    UnexpectedExit,
    CommandedStop,
    LauncherCleanup,
}
impl std::str::FromStr for Reason {
    type Err = io::Error;
    fn from_str(value: &str) -> io::Result<Self> {
        match value {
            "unexpected-exit" => Ok(Self::UnexpectedExit),
            "commanded-stop" => Ok(Self::CommandedStop),
            "launcher-cleanup" => Ok(Self::LauncherCleanup),
            _ => Err(invalid()),
        }
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CaptureContext {
    schema: u32,
    enabled: bool,
    build: String,
    boot: String,
    process: ProcessIdentity,
    context: Context,
}
pub(super) struct ExitRecord {
    pub id: String,
    pub header: super::crash::Header,
    pub event: super::crash::Event,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SkipReason {
    ContextAbsent,
    Disabled,
    DifferentProcess,
    DifferentBoot,
}
pub(super) enum Preparation {
    Ready(ExitRecord),
    Skipped(SkipReason),
}
fn invalid() -> io::Error {
    io::Error::other("invalid bounded process-exit evidence")
}
fn digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}
fn valid_context(context: &Context) -> bool {
    context.clock == "boottime"
        && context.kernel.as_ref().is_none_or(|v| digest(v))
        && context.platform.as_ref().is_none_or(|v| digest(v))
        && context.run_id.as_ref().is_none_or(|v| valid_uuid(v))
        && match context.stream {
            Stream::Production => context.labels.is_empty(),
            Stream::AutomatedTest => context.labels == ["automated-test"],
        }
}
/// Caller holds the service's single-launcher boundary and invokes this after
/// wait, before launching the next process. A disabled/absent/stale context is
/// an explicit skip, never a successful report and never inferred opt-in.
/// No context marker is changed or removed here. This also makes bounded helper
/// retries idempotent while preserving the captured boot/build/context.
pub(super) fn prepare_exit(
    blocking: &Blocking,
    directory: &Path,
    current_boot: &str,
    pid: u32,
    start_ticks: u64,
    wait_status: u32,
    reason: Reason,
) -> io::Result<Preparation> {
    if !valid_uuid(current_boot)
        || pid == 0
        || pid > i32::MAX as u32
        || start_ticks == 0
        || start_ticks > i64::MAX as u64
        || wait_status > 255
    {
        return Err(invalid());
    }
    let path = directory.join("capture-context.json");
    let bytes = match super::bounded(blocking, &path, 4096) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(Preparation::Skipped(SkipReason::ContextAbsent));
        }
        Err(error) => return Err(error),
    };
    let captured: CaptureContext = serde_json::from_slice(&bytes).map_err(|_| invalid())?;
    if captured.schema != 2
        || !digest(&captured.build)
        || !valid_uuid(&captured.boot)
        || captured.process.pid > i32::MAX as u32
        || !valid_context(&captured.context)
    {
        return Err(invalid());
    }
    if !captured.enabled {
        return Ok(Preparation::Skipped(SkipReason::Disabled));
    }
    if captured.boot != current_boot {
        return Ok(Preparation::Skipped(SkipReason::DifferentBoot));
    }
    if captured.process != (ProcessIdentity { pid, start_ticks }) {
        return Ok(Preparation::Skipped(SkipReason::DifferentProcess));
    }
    // One process has one terminal wait event. Stable identity deliberately
    // excludes status/reason: a conflicting later attribution must be refused
    // by the queue, rather than turned into a second apparently valid event.
    let identity = format!(
        "q2-process-exit-v2:{}:{}:{}",
        captured.boot, pid, start_ticks
    );
    let hash = ring::digest::digest(&ring::digest::SHA256, identity.as_bytes());
    let mut id_bytes = [0u8; 16];
    id_bytes.copy_from_slice(&hash.as_ref()[..16]);
    id_bytes[6] = (id_bytes[6] & 15) | 80;
    id_bytes[8] = (id_bytes[8] & 63) | 128;
    let hex: String = id_bytes.iter().map(|b| format!("{b:02x}")).collect();
    let id = format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    );
    Ok(Preparation::Ready(ExitRecord {
        id,
        header: super::crash::Header {
            build: captured.build,
            boot: captured.boot,
            process: captured.process,
            context: captured.context,
        },
        event: super::crash::Event::ProcessExit {
            wait_status,
            reason,
        },
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    const BOOT: &str = "12345678-1234-1234-1234-123456789abc";
    const RUN: &str = "12345678-1234-1234-1234-123456789def";
    fn context() -> serde_json::Value {
        serde_json::json!({"schema":2,"enabled":true,
        "boot":BOOT,"build":"a".repeat(64),"process":{"pid":42,"start_ticks":123},
        "context":{"stream":"automated-test","labels":["automated-test"],"run_id":RUN,
            "kernel":null,"platform":null,"clock":"boottime"}})
    }
    fn fixture() -> std::path::PathBuf {
        let directory = crate::library::generated_test_directory("process-exit");
        std::fs::create_dir_all(&directory).unwrap();
        directory
    }
    fn write(directory: &Path, value: &serde_json::Value) {
        std::fs::write(
            directory.join("capture-context.json"),
            serde_json::to_vec(value).unwrap(),
        )
        .unwrap();
    }
    fn prepare(directory: &Path, status: u32, reason: Reason) -> io::Result<Preparation> {
        prepare_exit(
            &Blocking::dedicated_thread(),
            directory,
            BOOT,
            42,
            123,
            status,
            reason,
        )
    }
    #[test]
    fn original_context_and_ambiguous_wait_status_are_preserved_without_signal_inference() {
        let directory = fixture();
        write(&directory, &context());
        let Preparation::Ready(first) = prepare(&directory, 139, Reason::UnexpectedExit).unwrap()
        else {
            panic!("missing record")
        };
        assert_eq!(first.header.context.run_id.as_deref(), Some(RUN));
        assert_eq!(first.header.boot, BOOT);
        assert_eq!(first.header.process.start_ticks, 123);
        assert_eq!(
            serde_json::to_value(&first.event).unwrap(),
            serde_json::json!({"type":"process-exit","wait_status":139,"reason":"unexpected-exit"})
        );
        let Preparation::Ready(second) = prepare(&directory, 0, Reason::CommandedStop).unwrap()
        else {
            panic!("missing record")
        };
        assert_eq!(
            first.id, second.id,
            "conflicting attributions must collide at durable queue"
        );
        assert!(valid_uuid(&first.id));
        std::fs::remove_dir_all(directory).unwrap();
    }
    #[test]
    fn missing_disabled_other_process_and_other_boot_are_explicit_skips() {
        let directory = fixture();
        assert!(matches!(
            prepare(&directory, 1, Reason::UnexpectedExit).unwrap(),
            Preparation::Skipped(SkipReason::ContextAbsent)
        ));
        for (key, value, reason) in [
            ("enabled", serde_json::json!(false), SkipReason::Disabled),
            (
                "process",
                serde_json::json!({"pid":42,"start_ticks":124}),
                SkipReason::DifferentProcess,
            ),
            ("boot", serde_json::json!(RUN), SkipReason::DifferentBoot),
        ] {
            let mut input = context();
            input[key] = value;
            write(&directory, &input);
            assert!(
                matches!(prepare(&directory,1,Reason::UnexpectedExit).unwrap(),Preparation::Skipped(value) if value==reason)
            );
        }
        std::fs::remove_dir_all(directory).unwrap();
    }
    #[test]
    fn strict_bounded_marker_refuses_private_fields_and_invalid_event_inputs() {
        let directory = fixture();
        for mutate in ["extra", "context", "schema"] {
            let mut value = context();
            match mutate {
                "extra" => {
                    value["token"] = serde_json::json!("secret-canary");
                }
                "context" => {
                    value["context"]["labels"] = serde_json::json!(["secret-canary"]);
                }
                _ => {
                    value["schema"] = serde_json::json!(1);
                }
            }
            write(&directory, &value);
            assert!(prepare(&directory, 1, Reason::UnexpectedExit).is_err());
        }
        std::fs::write(directory.join("capture-context.json"), vec![b'x'; 4097]).unwrap();
        assert!(prepare(&directory, 1, Reason::UnexpectedExit).is_err());
        write(&directory, &context());
        assert!(prepare(&directory, 256, Reason::UnexpectedExit).is_err());
        assert!("signal-11".parse::<Reason>().is_err());
        for event in [
            serde_json::json!({"type":"process-exit","wait_status":139,"reason":"unexpected-exit","signal":11}),
            serde_json::json!({"type":"process-exit","wait_status":139,"reason":"unknown"}),
        ] {
            assert!(serde_json::from_value::<super::super::crash::Event>(event).is_err());
        }
        std::fs::remove_file(directory.join("capture-context.json")).unwrap();
        std::os::unix::fs::symlink("missing", directory.join("capture-context.json")).unwrap();
        assert!(prepare(&directory, 1, Reason::UnexpectedExit).is_err());
        std::fs::remove_dir_all(directory).unwrap();
    }
}
