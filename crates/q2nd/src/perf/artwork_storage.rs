//! Generated cache workload. Run each backend in a separate process, with
//! distinct scratch directories. Reopen modes only read the previous fixture.
use super::Spec;
use crate::config::{Config, Paths};
use crate::gfx::Image;
use crate::library::{
    LibEvent, Library,
    art::{ArtCache, NOW_PLAYING_SIZE, THUMBNAIL_SIZE},
    art_pack::ArtPack,
};
use crate::rt::Blocking;
use crate::shard::{self, Store};
use md5::{Digest, Md5};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::Path,
    sync::{Arc, mpsc},
    time::{Duration, Instant},
};

const SIZES: [u32; 2] = [THUMBNAIL_SIZE, NOW_PLAYING_SIZE];
const CLUSTER: u64 = 256 * 1024;
const MANIFEST: &str = "fixture.json";

fn err(error: impl std::fmt::Display) -> String {
    error.to_string()
}

enum Backend {
    Loose(Paths),
    Pack(ArtPack),
}

impl Backend {
    fn new(packed: bool, paths: Paths) -> Self {
        if packed {
            Self::Pack(ArtPack::new(paths.art_cache()))
        } else {
            Self::Loose(paths)
        }
    }

    fn get(&self, blocking: &Blocking, key: &str) -> Result<Option<Vec<u8>>, String> {
        match self {
            Self::Pack(pack) => pack.get(blocking, key),
            Self::Loose(paths) => {
                let path = paths.sharded(Store::Art, format!("{key}.png").as_ref());
                paths.store_complete(blocking, Store::Art);
                for candidate in paths.candidates(&path) {
                    match fs::read(candidate) {
                        Ok(bytes) => return Ok(Some(bytes)),
                        Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                        Err(error) => return Err(err(error)),
                    }
                }
                Ok(None)
            }
        }
    }

    fn put(&self, blocking: &Blocking, rows: &[(&str, &[u8])]) -> Result<(), String> {
        match self {
            Self::Pack(pack) => pack.put(blocking, rows),
            Self::Loose(paths) => {
                for &(key, bytes) in rows {
                    let guard = paths.lock_store(blocking, Store::Art);
                    let path = paths
                        .prepare_sharded(blocking, &guard, format!("{key}.png").as_ref())
                        .map_err(err)?;
                    let temporary = path.with_extension(format!("sync-{:016x}", fastrand::u64(..)));
                    let mut file = OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .mode(0o600)
                        .custom_flags(libc::O_NOFOLLOW)
                        .open(&temporary)
                        .map_err(err)?;
                    let result = (|| {
                        file.write_all(bytes)?;
                        file.sync_all()?;
                        shard::rename(blocking, &temporary, &path)?;
                        drop(guard);
                        paths.sync_sharded(blocking, &path)
                    })();
                    if result.is_err() {
                        let _ = shard::remove(blocking, &temporary);
                    }
                    result.map_err(err)?;
                }
                Ok(())
            }
        }
    }
}

#[derive(Serialize, Deserialize)]
struct Cover {
    key: String,
    bytes: usize,
    md5: [u8; 16],
}

type Album = [Cover; 2];

const NAMESPACE: &str = "storage-bench";

fn cover_id(album: usize) -> String {
    format!("generated-{album}")
}

fn cover_key(album: usize, variant: usize) -> String {
    // Match the production JSON key: namespace, externally tagged source, size.
    let key = json!([NAMESPACE, {"Cover": cover_id(album)}, SIZES[variant]]);
    format!(
        "{:x}",
        Md5::digest(serde_json::to_vec(&key).expect("fixture key"))
    )
}

fn elapsed_ns(start: Instant) -> u64 {
    start.elapsed().as_nanos() as u64
}

#[derive(Default)]
struct Latency(Vec<u64>);

impl Latency {
    fn push(&mut self, ns: u64) {
        self.0.push(ns);
    }

    fn report(&self) -> Value {
        let mut ordered = self.0.clone();
        ordered.sort_unstable();
        let percentile = |p: usize| {
            let index = (p * ordered.len()).div_ceil(100).saturating_sub(1);
            ordered.get(index).copied()
        };
        json!({"count": ordered.len(), "total_ns": ordered.iter().sum::<u64>(),
            "p50_ns": percentile(50), "p95_ns": percentile(95),
            "p99_ns": percentile(99), "max_ns": ordered.last()})
    }
}

fn next_random(state: &mut u32) -> u32 {
    *state ^= *state << 13;
    *state ^= *state >> 17;
    *state ^= *state << 5;
    *state
}

fn original(album: usize) -> Image {
    // Bands, line art, gradients, and several strengths of fine texture.
    // This covers simple graphic sleeves and more detailed illustrations.
    let (width, height) = (192u32, 176u32);
    let mut state = (album as u32 + 1).wrapping_mul(0x9e3779b9);
    let hue = next_random(&mut state);
    let grain_mask = [0, 3, 15, 63][album % 4];
    let mut pixels = Vec::with_capacity((width * height) as usize);
    for y in 0..height {
        for x in 0..width {
            let dx = x.abs_diff(68 + hue % 54);
            let dy = y.abs_diff(65 + (hue >> 8) % 40);
            let disc = if dx * dx + dy * dy < 1900 { 85 } else { 0 };
            let bars = if y > 140 && (x + hue % 11) % 17 < 12 {
                39
            } else {
                0
            };
            let grain = next_random(&mut state) & grain_mask;
            let r = ((hue & 255) + x / 2 + disc + bars + grain) & 255;
            let g = ((hue >> 8) + (x + y) / 4 + disc / 2 + bars + grain / 2) & 255;
            let b = ((hue >> 16) + y / 2 + disc / 3 + bars + grain / 3) & 255;
            pixels.push((r << 16) | (g << 8) | b);
        }
    }
    Image {
        width,
        height,
        pixels,
    }
}

#[derive(Default)]
struct Writes {
    normalize_large: Latency,
    encode_large: Latency,
    normalize_thumbnail: Latency,
    encode_thumbnail: Latency,
    store_pair: Latency,
}

impl Writes {
    fn album(
        &mut self,
        backend: &Backend,
        blocking: &Blocking,
        album: usize,
    ) -> Result<Album, String> {
        let source = original(album).png().ok_or("encode fixture source")?;
        let started = Instant::now();
        let normalized =
            Image::decode_square(&source, NOW_PLAYING_SIZE).ok_or("normalize source cover")?;
        self.normalize_large.push(elapsed_ns(started));
        let started = Instant::now();
        let large = normalized.png().ok_or("encode normalized cover")?;
        self.encode_large.push(elapsed_ns(started));
        let started = Instant::now();
        let normalized = match backend {
            Backend::Loose(_) => Image::decode_square(&large, THUMBNAIL_SIZE),
            Backend::Pack(_) => normalized.thumbnail(THUMBNAIL_SIZE),
        }
        .ok_or("normalize thumbnail")?;
        self.normalize_thumbnail.push(elapsed_ns(started));
        let started = Instant::now();
        let thumbnail = normalized.png().ok_or("encode thumbnail")?;
        self.encode_thumbnail.push(elapsed_ns(started));

        let encoded = [thumbnail.as_slice(), large.as_slice()];
        let references: [(usize, [u8; 16]); 2] =
            encoded.map(|bytes| (bytes.len(), Md5::digest(bytes).into()));
        let keys = [cover_key(album, 0), cover_key(album, 1)];
        let started = Instant::now();
        // A synced cover publishes both sizes. The loose backend syncs each
        // file; the candidate uses one transaction for the pair.
        backend.put(blocking, &[(&keys[1], encoded[1]), (&keys[0], encoded[0])])?;
        self.store_pair.push(elapsed_ns(started));
        let [thumbnail, large] = keys;
        let [(thumb_bytes, thumb_md5), (large_bytes, large_md5)] = references;
        Ok([
            Cover {
                key: thumbnail,
                bytes: thumb_bytes,
                md5: thumb_md5,
            },
            Cover {
                key: large,
                bytes: large_bytes,
                md5: large_md5,
            },
        ])
    }

    fn report(&self) -> Value {
        json!({
            "normalize_large": self.normalize_large.report(), "encode_large": self.encode_large.report(),
            "normalize_thumbnail": self.normalize_thumbnail.report(), "encode_thumbnail": self.encode_thumbnail.report(),
            "store_synced_pair": self.store_pair.report(),
        })
    }
}

fn read(
    backend: &Backend,
    blocking: &Blocking,
    cover: &Cover,
    size: Option<u32>,
) -> Result<u64, String> {
    let started = Instant::now();
    let bytes = backend
        .get(blocking, &cover.key)?
        .ok_or("expected artwork missing")?;
    if let Some(size) = size {
        let image = Image::decode_square(&bytes, size).ok_or("cached cover did not decode")?;
        std::hint::black_box(image.pixels.as_slice());
    }
    let elapsed = elapsed_ns(started);
    if bytes.len() != cover.bytes || <[u8; 16]>::from(Md5::digest(&bytes)) != cover.md5 {
        return Err(format!("unexpected encoded bytes for {}", cover.key));
    }
    Ok(elapsed)
}

fn bursts(
    backend: &Backend,
    blocking: &Blocking,
    albums: &[Album],
    variant: usize,
    width: usize,
) -> Result<Value, String> {
    let count = 24;
    let mut individual = Latency::default();
    let mut burst = Latency::default();
    for batch in 0..count {
        let mut total = 0;
        for offset in 0..width {
            let album = &albums[(batch * 41 + offset) % albums.len()];
            let elapsed = read(backend, blocking, &album[variant], Some(SIZES[variant]))?;
            individual.push(elapsed);
            total += elapsed;
        }
        burst.push(total);
    }
    Ok(
        json!({"request": individual.report(), "burst": burst.report(), "requests_per_burst": width}),
    )
}

fn lookup(
    backend: &Backend,
    blocking: &Blocking,
    albums: &[Album],
    operations: usize,
) -> Result<Value, String> {
    let mut rng = 0x19681231;
    let mut hit = Latency::default();
    let mut hit_decoded = Latency::default();
    let mut miss = Latency::default();
    for index in 0..operations {
        let album = next_random(&mut rng) as usize % albums.len();
        let variant = (next_random(&mut rng) % 2) as usize;
        if index % 8 == 0 {
            let key = cover_key(20_000 + index, variant);
            let start = Instant::now();
            if backend.get(blocking, &key)?.is_some() {
                return Err("unexpected cache hit".into());
            }
            miss.push(elapsed_ns(start));
        } else if index % 4 == 1 {
            hit_decoded.push(read(
                backend,
                blocking,
                &albums[album][variant],
                Some(SIZES[variant]),
            )?);
        } else {
            hit.push(read(backend, blocking, &albums[album][variant], None)?);
        }
    }
    Ok(
        json!({"encoded_hit": hit.report(), "decoded_hit": hit_decoded.report(), "miss": miss.report()}),
    )
}

#[derive(Default, Serialize)]
struct Footprint {
    files: u64,
    directories: u64,
    logical_file_bytes: u64,
    filesystem_allocated_bytes: u64,
    modeled_exfat_file_bytes: u64,
    modeled_exfat_directory_bytes: u64,
    journal_logical_bytes: u64,
    journal_allocated_bytes: u64,
}

fn footprint(blocking: &Blocking, root: &Path) -> io::Result<Value> {
    fn walk(blocking: &Blocking, path: &Path, stats: &mut Footprint) -> io::Result<()> {
        let metadata = fs::symlink_metadata(path)?;
        stats.filesystem_allocated_bytes += metadata.blocks() * 512;
        if metadata.is_dir() {
            stats.directories += 1;
            let mut entries = 2; // room for directory bookkeeping
            for child in fs::read_dir(path)? {
                let child = child?;
                // File entry, stream extension, and up to 15 UTF-16 units
                // per name entry. All generated names are ASCII.
                entries += 2 + child.file_name().len().div_ceil(15) as u64;
                walk(blocking, &child.path(), stats)?;
            }
            stats.modeled_exfat_directory_bytes += (entries * 32).div_ceil(CLUSTER) * CLUSTER;
        } else if metadata.is_file() {
            stats.files += 1;
            stats.logical_file_bytes += metadata.len();
            if path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| {
                    name.ends_with("-wal") || name.ends_with("-shm") || name.ends_with("-journal")
                })
            {
                stats.journal_logical_bytes += metadata.len();
                stats.journal_allocated_bytes += metadata.blocks() * 512;
            }
            stats.modeled_exfat_file_bytes += metadata.len().div_ceil(CLUSTER) * CLUSTER;
        } else {
            return Err(io::Error::other("unexpected artwork cache entry"));
        }
        Ok(())
    }

    let mut stats = Footprint::default();
    walk(blocking, root, &mut stats)?;
    Ok(
        json!({"all_cache_entries": stats, "modeled_cluster_bytes": CLUSTER,
        "modeled_exfat_total_bytes": stats.modeled_exfat_file_bytes + stats.modeled_exfat_directory_bytes,
        "filesystem_scope": "cache/art tree including present SQLite sidecars and directories; excludes global filesystem overhead"}),
    )
}

fn first_use(
    packed: bool,
    paths: Paths,
    blocking: &Blocking,
    albums: &[Album],
) -> Result<(Backend, Value), String> {
    let cover = &albums[albums.len() / 2][1];
    let started = Instant::now();
    let backend = Backend::new(packed, paths);
    let open_ns = elapsed_ns(started);
    let cover_ns = read(&backend, blocking, cover, Some(NOW_PLAYING_SIZE))?;
    Ok((
        backend,
        json!({"new_handle_ns": open_ns, "first_read_decode_ns": cover_ns,
        "new_handle_to_useful_cover_ns": open_ns + cover_ns}),
    ))
}

fn verify(backend: &Backend, blocking: &Blocking, albums: &[Album]) -> Result<Value, String> {
    let mut payload_bytes = [0u64; 2];
    let mut corpus = Md5::new();
    for album in albums {
        for variant in 0..2 {
            let cover = &album[variant];
            read(backend, blocking, cover, None)?;
            payload_bytes[variant] += cover.bytes as u64;
            corpus.update(cover.key.as_bytes());
            corpus.update(cover.md5);
        }
    }
    Ok(
        json!({"albums": albums.len(), "verified_covers": 2 * albums.len(),
        "thumbnail_payload_bytes": payload_bytes[0], "large_payload_bytes": payload_bytes[1],
        "verified_corpus_md5": format!("{:x}", corpus.finalize())}),
    )
}

pub(super) fn run(blocking: &Blocking, spec: &Spec) -> Result<Value, String> {
    let (packed, reopen_only) = match spec.scene.as_str() {
        "loose" => (false, false),
        "pack" => (true, false),
        "loose-reopen" => (false, true),
        "pack-reopen" => (true, true),
        _ => {
            return Err(
                "artwork-storage scene must be loose, pack, loose-reopen or pack-reopen".into(),
            );
        }
    };
    let paths = Paths::new(spec.scratch.clone());
    let root = paths.art_cache();
    let operations = (spec.iterations * 2).clamp(32, 4096);
    let mixed_albums = spec.iterations.clamp(4, 64);
    if reopen_only {
        let albums = read_manifest(blocking, spec)?;
        let (backend, first) = first_use(packed, paths, blocking, &albums[..spec.iterations])?;
        let random = lookup(&backend, blocking, &albums[..spec.iterations], operations)?;
        let checked = verify(&backend, blocking, &albums)?;
        let active = footprint(blocking, &root).map_err(err)?;
        drop(backend);
        return Ok(
            json!({"scene": spec.scene, "first_use": first, "random_reopened": random,
            "verification": checked, "active_footprint": active, "settled_footprint": footprint(blocking, &root).map_err(err)?,
            "cache_state": "fresh process; no kernel cache operation performed by benchmark"}),
        );
    }

    // Never write into an old or user-owned dataset.
    fs::create_dir(&spec.scratch).map_err(err)?;
    fs::create_dir_all(&root).map_err(err)?;
    if !packed {
        paths.mark_complete(blocking, Store::Art).map_err(err)?;
    }
    let mut albums = Vec::with_capacity(spec.iterations + mixed_albums);
    let mut seed = Writes::default();
    let backend = Backend::new(packed, paths.clone());
    for album in 0..spec.iterations {
        albums.push(seed.album(&backend, blocking, album)?);
    }
    drop(backend);

    let (backend, first) = first_use(packed, paths.clone(), blocking, &albums)?;
    let thumbnails = bursts(&backend, blocking, &albums, 0, 12)?;
    let cover_flow = bursts(&backend, blocking, &albums, 1, 7)?;
    let warm = lookup(&backend, blocking, &albums, operations)?;

    let mut mixed = Writes::default();
    let mut during_sync = Latency::default();
    let initial_footprint = footprint(blocking, &root).map_err(err)?;
    for index in 0..mixed_albums {
        for offset in 0..12 {
            let variant = offset % 2;
            let album = (index * 47 + offset) % albums.len();
            during_sync.push(read(
                &backend,
                blocking,
                &albums[album][variant],
                Some(SIZES[variant]),
            )?);
        }
        albums.push(mixed.album(&backend, blocking, spec.iterations + index)?);
    }
    // Measure after the final transaction while the backend is still open.
    let active_footprint = footprint(blocking, &root).map_err(err)?;
    drop(backend);

    let backend = Backend::new(packed, paths);
    let reopened = lookup(&backend, blocking, &albums[..spec.iterations], operations)?;
    let checked = verify(&backend, blocking, &albums)?;
    drop(backend);
    let file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(spec.scratch.join(MANIFEST))
        .map_err(err)?;
    let mut manifest = io::BufWriter::new(file);
    serde_json::to_writer(&mut manifest, &albums).map_err(err)?;
    manifest.flush().map_err(err)?;

    Ok(
        json!({"scene": spec.scene, "initial_albums": spec.iterations,
        "first_use": first, "thumbnail_bursts": thumbnails, "cover_flow_bursts": cover_flow,
        "random_warm": warm, "random_reopened": reopened,
        "setup_writes": seed.report(), "mixed_synced_writes": mixed.report(),
        "mixed_decoded_reads": during_sync.report(), "verification": checked,
        "initial_footprint": initial_footprint, "active_footprint": active_footprint, "settled_footprint": footprint(blocking, &root).map_err(err)?,
        "cache_state": "no decoded-image LRU; storage handles reopened; OS page, inode and directory caches untouched"}),
    )
}

fn read_manifest(_blocking: &Blocking, spec: &Spec) -> Result<Vec<Album>, String> {
    let file = File::open(spec.scratch.join(MANIFEST)).map_err(err)?;
    let albums: Vec<Album> = serde_json::from_reader(io::BufReader::new(file)).map_err(err)?;
    if albums.len() != spec.iterations + spec.iterations.clamp(4, 64) {
        return Err("fixture album count differs from spec".into());
    }
    Ok(albums)
}

const TRACKS: usize = 12;
const UNIQUE_TRACK: usize = 9;

fn sized_images(seed: usize) -> Result<[Image; 2], String> {
    let source = original(seed).png().ok_or("encode fixture source")?;
    let large = Image::decode_square(&source, NOW_PLAYING_SIZE).ok_or("normalize fixture cover")?;
    let thumbnail = large
        .thumbnail(THUMBNAIL_SIZE)
        .ok_or("normalize fixture thumbnail")?;
    Ok([thumbnail, large])
}

fn image_digest(image: &Image) -> [u8; 16] {
    let mut hash = Md5::new();
    hash.update(image.width.to_le_bytes());
    hash.update(image.height.to_le_bytes());
    for pixel in &image.pixels {
        hash.update(pixel.to_le_bytes());
    }
    hash.finalize().into()
}

/// Seed distinct per-track keys for two artwork sizes, then browse and play
/// those keys through the normal asynchronous artwork reader.
pub(super) fn run_aliases(blocking: &Blocking, spec: &Spec) -> Result<Value, String> {
    if spec.scene != "pack" || !(12..=512).contains(&spec.iterations) {
        return Err("artwork-aliases requires scene pack and 12 to 512 albums".into());
    }
    fs::create_dir(&spec.scratch).map_err(err)?;
    let paths = Paths::new(spec.scratch.clone());
    let root = paths.art_cache();
    fs::create_dir_all(&root).map_err(err)?;

    let (mut first_write, mut alias_write, mut unique_write) =
        (Latency::default(), Latency::default(), Latency::default());
    let mut logical_bytes = [0u64; 2];
    let mut distinct_bytes = [0u64; 2];
    let mut corpus = Md5::new();
    let mut expected_pixels = Vec::with_capacity(spec.iterations);
    let backend = Backend::new(true, paths.clone());
    for album in 0..spec.iterations {
        let first_id = album * TRACKS;
        let images = [sized_images(album * 2)?, sized_images(album * 2 + 1)?];
        let mut encoded = [[Vec::new(), Vec::new()], [Vec::new(), Vec::new()]];
        let mut encoded_digests = [[[0u8; 16]; 2]; 2];
        for (index, pair) in images.iter().enumerate() {
            for (variant, image) in pair.iter().enumerate() {
                let bytes = image.png().ok_or("encode fixture PNG")?;
                distinct_bytes[variant] += bytes.len() as u64;
                encoded_digests[index][variant] = Md5::digest(&bytes).into();
                encoded[index][variant] = bytes;
            }
        }
        expected_pixels.push(images.map(|pair| pair.each_ref().map(image_digest)));
        for track in 0..TRACKS {
            let index = usize::from(track == UNIQUE_TRACK);
            let keys = [
                cover_key(first_id + track, 0),
                cover_key(first_id + track, 1),
            ];
            let [small, large] = &encoded[index];
            let start = Instant::now();
            backend.put(blocking, &[(&keys[1], large), (&keys[0], small)])?;
            let ns = elapsed_ns(start);
            match track {
                0 => first_write.push(ns),
                UNIQUE_TRACK => unique_write.push(ns),
                _ => alias_write.push(ns),
            }
            for (variant, key) in keys.iter().enumerate() {
                logical_bytes[variant] += encoded[index][variant].len() as u64;
                corpus.update(key.as_bytes());
                corpus.update(encoded_digests[index][variant]);
            }
        }
    }
    let active = footprint(blocking, &root).map_err(err)?;
    drop(backend);
    let settled = footprint(blocking, &root).map_err(err)?;

    let (send, receive) = mpsc::channel();
    let (lib, _) = Library::create(
        paths,
        &Config::default(),
        Arc::new(move |event| {
            if matches!(event, LibEvent::ArtReady) {
                let _ = send.send(());
            }
        }),
    );
    lib.art.set_namespace(NAMESPACE);
    ArtCache::start(&lib);
    let mut trace = Md5::new();
    let mut request = |id: usize, variant: usize, expected: [u8; 16]| -> Result<u64, String> {
        let id = cover_id(id);
        let size = SIZES[variant];
        let start = Instant::now();
        if lib.art.get(&id, size).is_some() {
            return Err(format!("expected new worker request for {id} at {size}"));
        }
        receive
            .recv_timeout(Duration::from_secs(10))
            .map_err(|error| format!("worker did not publish {id} at {size}: {error}"))?;
        let image = lib
            .art
            .peek_cover(&id, size)
            .ok_or("worker published a different cover")?;
        let ns = elapsed_ns(start);
        if image_digest(&image) != expected {
            return Err(format!("incorrect decoded pixels for {id} at {size}"));
        }
        trace.update(id.as_bytes());
        trace.update(size.to_le_bytes());
        trace.update(expected);
        Ok(ns)
    };

    let (mut browse, mut first_play, mut alias_play, mut unique_play, mut track_list) = (
        Latency::default(),
        Latency::default(),
        Latency::default(),
        Latency::default(),
        Latency::default(),
    );
    for (album, pixels) in expected_pixels.iter().enumerate() {
        browse.push(request(album * TRACKS, 0, pixels[0][0])?);
    }
    for (album, pixels) in expected_pixels.iter().enumerate() {
        for track in 1..TRACKS {
            let id = album * TRACKS + track;
            let expected = pixels[usize::from(track == UNIQUE_TRACK)];
            track_list.push(request(id, 0, expected[0])?);
        }
        for track in 0..TRACKS {
            let id = album * TRACKS + track;
            let expected = pixels[usize::from(track == UNIQUE_TRACK)];
            let ns = request(id, 1, expected[1])?;
            match track {
                0 => first_play.push(ns),
                UNIQUE_TRACK => unique_play.push(ns),
                _ => alias_play.push(ns),
            }
        }
    }

    Ok(json!({
        "scene": spec.scene, "albums": spec.iterations, "tracks_per_album": TRACKS,
        "aliases_per_album": TRACKS - 2, "extra_unique_tracks_per_album": 1,
        "verified_worker_images": 2 * TRACKS * spec.iterations,
        "corpus_md5": format!("{:x}", corpus.finalize()),
        "verified_request_trace_md5": format!("{:x}", trace.finalize()),
        "sizes_px": SIZES, "logical_png_bytes_by_size": logical_bytes,
        "distinct_png_bytes_by_size": distinct_bytes,
        "writes_first_pair": first_write.report(), "writes_alias_pair": alias_write.report(),
        "writes_unique_track_pair": unique_write.report(),
        "worker_album_list_thumbnail": browse.report(), "worker_track_list_thumbnail": track_list.report(),
        "worker_first_now_playing": first_play.report(), "worker_alias_now_playing": alias_play.report(),
        "worker_unique_track_now_playing": unique_play.report(),
        "active_footprint": active, "settled_footprint": settled,
        "cache_state": "worker starts with empty decoded LRU after pack close; kernel caches untouched by benchmark",
    }))
}

/// The same request shape as a stationary Cover Flow window with forward
/// prefetch: centre and six on either side visible, another eight ahead.
pub(super) fn run_worker(blocking: &Blocking, spec: &Spec) -> Result<Value, String> {
    const BEHIND: usize = 6;
    const VISIBLE: usize = 2 * BEHIND + 1;
    const ALL: usize = VISIBLE + 8;
    const DEADLINE: Duration = Duration::from_secs(10);
    if spec.iterations < ALL || !matches!(spec.scene.as_str(), "loose" | "pack") {
        return Err(
            "artwork-worker needs a loose or pack fixture with at least 21 initial albums".into(),
        );
    }
    let albums = read_manifest(blocking, spec)?;
    let paths = Paths::new(spec.scratch.clone());
    let root = paths.art_cache();
    let (send, receive) = mpsc::channel();
    let (lib, _) = Library::create(
        paths,
        &Config::default(),
        Arc::new(move |event| {
            if matches!(event, LibEvent::ArtReady) {
                let _ = send.send(());
            }
        }),
    );
    lib.art.set_namespace(NAMESPACE);

    let first = (spec.iterations - ALL) / 2;
    let ids: Vec<_> = (first..first + ALL).map(cover_id).collect();
    if ids
        .iter()
        .any(|id| lib.art.peek_cover(id, NOW_PLAYING_SIZE).is_some())
    {
        return Err("artwork worker started with decoded covers in memory".into());
    }
    ArtCache::start(&lib);
    let started = Instant::now();
    let deadline = started + DEADLINE;
    // The renderer draws the edge covers first, then the center on top.
    // Keep that order so the center is the final visible request queued.
    let mut visible_order: [usize; VISIBLE] = std::array::from_fn(|index| index);
    visible_order.sort_by_key(|&index| std::cmp::Reverse(index.abs_diff(BEHIND)));
    lib.art.frame_covers(visible_order.map(|index| Some(ids[index].as_str())), NOW_PLAYING_SIZE);

    // Match a forward turn: warm the center, then the next and previous
    // visible covers in pairs, followed by the eight ahead of the frame.
    let window = (0..ALL - BEHIND).flat_map(|distance| {
        [Some(BEHIND + distance), BEHIND.checked_sub(distance).filter(|_| distance > 0)]
    }).flatten();
    lib.art.warm_window(window.map(|index| ids[index].as_str()), NOW_PLAYING_SIZE);
    let issued_ns = elapsed_ns(started);

    let mut readiness = [None; ALL];
    let mut ready_events = 0;
    while ready_events < ALL {
        let remaining = deadline.saturating_duration_since(Instant::now());
        receive.recv_timeout(remaining).map_err(|error| {
            format!("artwork worker stopped after {ready_events}/{ALL} ready events: {error}")
        })?;
        ready_events += 1;
        for (index, id) in ids.iter().enumerate() {
            if readiness[index].is_none() && lib.art.peek_cover(id, NOW_PLAYING_SIZE).is_some() {
                readiness[index] = Some(elapsed_ns(started));
            }
        }
    }
    let ready = readiness
        .into_iter()
        .collect::<Option<Vec<_>>>()
        .ok_or("artwork worker notified without all covers available in memory")?;

    // Check the worker's actual decoded pixels after measuring the window.
    let mut hash = Md5::new();
    for (index, id) in ids.iter().enumerate() {
        let album = first + index;
        let large = lib
            .art
            .peek_cover(id, NOW_PLAYING_SIZE)
            .ok_or("window cover evicted before verification")?;
        let source = original(album).png().ok_or("encode fixture source")?;
        let expected =
            Image::decode_square(&source, NOW_PLAYING_SIZE).ok_or("normalize fixture image")?;
        let cached = expected.png().ok_or("encode expected cached cover")?;
        if (large.width, large.height) != (expected.width, expected.height)
            || large.pixels != expected.pixels
            || <[u8; 16]>::from(Md5::digest(&cached)) != albums[album][1].md5
        {
            return Err(format!("incorrect decoded artwork for {id}"));
        }
        hash.update(albums[album][1].md5);
    }
    Ok(json!({
        "scene": spec.scene,
        "ready_events": ready_events,
        "verified_covers": ALL,
        "verified_window_md5": format!("{:x}", hash.finalize()),
        "request_submit_ns": issued_ns,
        "first_ready_ns": ready.iter().min(),
        "centre_ready_ns": ready[BEHIND],
        "visible_ready_ns": ready[..VISIBLE].iter().max(),
        "window_ready_ns": ready.iter().max(),
        "per_cover_ready_ns": ready,
        "active_footprint": footprint(blocking, &root).map_err(err)?,
        "cache_state": "new library and decoded-image LRU; artwork worker only; kernel caches untouched by benchmark"
    }))
}

/// Validate the complete generated loose tree before enabling a destructive
/// benchmark. Unknown entries or a fixture from a prior run reject admission.
fn admit_migration(
    blocking: &Blocking,
    spec: &Spec,
    paths: &Paths,
) -> Result<(Vec<Album>, Value), String> {
    use std::collections::BTreeMap;
    use std::ffi::OsStr;

    fn entries(
        _blocking: &Blocking,
        directory: &Path,
        expected: usize,
    ) -> Result<Vec<fs::DirEntry>, String> {
        if !fs::symlink_metadata(directory).map_err(err)?.is_dir() {
            return Err("migration fixture contains a non-directory".into());
        }
        let mut entries = fs::read_dir(directory)
            .map_err(err)?
            .take(expected + 1)
            .collect::<io::Result<Vec<_>>>()
            .map_err(err)?;
        if entries.len() != expected {
            return Err("migration fixture has an unexpected directory entry count".into());
        }
        entries.sort_unstable_by_key(fs::DirEntry::file_name);
        Ok(entries)
    }

    fn regular_file(_blocking: &Blocking, path: &Path, expected_len: u64) -> Result<(), String> {
        let metadata = fs::symlink_metadata(path).map_err(err)?;
        if !metadata.is_file() || metadata.len() != expected_len {
            return Err("migration fixture contains an unexpected file".into());
        }
        Ok(())
    }

    let scratch = entries(blocking, &spec.scratch, 2)?;
    if scratch[0].file_name() != "cache" || scratch[1].file_name() != MANIFEST {
        return Err("migration requires a fresh loose artwork-storage fixture".into());
    }
    let manifest = fs::symlink_metadata(scratch[1].path()).map_err(err)?;
    if !manifest.is_file() || manifest.len() > 4 * 1024 * 1024 {
        return Err("migration fixture manifest is invalid".into());
    }
    let cache = entries(blocking, &scratch[0].path(), 1)?;
    if cache[0].file_name() != "art" {
        return Err("migration fixture contains an unexpected cache".into());
    }

    let albums = read_manifest(blocking, spec)?;
    let mut shards = BTreeMap::<String, BTreeMap<String, u64>>::new();
    for (index, album) in albums.iter().enumerate() {
        for (variant, cover) in album.iter().enumerate() {
            if cover.key != cover_key(index, variant)
                || !(45..=5 * 1024 * 1024).contains(&cover.bytes)
            {
                return Err(
                    "migration fixture contains a non-generated artwork key or size".into(),
                );
            }
            let name = format!("{}.png", cover.key);
            shards
                .entry(shard::shard(OsStr::new(&name)))
                .or_default()
                .insert(name, cover.bytes as u64);
        }
    }
    let root = paths.art_cache();
    let mut directories = entries(blocking, &root, shards.len() + 1)?.into_iter();
    let marker = directories
        .next()
        .ok_or("migration fixture has no loose layout marker")?;
    let marker_bytes = b"sharded v1\n";
    if marker.file_name() != shard::MARKER {
        return Err("migration fixture has no loose layout marker".into());
    }
    regular_file(blocking, &marker.path(), marker_bytes.len() as u64)?;
    if fs::read(marker.path()).map_err(err)?.as_slice() != marker_bytes {
        return Err("migration fixture has the wrong layout marker".into());
    }
    for (directory, (shard_name, expected)) in directories.zip(shards) {
        if directory.file_name() != OsStr::new(&shard_name) {
            return Err("migration fixture contains an unowned directory".into());
        }
        for (entry, (name, len)) in entries(blocking, &directory.path(), expected.len())?
            .into_iter()
            .zip(expected)
        {
            if entry.file_name() != OsStr::new(&name) {
                return Err("migration fixture contains unowned artwork".into());
            }
            regular_file(blocking, &entry.path(), len)?;
        }
    }
    let verification = verify(&Backend::new(false, paths.clone()), blocking, &albums)?;
    Ok((albums, verification))
}

pub(super) fn run_migration(blocking: &Blocking, spec: &Spec) -> Result<Value, String> {
    if spec.scene != "loose" {
        return Err("artwork-migration requires a fresh loose artwork-storage fixture".into());
    }
    let paths = Paths::new(spec.scratch.clone());
    let (albums, before_verification) = admit_migration(blocking, spec, &paths)?;
    let root = paths.art_cache();
    let before_footprint = footprint(blocking, &root).map_err(err)?;
    let mut config = Config::default();
    config.storage.reserve_mb = 0;
    let (library, _) = Library::create(paths.clone(), &config, Arc::new(|_| {}));

    let ui = crate::rt::Ui::new().map_err(err)?;
    let started = Instant::now();
    let progress = ui
        .block_on(library.migrate_generated_artwork())
        .ok_or("generated migration timed out or lost its worker")?;
    let migration_ns = elapsed_ns(started);
    drop(library);
    let progress = json!({
        "moved": progress.moved, "gone": progress.gone, "failed": progress.failed,
        "conflicts": progress.conflicts, "stopped": progress.stopped,
    });
    if progress
        != json!({"moved": 2 * albums.len(), "gone": 0, "failed": 0, "conflicts": 0, "stopped": 0})
    {
        return Err(format!(
            "incomplete generated artwork migration after {migration_ns} ns: {progress}"
        ));
    }
    let packed = Backend::new(true, paths);
    let after_verification = verify(&packed, blocking, &albums)?;
    drop(packed);
    if before_verification != after_verification {
        return Err("migration changed the verified artwork corpus".into());
    }
    Ok(json!({
        "scene": spec.scene,
        "migration_ns": migration_ns,
        "progress": progress,
        "verification": after_verification,
        "before_footprint": before_footprint,
        "settled_footprint": footprint(blocking, &root).map_err(err)?,
        "cache_state": "fixture verified before timing; eight-row cap and 2 ms delay per job",
    }))
}
