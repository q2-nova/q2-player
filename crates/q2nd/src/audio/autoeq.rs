#![forbid(unsafe_code)]
//! A pinned recommended catalog; downloaded profiles remain useful offline.
use super::eq::Profile;
use std::path::Path;
use std::sync::Arc;
use std::ops::Range;
pub const REVISION: &str = "7ae0f56d53074872b028649617a22bbb4232feb7";
const ARCHIVE_REVISION: &str = "6282acb7846808bbe495568a56287e660d877d87";
#[cfg(test)]
const ARCHIVE_INDEX: &str = include_str!("../../assets/autoeq/custom-studio/index.tsv");
#[cfg(test)]
const INDEX: &str = include_str!("../../assets/autoeq/index.tsv");
const CATALOG_FILES: [(&str, &str); 2] = [
    ("index.tsv", "6027b8ee6df4092acc18d5207abd729c"),
    ("custom-studio/index.tsv", "51edf870d60b5aac9c2a08495c5f77d1"),
];
const ARCHIVE_FILES: [(&str, &str); 4] = [
    ("custom-studio/step-1.txt", "35d5f7a53670f87bad633df112c07826"),
    ("custom-studio/step-2.txt", "c8961e9f2c4d0ebfdd1944ab3bce1528"),
    ("custom-studio/step-3.txt", "1c1d8a9ae17425ccb7e213f3ba970308"),
    ("custom-studio/step-4.txt", "006a9bcc8e6638f25d4b3a706d166834"),
];
struct Catalog {
    text: String,
    directory: std::path::PathBuf,
    #[cfg(test)]
    fixture: bool,
}
#[derive(Clone)]
pub struct Entry { catalog: Arc<Catalog>, name: Range<usize>, path: Range<usize>, archived: Option<usize> }

fn read_packaged(blocking: &crate::rt::Blocking, directory: &Path, file: (&str, &str), limit: usize) -> Result<String, String> {
    use md5::{Digest, Md5};
    let bytes = crate::resources::read(blocking, &directory.join(file.0), limit)
        .map_err(|_| "Headphone resources are unavailable; restore the packaged files".to_string())?;
    // Version/corruption check, not an authenticity or security signature. The
    // image manifest separately binds SHA256. Never retune the historical LS/HS
    // exports by applying that correction to a changed resource file.
    if format!("{:x}", Md5::digest(&bytes)) != file.1 {
        return Err("Headphone resources differ from the packaged version".into());
    }
    String::from_utf8(bytes).map_err(|_| "Headphone resource text is invalid".into())
}

fn search_catalog(catalog: Arc<Catalog>, archived: bool, query: &str) -> Result<Vec<Entry>, String> {
    let query = query.to_lowercase();
    let mut offset = 0;
    let mut entries = Vec::new();
    for (row, line) in catalog.text.split_terminator('\n').enumerate() {
        let (name, path) = line.split_once('\t').ok_or("Headphone catalog row is invalid")?;
        if name.is_empty() || path.contains(['\t', '\r', '?', '#']) || path.contains("..")
            || path.split('/').count() < 3 || (archived && row >= ARCHIVE_FILES.len()) {
            return Err("Headphone catalog row is invalid".into());
        }
        if query.split_whitespace().all(|word| name.to_lowercase().contains(word)) {
            entries.push(Entry { catalog: catalog.clone(), name: offset..offset + name.len(),
                path: offset + name.len() + 1..offset + line.len(), archived: archived.then_some(row) });
        }
        offset += line.len() + 1;
    }
    Ok(entries)
}

fn search_from(blocking: &crate::rt::Blocking, directory: &Path, query: &str) -> Result<Vec<Entry>, String> {
    let mut entries = Vec::new();
    for (index, file) in CATALOG_FILES.into_iter().enumerate() {
        let catalog = Arc::new(Catalog { text: read_packaged(blocking, directory, file, 1024 * 1024)?,
            directory: directory.into(), #[cfg(test)] fixture: false });
        entries.extend(search_catalog(catalog, index == 1, query)?);
    }
    entries.sort_by(|a, b| a.name().cmp(b.name()));
    Ok(entries)
}

pub async fn search_async(query: String) -> Result<Vec<Entry>, String> {
    crate::rt::spawn_blocking(move |blocking| search_from(blocking, &crate::resources::directory("autoeq"), &query))
        .await.map_err(|_| "Headphone catalog worker stopped".to_string())?
}

/// The catalog model for a Bluetooth device (`eq_match::best`), if one matches.
pub async fn suggest_async(hint: super::eq_match::Hint) -> Result<Option<Entry>, String> {
    crate::rt::spawn_blocking(move |blocking| {
        let entries = search_from(blocking, &crate::resources::directory("autoeq"), "")?;
        let pairs: Vec<(&str, &str)> = entries.iter().map(|e| (e.name(), e.path())).collect();
        Ok(super::eq_match::best(&hint, &pairs).map(|index| entries[index].clone()))
    }).await.map_err(|_| "Headphone catalog worker stopped".to_string())?
}

/// The catalog entry at `path`, as a device's saved choice names it.
pub async fn find_async(path: String) -> Result<Option<Entry>, String> {
    crate::rt::spawn_blocking(move |blocking| {
        Ok(search_from(blocking, &crate::resources::directory("autoeq"), "")?.into_iter().find(|e| e.path() == path))
    }).await.map_err(|_| "Headphone catalog worker stopped".to_string())?
}

/// In-memory reference for UI hierarchy and legacy numeric fixtures. Production
/// uses the same parser with bounded packaged reads, exercised separately.
#[cfg(test)]
pub fn search(query: &str) -> Vec<Entry> {
    let mut entries = Vec::new();
    for (index, text) in [INDEX, ARCHIVE_INDEX].into_iter().enumerate() {
        entries.extend(search_catalog(Arc::new(Catalog { text: text.into(), directory: Default::default(), fixture: true }), index == 1, query).unwrap());
    }
    entries.sort_by(|a, b| a.name().cmp(b.name()));
    entries
}
impl Entry {
    pub fn name(&self) -> &str { &self.catalog.text[self.name.clone()] }
    pub fn path(&self) -> &str { &self.catalog.text[self.path.clone()] }
    fn archived_text(&self, blocking: &crate::rt::Blocking) -> Result<Option<String>, String> {
        let Some(index) = self.archived else { return Ok(None); };
        #[cfg(test)]
        if self.catalog.fixture {
            return Ok(Some([
                include_str!("../../assets/autoeq/custom-studio/step-1.txt"),
                include_str!("../../assets/autoeq/custom-studio/step-2.txt"),
                include_str!("../../assets/autoeq/custom-studio/step-3.txt"),
                include_str!("../../assets/autoeq/custom-studio/step-4.txt"),
            ][index].into()));
        }
        read_packaged(blocking, &self.catalog.directory, ARCHIVE_FILES[index], 16384).map(Some)
    }
    pub fn url(&self) -> String {
        let file=self.path().rsplit('/').next().unwrap();
        let revision=if self.archived.is_some() {ARCHIVE_REVISION} else {REVISION};
        format!("https://raw.githubusercontent.com/jaakkopasanen/AutoEq/{revision}/results/{}/{file}%20ParametricEQ.txt",self.path())
    }
    fn cache_name(&self) -> String {
        use md5::{Digest, Md5};
        format!("{:x}.txt",Md5::digest(self.url().as_bytes()))
    }
    fn cached(&self, blocking: &crate::rt::Blocking, root: &Path) -> Result<Option<Profile>,String> {
        if let Some(raw)=self.archived_text(blocking)? {
            // AutoEq issue #586: historical shelf export labels were incorrect.
            // Scope the documented correction to these exact bundled exports;
            // arbitrary imports retain the strict LSC/HSC parser. No retuning.
            let text=raw.replace("ON LS Fc", "ON LSC Fc").replace("ON HS Fc", "ON HSC Fc");
            return Profile::parse(self.name().into(),self.url(),&text).map(Some);
        }
        let dir=root.join("autoeq");
        let path=dir.join(self.cache_name());
        // Bound local reads too: this directory is user-writable on the card.
        if let Ok(file)=std::fs::File::open(&path) {
            use std::io::Read;
            let mut text=String::new();
            if file.take(16385).read_to_string(&mut text).is_ok() && text.len()<=16384 {
                if let Ok(p)=Profile::parse(self.name().into(),self.url(),&text) { return Ok(Some(p)); }
            }
        }
        Ok(None)
    }
    pub async fn load_async(self, root: std::path::PathBuf, offline: bool, proxy: Option<String>) -> Result<Profile,String> {
        let entry=self.clone();let cached_root=root.clone();
        if let Some(profile)=crate::rt::spawn_blocking(move |blocking| entry.cached(blocking, &cached_root))
            .await.map_err(|_|"AutoEQ cache unavailable")?? { return Ok(profile); }
        if offline { return Err("Profile not saved; connect to download".into()); }
        let text=subsonic::autoeq::fetch_async(&self.url(),proxy.as_deref()).await?;
        // Parsing and durable card publication are finite local work. No pool
        // worker is held while the public download awaits headers or body data.
        crate::rt::spawn_blocking(move |blocking| {
            let profile=Profile::parse(self.name().into(),self.url(),&text)?;
            let dir=root.join("autoeq");let path=dir.join(self.cache_name());
            std::fs::create_dir_all(&dir).map_err(|_|"Cannot save AutoEQ profile")?;
            crate::config::write_atomic(blocking, &path,text.as_bytes()).map_err(|_|"Cannot save AutoEQ profile")?;
            Ok(profile)
        }).await.map_err(|_|"Cannot save AutoEQ profile")?
    }
    #[cfg(test)]
    pub fn load(&self, root: &Path, offline: bool, proxy: Option<&str>) -> Result<Profile,String> {
        let entry=self.clone();let root=root.to_path_buf();let proxy=proxy.map(str::to_owned);
        let (tx,rx)=std::sync::mpsc::sync_channel(1);
        crate::rt::io().spawn(async move {let _=tx.send(entry.load_async(root,offline,proxy).await);});
        rx.recv().map_err(|_|"AutoEQ task stopped")?
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn packaged_catalog_and_profiles_match_reference_and_refuse_changed_or_missing_data() {
        let root = crate::library::generated_test_directory("packaged-autoeq");
        std::fs::create_dir_all(root.join("custom-studio")).unwrap();
        for (name, bytes) in [
            ("index.tsv", INDEX), ("custom-studio/index.tsv", ARCHIVE_INDEX),
            ("custom-studio/step-1.txt", include_str!("../../assets/autoeq/custom-studio/step-1.txt")),
            ("custom-studio/step-2.txt", include_str!("../../assets/autoeq/custom-studio/step-2.txt")),
            ("custom-studio/step-3.txt", include_str!("../../assets/autoeq/custom-studio/step-3.txt")),
            ("custom-studio/step-4.txt", include_str!("../../assets/autoeq/custom-studio/step-4.txt")),
        ] { std::fs::write(root.join(name), bytes).unwrap(); }
        let blocking = crate::rt::Blocking::dedicated_thread();
        let reference = search("");
        let actual = search_from(&blocking, &root, "").unwrap();
        assert_eq!(actual.len(), reference.len());
        for (entry, old) in actual.iter().zip(&reference) {
            assert_eq!((entry.name(), entry.path(), entry.url()), (old.name(), old.path(), old.url()));
        }
        let models = search_from(&blocking, &root, "custom studio").unwrap();
        let old = search("custom studio");
        let no_cache = Path::new("/dev/null/generated-unused-cache");
        for (entry, reference) in models.iter().zip(&old) {
            assert_eq!(entry.cached(&blocking, no_cache).unwrap(), reference.cached(&blocking, no_cache).unwrap());
        }
        // A syntactically valid but modified historical export must not retune
        // a profile under the pinned version or receive the LS/HS correction.
        std::fs::write(root.join(ARCHIVE_FILES[0].0), "Preamp: -1 dB\n").unwrap();
        assert!(models[0].cached(&blocking, no_cache).is_err());
        std::fs::write(root.join("index.tsv"), INDEX.replacen("1Custom", "2Custom", 1)).unwrap();
        assert!(search_from(&blocking, &root, "").is_err());
        std::fs::remove_file(root.join("index.tsv")).unwrap();
        assert!(search_from(&blocking, &root, "").is_err());
        std::fs::File::create(root.join("index.tsv")).unwrap().set_len(1024 * 1024 + 1).unwrap();
        assert!(search_from(&blocking, &root, "").is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn custom_studio_variants_are_bundled_distinct_and_numerically_unchanged() {
        let entries=search("beyerdynamic custom studio");
        assert_eq!(entries.len(),4);
        // A path which cannot be opened proves this needs neither cache writes
        // nor initial network access. Owner data never participates.
        let root=Path::new("/dev/null/generated-autoeq-fixture");
        for (i,entry) in entries.iter().enumerate() {
            assert_eq!(entry.name(),format!("Beyerdynamic Custom Studio (step {})",i+1));
            assert!(entry.url().contains(ARCHIVE_REVISION));
            let raw=entry.archived_text(&crate::rt::Blocking::dedicated_thread()).unwrap().unwrap();
            assert!(Profile::parse(entry.name().into(),entry.url(),&raw).is_err(),"generic LS/HS imports remain strict");
            let profile=entry.load(root,true,None).unwrap();
            assert_eq!(profile,entry.load(root,false,Some("invalid-generated-proxy")).unwrap());
            assert_eq!(profile.preamp,[-6.5,-6.5,-6.8,-6.8][i]);
            assert_eq!(profile.filters.len(),10);
            assert_eq!(profile.source,entry.url());
            let original: Vec<_>=raw.split_whitespace().filter(|word|word.parse::<f64>().is_ok()).collect();
            let corrected=raw.replace("ON LS Fc","ON LSC Fc").replace("ON HS Fc","ON HSC Fc");
            let normalized: Vec<_>=corrected.split_whitespace().filter(|word|word.parse::<f64>().is_ok()).collect();
            assert_eq!(original,normalized,"all frequencies/gains/Q/preamp retain exact text");
            for (line,filter) in raw.lines().skip(1).zip(&profile.filters) {
                let w:Vec<_>=line.split_whitespace().collect();
                assert_eq!(filter.hz,w[5].parse::<f64>().unwrap());
                assert_eq!(filter.gain,w[8].parse::<f64>().unwrap());
                assert_eq!(filter.q,w[11].parse::<f64>().unwrap());
            }
        }
        assert_eq!(search("custom one pro").len(),5);
        assert!(search("custom one studio").is_empty(),"historical typo is not a separate model");
    }
    #[test]
    fn saved_profile_loads_offline_and_corruption_does_not_apply() {
        let entry=search("sennheiser 650").into_iter().find(|e|e.name()=="Sennheiser HD 650").unwrap();
        let base=std::env::var_os("Q2_GENERATED_TEST_ROOT").map(std::path::PathBuf::from).unwrap_or_else(||Path::new(env!("CARGO_MANIFEST_DIR")).join("../../build/eq"));
        let root=base.join(format!("autoeq-cache-test-{}",std::process::id()));
        std::fs::create_dir_all(root.join("autoeq")).unwrap();
        let path=root.join("autoeq").join(entry.cache_name());
        std::fs::write(&path,include_str!("../../assets/autoeq/hd650-fixture.txt")).unwrap();
        let profile=entry.load(&root,true,None).unwrap();
        assert_eq!(profile.name,"Sennheiser HD 650");
        assert_eq!(profile.filters.len(),10);
        let oversized=format!("{}{}",include_str!("../../assets/autoeq/hd650-fixture.txt")," ".repeat(16385));
        std::fs::write(&path,oversized).unwrap();
        assert!(entry.load(&root,true,None).unwrap_err().contains("connect"));
        std::fs::write(&path,"Preamp: NaN dB").unwrap();
        assert!(entry.load(&root,true,None).unwrap_err().contains("connect"));
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn catalog_has_unique_safe_paths_and_searches_all_words() {
        let entries=search("");
        assert_eq!(entries.len(),6037);
        assert!(entries.windows(2).all(|pair|pair[0].name()<=pair[1].name()));
        let mut paths=std::collections::HashSet::new();
        for e in entries {
            assert!(paths.insert(e.path().to_owned()));
            assert!(!e.path().contains("..") && !e.path().contains('?') && !e.path().contains('#'));
            assert!(e.path().split('/').count()>=3);
        }
        let hd=search("sennheiser 650");
        assert!(hd.iter().any(|e|e.name()=="Sennheiser HD 650"));
        let k702=search("k702");
        assert_eq!(k702.len(),4);
        assert!(k702.iter().any(|e|e.name()=="AKG K702"));
        assert_eq!(search("AKG K702").len(),4);
        let passive=search("Samsung Galaxy Buds2 Pro passive");
        assert_eq!(passive.len(),1);
        assert!(passive[0].url().ends_with("(Passive%20mode)/Samsung%20Galaxy%20Buds2%20Pro%20(Passive%20mode)%20ParametricEQ.txt"));
        assert!(search("nonexistent model q2nd fixture").is_empty());
    }
}
