use super::super::{HomeSeed, Media, WindowSeed, decode, entry};
use super::*;
fn projection(w: Window, rows: Rows) -> String {
    let entries: Vec<_> = w
        .rows
        .into_iter()
        .map(|r| entry(r, rows, Media::Book, "a", Some("show")).map_err(|_| ()))
        .collect();
    format!(
        "{:?}",
        (
            entries, w.count, w.total, w.page, w.limit, w.library, w.found
        )
    )
}
fn legacy_projection(body: &[u8], plan: &Plan) -> Result<String, ()> {
    decode(
        body,
        WindowSeed {
            path: &plan.path,
            start: plan.start,
            limit: plan.limit,
            rows: plan.rows,
        },
    )
    .map(|w| projection(w, plan.rows))
    .map_err(|_| ())
}
fn compare(body: &[u8], plan: Plan, width: usize) {
    let expected = legacy_projection(body, &plan);
    compare_projection(body, plan, width, expected);
}
fn compare_projection(body: &[u8], plan: Plan, width: usize, expected: Result<String, ()>) {
    let mut parser = Parser::window(plan.clone());
    let actual = body
        .chunks(width)
        .try_for_each(|chunk| parser.feed(chunk))
        .and_then(|()| parser.finish())
        .map(|w| projection(w, plan.rows))
        .map_err(|_| ());
    assert_eq!(
        actual,
        expected,
        "width={width}, input={:?}",
        String::from_utf8_lossy(&body[..body.len().min(600)])
    );
}
fn plan(rows: Rows, start: u64) -> Plan {
    Plan {
        path: vec!["results"],
        start,
        limit: 2,
        rows,
    }
}
#[test]
fn fragmentation_preserves_unicode_escapes_duplicates_and_sequence_structs() {
    let fixtures:[&[u8];8]=[
  br#"{"results":[{"id":"x","media":{"metadata":{"title":"A\uD83D\uDE80\n\"\\"}}}],"total":10,"page":2,"limit":2,"libraryId":"a"}"#,
  "{\"results\":[{\"id\":\"é🚀\",\"media\":{\"metadata\":{\"title\":\"日本語\"}}}]}".as_bytes(),
  br#"{"total":7,"results":{"results":[]},"total":null,"results":[{"id":"x"}]}"#,
  br#"{"results":[["id",null,"title",null,null,null,null,null,null,[["Title",null,"Author"],2]]]}"#,
  br#"{"results":[{"id":"x","id":null}]}"#,
  br#"{"results":[{"episode":"12","audioFile":{}},{"episode":-1,"audioFile":null},{"episode":{"id":"ep"}}]}"#,
  br#"{"results":[{"books":[1,null,{},[2,3]],"items":null,"media":{}}]}"#,
  br#"{"results":[{"id":null,"media":null}]}"#,
 ];
    for body in fixtures {
        for width in 1..=body.len().min(64) {
            for start in [0, 1, 8] {
                compare(body, plan(Rows::Items, start), width);
            }
        }
    }
}
#[test]
fn randomized_public_projection_and_adversarial_spelling_match_legacy_seed() {
    let mut rng = fastrand::Rng::with_seed(0x50a9_ee3);
    for _ in 0..800 {
        let mut rows = Vec::new();
        for i in 0..rng.usize(0..8) {
            rows.push(serde_json::json!({"id":format!("id{i}"),"title":"Episode","libraryId":"a","libraryItemId":"show","media":{"metadata":{"title":"é🚀","authorName":"author","subtitle":"unused"},"episodes":[{"ignored":"stuff"}]},"audioFile":if rng.bool(){serde_json::json!({"any":[null,true]})}else{serde_json::Value::Null},"books":[1,2],"episode":if rng.bool(){serde_json::json!("12")}else{serde_json::Value::Null}}));
        }
        let mut bytes=serde_json::to_vec(&serde_json::json!({"results":rows,"total":30,"libraryId":"a","unused":[{"large":-1e60}]})).unwrap();
        if rng.bool() && !bytes.is_empty() {
            let at = rng.usize(..bytes.len());
            bytes[at] = rng.u8(..);
        }
        for kind in [
            Rows::Items,
            Rows::PodcastEpisodes,
            Rows::Collections,
            Rows::PlaylistItems,
        ] {
            compare(&bytes, plan(kind, rng.u64(0..8)), rng.usize(1..80));
        }
    }
    // Exercise every reference retained by entry(), plus deeper references
    // that must still be validated without building an arbitrary Row tree.
    fn nested(rng: &mut fastrand::Rng, depth: u32) -> serde_json::Value {
        let mut row = serde_json::json!({"id":format!("id{}",rng.u32(..)),"title":"Episode","name":"Group","libraryId":"a","libraryItemId":"show","episodeId":"fallback","media":{"metadata":{"title":"Book","authorName":"Writer"}},"podcast":{"metadata":{"title":"Show"}},"books":[{},{}]});
        if depth != 0 {
            for key in ["recentEpisode", "episode", "libraryItem"] {
                if rng.bool() {
                    row[key] = nested(rng, depth - 1);
                }
            }
        }
        row
    }
    for _ in 0..160 {
        let body = serde_json::to_vec(
            &serde_json::json!({"results":[nested(&mut rng,4),nested(&mut rng,2)]}),
        )
        .unwrap();
        for rows in [
            Rows::Items,
            Rows::PlaylistItems,
            Rows::Episodes,
            Rows::Series,
        ] {
            compare(&body, plan(rows, 0), rng.usize(1..128));
        }
    }
    for value in [
        "-0",
        "18446744073709551615",
        "18446744073709551616",
        "1.0",
        "1e999999999999",
        "01",
        "1.",
        "--1",
        "1e+",
        "null",
        "true",
    ] {
        for field in ["episode", "numBooks", "books", "unknown"] {
            let body = format!("{{\"results\":[{{\"{field}\":{value}}}]}}");
            compare(body.as_bytes(), plan(Rows::Items, 3), 1);
        }
    }
}
#[test]
fn home_order_duplicates_limits_and_all_shelves_match_seed() {
    let samples = [
        r#"[{"entities":[{"id":"x","media":{"metadata":{"title":"x"}}}],"total":8,"id":"discover"}]"#,
        r#"[{"id":"discover","entities":[]},{"entities":[{"id":"y"}],"id":"discover","total":2}]"#,
        r#"[{"id":"other","entities":[{"id":false}]}]"#,
        r#"[{"id":"discover"}]"#,
        r#"[{"id":"other"}]"#,
    ];
    for body in samples {
        for width in 1..=body.len().min(32) {
            let expected = decode(
                body.as_bytes(),
                HomeSeed {
                    id: "discover",
                    limit: 2,
                },
            )
            .map(|w| projection(w, Rows::Items))
            .map_err(|_| ());
            let mut parser = Parser::home("discover", 2);
            let actual = body
                .as_bytes()
                .chunks(width)
                .try_for_each(|b| parser.feed(b))
                .and_then(|()| parser.finish())
                .map(|w| projection(w, Rows::Items))
                .map_err(|_| ());
            assert_eq!(actual, expected, "{body}");
        }
    }
    for n in [32, 33] {
        let bytes =
            serde_json::to_vec(&vec![serde_json::json!({"id":"other","entities":[]}); n]).unwrap();
        let mut p = Parser::home("discover", 2);
        let actual = p.feed(&bytes).and_then(|()| p.finish());
        assert_eq!(actual.is_ok(), n == 32);
    }
}
#[test]
fn large_ignored_unselected_and_unused_scalars_remain_bounded_without_narrower_acceptance() {
    let long = "x".repeat(1024 * 1024);
    for field in ["id", "title", "unknown"] {
        let body = format!(
            "{{\"results\":[{{\"{field}\":\"{long}\"}},{{\"id\":\"selected\",\"media\":{{\"metadata\":{{\"title\":\"ok\",\"subtitle\":\"{long}\"}}}}}}]}}"
        );
        compare(body.as_bytes(), plan(Rows::Items, 1), 65536);
        compare(body.as_bytes(), plan(Rows::Items, 0), 65536);
    }
    // Serde IgnoredAny deliberately accepts these spellings, while a typed string
    // must reject them. Neither unknown nor unselected means skip typed validation.
    for spelling in [
        b"\xff".as_slice(),
        br"\uD800".as_slice(),
        br"\uDC00".as_slice(),
    ] {
        for field in ["unknown", "title"] {
            let mut body = format!("{{\"results\":[{{\"{field}\":\"").into_bytes();
            body.extend(spelling);
            body.extend(b"\"}]}");
            compare(&body, plan(Rows::Items, 3), 1);
        }
    }
    let body = format!(
        "{{\"unknown\":{}0{},\"results\":[]}}",
        "[".repeat(20000),
        "]".repeat(20000)
    );
    compare(body.as_bytes(), plan(Rows::Items, 0), 13);
    for depth in [30, 125, 128, 150] {
        let body = format!(
            "{{\"results\":[{}{{}}{}]}}",
            "{\"libraryItem\":".repeat(depth),
            "}".repeat(depth)
        );
        let plan = plan(Rows::Items, 3);
        // Only the legacy Serde oracle recursively constructs nested Rows.
        // Give its debug frames room to reach Serde's own depth limit. The
        // incremental parser still runs on the test harness's ordinary stack.
        let expected = std::thread::scope(|scope| {
            std::thread::Builder::new()
                .stack_size(8 * 1024 * 1024)
                .spawn_scoped(scope, || legacy_projection(body.as_bytes(), &plan))
                .unwrap()
                .join()
                .unwrap()
        });
        compare_projection(body.as_bytes(), plan, 17, expected);
    }
}
#[test]
fn snapshot_nested_scope_counts_and_eof_are_not_lost() {
    for body in [br#"{"total":9,"media":{"total":3,"episodes":[{"id":"a","audioFile":{}},{"id":"b","audioFile":null},{"id":"c","audioFile":true}]},"libraryId":"a"}"#.as_slice(),br#"{"media":{"episodes":[]},"libraryId":"wrong"}"#.as_slice()] {
  let p=Plan{path:vec!["media","episodes"],start:1,limit:1,rows:Rows::PodcastEpisodes};for width in 1..32 {compare(body,p.clone(),width);}for n in 0..body.len(){compare(&body[..n],p.clone(),7);}
 }
    // Stream the actual64MiB boundary with one reusable64KiB input block;
    // neither producer nor parser may construct a complete document buffer.
    let prefix = br#"{"results":[],"unknown":""#;
    let suffix = b"\"}";
    let mut exact = Parser::window(plan(Rows::Items, 0));
    exact.feed(prefix).unwrap();
    let chunk = vec![b'x'; 64 * 1024];
    let mut left = MAX_RESPONSE as usize - prefix.len() - suffix.len();
    while left != 0 {
        let n = left.min(chunk.len());
        exact.feed(&chunk[..n]).unwrap();
        left -= n;
    }
    exact.feed(suffix).unwrap();
    assert!(exact.finish().is_ok());
    let mut p = Parser::window(plan(Rows::Items, 0));
    p.feed(br#"{"results":[]}"#).unwrap();
    p.bytes = MAX_RESPONSE;
    p.feed(b" ").expect_err("exact decoded response bound");
}
#[test]
fn stalled_browse_does_not_occupy_document_workers_and_cancel_closes_connections() {
    if std::env::var_os("Q2_ABS_BROWSE_NETWORK").as_deref() != Some(std::ffi::OsStr::new("1")) {
        // Other native tests intentionally saturate the global HTTP decode pool.
        // Use a fresh owned process to make this capacity negative control causal.
        let result=std::process::Command::new(std::env::current_exe().unwrap())
   .args(["--exact","audiobookshelf_browse::stream::tests::stalled_browse_does_not_occupy_document_workers_and_cancel_closes_connections","--test-threads","1","--nocapture"])
   .env("Q2_ABS_BROWSE_NETWORK","1").output().unwrap();
        assert!(
            result.status.success(),
            "generated browse child failed: {} {}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        return;
    }
    use std::{
        io::{Read, Write},
        net::TcpListener,
        time::Duration,
    };
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let (ready, mut entered) = tokio::sync::mpsc::channel(4);
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stopping = stop.clone();
    let server = std::thread::spawn(move || {
        let mut children = Vec::new();
        while !stopping.load(std::sync::atomic::Ordering::Acquire) {
            match listener.accept() {
                Ok((mut socket, _)) => {
                    let ready = ready.clone();
                    children.push(std::thread::spawn(move||{
    socket.set_read_timeout(Some(Duration::from_secs(3))).unwrap();socket.set_write_timeout(Some(Duration::from_secs(3))).unwrap();let mut header=Vec::new();while !header.ends_with(b"\r\n\r\n"){let mut b=[0];if socket.read(&mut b).unwrap_or(0)==0{return false;}header.push(b[0]);if header.len()>8192{return false;}}
    assert!(String::from_utf8_lossy(&header).contains("Authorization: Bearer generated"));
    if header.starts_with(b"GET /api/probe "){socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}").unwrap();return false;}
    socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100000\r\nConnection: close\r\n\r\n{\"results\":[{\"id\":\"").unwrap();ready.blocking_send(()).unwrap();let mut b=[0];matches!(socket.read(&mut b),Ok(0))
   }));
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(1))
                }
                Err(e) => panic!("{e}"),
            }
        }
        children
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .filter(|closed| *closed)
            .count()
    });
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(2)
        .build()
        .unwrap();
    let ok = runtime.block_on(async {
        let client =
            super::super::Client::new(&format!("http://{address}"), "generated", None).unwrap();
        let mut outcomes = Vec::new();
        for legacy in [true, false] {
            let (working, mut started) = tokio::sync::mpsc::channel(3);
            let mut pending = Vec::new();
            for _ in 0..3 {
                let c = client.clone();
                let working = working.clone();
                pending.push(tokio::spawn(async move {
                    if legacy {
                        let response = c
                            .request("GET", "api/libraries/a/items")
                            .unwrap()
                            .send(hyper::body::Bytes::new())
                            .await
                            .unwrap();
                        let _ = response
                            .read_with(move |reader| {
                                working.blocking_send(()).unwrap();
                                decode(
                                    reader,
                                    WindowSeed {
                                        path: &["results"],
                                        start: 0,
                                        limit: 2,
                                        rows: Rows::Items,
                                    },
                                )
                            })
                            .await;
                    } else {
                        let _ = c
                            .browse_page_validated_async(
                                "a",
                                Media::Book,
                                &super::super::Section::Books,
                                0,
                                2,
                                None,
                            )
                            .await;
                    }
                }));
            }
            let entered_all = tokio::time::timeout(Duration::from_secs(2), async {
                for _ in 0..3 {
                    assert!(entered.recv().await.is_some());
                }
                if legacy {
                    for _ in 0..3 {
                        assert!(started.recv().await.is_some());
                    }
                }
            })
            .await
            .is_ok();
            let probe = tokio::time::timeout(
                if legacy {
                    Duration::from_millis(50)
                } else {
                    Duration::from_secs(1)
                },
                async {
                    let response = client
                        .request("GET", "api/probe")
                        .unwrap()
                        .send(hyper::body::Bytes::new())
                        .await
                        .unwrap();
                    response
                        .read_with(|mut reader| {
                            let mut text = String::new();
                            let _ = reader.read_to_string(&mut text);
                            text
                        })
                        .await
                        .unwrap()
                },
            )
            .await;
            for task in pending {
                task.abort();
                let _ = task.await;
            }
            outcomes.push(
                entered_all
                    && if legacy {
                        probe.is_err()
                    } else {
                        probe.is_ok_and(|text| text == "{}")
                    },
            );
        }
        outcomes.into_iter().all(|passed| passed)
    });
    stop.store(true, std::sync::atomic::Ordering::Release);
    let closed = server.join().unwrap();
    assert_eq!(closed, 6, "cancel must close every generated stalled body");
    assert!(
        ok,
        "stalled browsing must leave document decode capacity available"
    );
}

#[test]
fn episode_metadata_survives_fragmented_stream_projection() {
    let body=br#"{"results":[{"id":"e","libraryItemId":"show","title":"The Saga of Sagas","publishedAt":1723593600000,"season":"4","episode":21,"duration":7077.1,"audioFile":{"duration":7077.1}}]}"#;
    for width in 1..=body.len() { compare(body, plan(Rows::Episodes, 0), width); }
    let mut parser=Parser::window(plan(Rows::Episodes, 0));
    for byte in body.chunks(1) { parser.feed(byte).unwrap(); }
    let row=parser.finish().unwrap().rows.remove(0);
    let projected=entry(row,Rows::Episodes,Media::Podcast,"a",None).unwrap();
    assert_eq!(projected.detail,"August 14, 2024 · S4 E21 · 1:57:57");
}
