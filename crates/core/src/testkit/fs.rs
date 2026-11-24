//! A small disk in memory, enough for the core's own tests: folders,
//! files with inodes, times that move on every change, case-insensitive
//! volumes, atomic exchange, and one injectable failure. The simulator's
//! disk (hearth-sync-sim) is the full one, with power loss and per-OS
//! behaviour.

use std::collections::BTreeMap;

use crate::fs::{DirEntry, EntryKind, Fs, FsError, FsResult, Meta};
use crate::guarantees::DiskFile;
use crate::ids::{FileId, Hash};
use crate::names::{LocalName, LocalPath, NameKey, VolumeRules, name_key};

#[derive(Clone, Debug)]
enum Content {
    File(Vec<u8>),
    Dir(BTreeMap<Vec<u8>, u128>),
    Link(String),
}

#[derive(Clone, Debug)]
struct Inode {
    content: Content,
    mtime_ns: i64,
    ctime_ns: i64,
    exec: bool,
}

/// The fake disk.
#[derive(Clone, Debug)]
pub struct FakeFs {
    inodes: BTreeMap<u128, Inode>,
    root: u128,
    root_present: bool,
    next: u128,
    tick: i64,
    rules: VolumeRules,
    /// The volume id it reports.
    pub volume: u64,
    /// Free bytes it reports.
    pub free: u64,
    /// The next call fails with this error, once.
    pub fail_next: Option<FsError>,
}

impl FakeFs {
    /// An empty sync root on a volume with these rules.
    pub fn new(rules: VolumeRules) -> FakeFs {
        let mut inodes = BTreeMap::new();
        inodes.insert(
            1,
            Inode {
                content: Content::Dir(BTreeMap::new()),
                mtime_ns: 1,
                ctime_ns: 1,
                exec: false,
            },
        );
        FakeFs {
            inodes,
            root: 1,
            root_present: true,
            next: 2,
            tick: 1_000_000_000,
            rules,
            volume: 7,
            free: 1 << 40,
            fail_next: None,
        }
    }

    fn now(&mut self) -> i64 {
        self.tick += 1_000_000;
        self.tick
    }

    fn key(&self, name: &[u8]) -> Option<NameKey> {
        name_key(&LocalName(name.to_vec()), &self.rules)
    }

    fn check_fault(&mut self) -> FsResult<()> {
        match self.fail_next.take() {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// The entry of a folder under a name, as the volume compares names.
    fn lookup(&self, dir: u128, name: &[u8]) -> Option<(Vec<u8>, u128)> {
        let Content::Dir(entries) = &self.inodes.get(&dir)?.content else {
            return None;
        };
        if let Some(i) = entries.get(name) {
            return Some((name.to_vec(), *i));
        }
        let k = self.key(name);
        if k.is_none() {
            return None;
        }
        entries
            .iter()
            .find(|(n, _)| self.key(n) == k)
            .map(|(n, i)| (n.clone(), *i))
    }

    fn resolve(&self, path: &LocalPath) -> FsResult<Option<u128>> {
        if !self.root_present {
            return Err(FsError::RootUnavailable);
        }
        let mut cur = self.root;
        for c in &path.0 {
            match &self.inodes.get(&cur).map(|i| &i.content) {
                Some(Content::Dir(_)) => {}
                _ => return Err(FsError::NotAFolder),
            }
            match self.lookup(cur, &c.0) {
                Some((_, i)) => cur = i,
                None => return Ok(None),
            }
        }
        Ok(Some(cur))
    }

    fn parent_of(&self, path: &LocalPath) -> FsResult<(u128, Vec<u8>)> {
        let parent = path.parent().ok_or(FsError::Exists)?;
        let name = path.file_name().ok_or(FsError::Exists)?.0.clone();
        let p = self.resolve(&parent)?.ok_or(FsError::NotFound)?;
        match self.inodes.get(&p).map(|i| &i.content) {
            Some(Content::Dir(_)) => Ok((p, name)),
            _ => Err(FsError::NotAFolder),
        }
    }

    fn meta_of(&self, i: u128) -> Meta {
        let n = &self.inodes[&i];
        let (kind, size) = match &n.content {
            Content::File(b) => (EntryKind::File, b.len() as u64),
            Content::Dir(_) => (EntryKind::Folder, 0),
            Content::Link(t) => (EntryKind::Symlink(t.clone()), 0),
        };
        Meta {
            kind,
            file_id: FileId {
                volume: self.volume,
                id: i,
            },
            size,
            mtime_ns: n.mtime_ns,
            ctime_ns: n.ctime_ns,
            exec: n.exec,
        }
    }

    fn dir_mut(&mut self, d: u128) -> &mut BTreeMap<Vec<u8>, u128> {
        match &mut self.inodes.get_mut(&d).expect("a folder").content {
            Content::Dir(m) => m,
            _ => unreachable!("checked to be a folder"),
        }
    }

    fn touch_dir(&mut self, d: u128) {
        let t = self.now();
        if let Some(n) = self.inodes.get_mut(&d) {
            n.mtime_ns = t;
            n.ctime_ns = t;
        }
    }

    fn insert(&mut self, path: &LocalPath, content: Content) -> FsResult<u128> {
        let (p, name) = self.parent_of(path)?;
        if self.lookup(p, &name).is_some() {
            return Err(FsError::Exists);
        }
        let i = self.next;
        self.next += 1;
        let t = self.now();
        self.inodes.insert(
            i,
            Inode {
                content,
                mtime_ns: t,
                ctime_ns: t,
                exec: false,
            },
        );
        self.dir_mut(p).insert(name, i);
        self.touch_dir(p);
        Ok(i)
    }

    // ----- what the person does -----

    /// Writes a file in place (same inode), making folders as needed.
    pub fn write(&mut self, parts: &[&str], bytes: &[u8]) {
        let path = LocalPath::from_strs(parts);
        self.mkdir_all(&parts[..parts.len() - 1]);
        match self.resolve(&path) {
            Ok(Some(i)) => {
                let t = self.now();
                let n = self.inodes.get_mut(&i).expect("exists");
                n.content = Content::File(bytes.to_vec());
                n.mtime_ns = t;
                n.ctime_ns = t;
            }
            _ => {
                let _ = self.insert(&path, Content::File(bytes.to_vec()));
            }
        }
    }

    /// Writes content of the same size in place and puts the old
    /// modification time back, as tools that restore an mtime do (the
    /// change time still moves, which is how the scanner sees it: 3.3).
    pub fn write_keeping_mtime(&mut self, parts: &[&str], bytes: &[u8]) {
        let path = LocalPath::from_strs(parts);
        if let Ok(Some(i)) = self.resolve(&path) {
            let t = self.now();
            let n = self.inodes.get_mut(&i).expect("exists");
            n.content = Content::File(bytes.to_vec());
            n.ctime_ns = t;
        }
    }

    /// Saves as editors do: a new file renamed over the old (new inode).
    pub fn save_by_rename(&mut self, parts: &[&str], bytes: &[u8]) {
        let path = LocalPath::from_strs(parts);
        self.remove(parts);
        let _ = self.insert(&path, Content::File(bytes.to_vec()));
    }

    /// Makes folders down a path.
    pub fn mkdir_all(&mut self, parts: &[&str]) {
        for i in 1..=parts.len() {
            let p = LocalPath::from_strs(&parts[..i]);
            if let Ok(None) = self.resolve(&p) {
                let _ = self.insert(&p, Content::Dir(BTreeMap::new()));
            }
        }
    }

    /// Removes an item and everything below it.
    pub fn remove(&mut self, parts: &[&str]) {
        let path = LocalPath::from_strs(parts);
        if let Ok((p, name)) = self.parent_of(&path) {
            if let Some((real, _)) = self.lookup(p, &name) {
                self.dir_mut(p).remove(&real);
                self.touch_dir(p);
            }
        }
    }

    /// Renames or moves an item (the person's own rename).
    pub fn rename(&mut self, from: &[&str], to: &[&str]) {
        let _ = Fs::rename_noreplace(self, &LocalPath::from_strs(from), &LocalPath::from_strs(to));
    }

    /// Makes a symbolic link.
    pub fn symlink(&mut self, parts: &[&str], target: &str) {
        let _ = self.insert(&LocalPath::from_strs(parts), Content::Link(target.into()));
    }

    /// The bytes of a file, if it is one.
    pub fn read_file(&self, parts: &[&str]) -> Option<Vec<u8>> {
        let i = self.resolve(&LocalPath::from_strs(parts)).ok()??;
        match &self.inodes[&i].content {
            Content::File(b) => Some(b.clone()),
            _ => None,
        }
    }

    /// Whether something is at a path.
    pub fn exists(&self, parts: &[&str]) -> bool {
        matches!(self.resolve(&LocalPath::from_strs(parts)), Ok(Some(_)))
    }

    /// Takes the sync root away (an unplugged disk) or brings it back.
    pub fn set_root_present(&mut self, present: bool) {
        self.root_present = present;
    }

    /// Every file under the root, the engine's own places included.
    pub fn files(&self) -> Vec<DiskFile> {
        let mut out = Vec::new();
        let mut stack = vec![(self.root, LocalPath::root())];
        while let Some((i, p)) = stack.pop() {
            match &self.inodes[&i].content {
                Content::Dir(m) => {
                    for (n, c) in m {
                        stack.push((*c, p.join(&LocalName(n.clone()))));
                    }
                }
                Content::File(b) => {
                    let m = self.meta_of(i);
                    out.push(DiskFile {
                        path: p,
                        hash: Hash::of(b),
                        file_id: m.file_id,
                        size: m.size,
                        mtime_ns: m.mtime_ns,
                    });
                }
                Content::Link(_) => {}
            }
        }
        out.sort_by(|a, b| a.path.cmp(&b.path));
        out
    }

    /// Files outside the engine's own places, as text paths and contents.
    pub fn user_files(&self) -> BTreeMap<String, Vec<u8>> {
        self.files()
            .into_iter()
            .filter(|f| !crate::guarantees::is_engine_path(&f.path))
            .map(|f| {
                let parts: Vec<String> = f.path.0.iter().map(LocalName::display).collect();
                let refs: Vec<&str> = parts.iter().map(String::as_str).collect();
                (f.path.display(), self.read_file(&refs).unwrap_or_default())
            })
            .collect()
    }
}

impl Fs for FakeFs {
    fn volume(&mut self) -> FsResult<u64> {
        self.check_fault()?;
        if !self.root_present {
            return Err(FsError::RootUnavailable);
        }
        Ok(self.volume)
    }

    fn read_marker(&mut self) -> FsResult<Option<String>> {
        self.check_fault()?;
        let p = LocalPath::from_strs(&[crate::names::ENGINE_DIR, "root"]);
        match self.resolve(&p)? {
            Some(i) => match &self.inodes[&i].content {
                Content::File(b) => Ok(Some(String::from_utf8_lossy(b).into_owned())),
                _ => Ok(None),
            },
            None => Ok(None),
        }
    }

    fn write_marker(&mut self, marker: &str) -> FsResult<()> {
        self.check_fault()?;
        self.mkdir_all(&[crate::names::ENGINE_DIR]);
        self.write(&[crate::names::ENGINE_DIR, "root"], marker.as_bytes());
        Ok(())
    }

    fn list_dir(&mut self, dir: &LocalPath) -> FsResult<Vec<DirEntry>> {
        self.check_fault()?;
        let i = self.resolve(dir)?.ok_or(FsError::NotFound)?;
        let Content::Dir(m) = &self.inodes[&i].content else {
            return Err(FsError::NotAFolder);
        };
        Ok(m.iter()
            .map(|(n, c)| DirEntry {
                name: LocalName(n.clone()),
                meta: self.meta_of(*c),
            })
            .collect())
    }

    fn stat(&mut self, path: &LocalPath) -> FsResult<Option<Meta>> {
        self.check_fault()?;
        match self.resolve(path) {
            Ok(Some(i)) => Ok(Some(self.meta_of(i))),
            Ok(None) | Err(FsError::NotAFolder) => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn read(&mut self, path: &LocalPath, offset: u64, len: u64) -> FsResult<Vec<u8>> {
        self.check_fault()?;
        let i = self.resolve(path)?.ok_or(FsError::NotFound)?;
        match &self.inodes[&i].content {
            Content::File(b) => {
                let start = usize::try_from(offset).unwrap_or(usize::MAX).min(b.len());
                let end = start
                    .saturating_add(usize::try_from(len).unwrap_or(usize::MAX))
                    .min(b.len());
                Ok(b[start..end].to_vec())
            }
            _ => Err(FsError::Io("not a file".into())),
        }
    }

    fn create(&mut self, path: &LocalPath) -> FsResult<()> {
        self.check_fault()?;
        self.insert(path, Content::File(Vec::new())).map(|_| ())
    }

    fn write_at(&mut self, path: &LocalPath, offset: u64, data: &[u8]) -> FsResult<()> {
        self.check_fault()?;
        let i = self.resolve(path)?.ok_or(FsError::NotFound)?;
        let t = self.now();
        let n = self.inodes.get_mut(&i).ok_or(FsError::NotFound)?;
        let Content::File(b) = &mut n.content else {
            return Err(FsError::Io("not a file".into()));
        };
        let off = usize::try_from(offset).map_err(|_| FsError::NoSpace)?;
        if b.len() < off + data.len() {
            b.resize(off + data.len(), 0);
        }
        b[off..off + data.len()].copy_from_slice(data);
        n.mtime_ns = t;
        n.ctime_ns = t;
        Ok(())
    }

    fn truncate(&mut self, path: &LocalPath, len: u64) -> FsResult<()> {
        self.check_fault()?;
        let i = self.resolve(path)?.ok_or(FsError::NotFound)?;
        let t = self.now();
        let n = self.inodes.get_mut(&i).ok_or(FsError::NotFound)?;
        if let Content::File(b) = &mut n.content {
            b.resize(usize::try_from(len).unwrap_or(0), 0);
        }
        n.ctime_ns = t;
        Ok(())
    }

    fn set_meta(&mut self, path: &LocalPath, mtime_ns: i64, exec: bool) -> FsResult<()> {
        self.check_fault()?;
        let i = self.resolve(path)?.ok_or(FsError::NotFound)?;
        let t = self.now();
        let n = self.inodes.get_mut(&i).ok_or(FsError::NotFound)?;
        n.mtime_ns = mtime_ns;
        n.exec = exec;
        n.ctime_ns = t;
        Ok(())
    }

    fn sync_file(&mut self, path: &LocalPath) -> FsResult<()> {
        self.check_fault()?;
        self.resolve(path)?.ok_or(FsError::NotFound).map(|_| ())
    }

    fn sync_dir(&mut self, path: &LocalPath) -> FsResult<()> {
        self.check_fault()?;
        self.resolve(path)?.ok_or(FsError::NotFound).map(|_| ())
    }

    fn rename_noreplace(&mut self, from: &LocalPath, to: &LocalPath) -> FsResult<()> {
        self.check_fault()?;
        let (fp, fname) = self.parent_of(from)?;
        let (real, i) = self.lookup(fp, &fname).ok_or(FsError::NotFound)?;
        let (tp, tname) = self.parent_of(to)?;
        if let Some((_, other)) = self.lookup(tp, &tname) {
            if other != i {
                return Err(FsError::Exists);
            }
        }
        // Never into itself.
        let mut cur = Some(tp);
        while let Some(c) = cur {
            if c == i {
                return Err(FsError::Io("into itself".into()));
            }
            cur = self.inodes.iter().find_map(|(k, n)| match &n.content {
                Content::Dir(m) if m.values().any(|v| *v == c) => Some(*k),
                _ => None,
            });
        }
        self.dir_mut(fp).remove(&real);
        self.dir_mut(tp).insert(tname, i);
        let t = self.now();
        if let Some(n) = self.inodes.get_mut(&i) {
            n.ctime_ns = t;
        }
        self.touch_dir(fp);
        self.touch_dir(tp);
        Ok(())
    }

    fn exchange(&mut self, a: &LocalPath, b: &LocalPath) -> FsResult<()> {
        self.check_fault()?;
        let (ap, an) = self.parent_of(a)?;
        let (bp, bn) = self.parent_of(b)?;
        let (ar, ai) = self.lookup(ap, &an).ok_or(FsError::NotFound)?;
        let (br, bi) = self.lookup(bp, &bn).ok_or(FsError::NotFound)?;
        self.dir_mut(ap).insert(ar, bi);
        self.dir_mut(bp).insert(br, ai);
        let t = self.now();
        for i in [ai, bi] {
            if let Some(n) = self.inodes.get_mut(&i) {
                n.ctime_ns = t;
            }
        }
        Ok(())
    }

    fn mkdir(&mut self, path: &LocalPath) -> FsResult<()> {
        self.check_fault()?;
        self.insert(path, Content::Dir(BTreeMap::new())).map(|_| ())
    }

    fn remove_empty_dir(&mut self, path: &LocalPath) -> FsResult<()> {
        self.check_fault()?;
        let (p, name) = self.parent_of(path)?;
        let (real, i) = self.lookup(p, &name).ok_or(FsError::NotFound)?;
        match &self.inodes[&i].content {
            Content::Dir(m) if m.is_empty() => {}
            Content::Dir(_) => return Err(FsError::NotEmpty),
            _ => return Err(FsError::NotAFolder),
        }
        self.dir_mut(p).remove(&real);
        self.touch_dir(p);
        Ok(())
    }

    fn remove_temp(&mut self, path: &LocalPath) -> FsResult<()> {
        self.check_fault()?;
        let engine_temp = path.0.first().is_some_and(|n| n.0 == b".hearthsync")
            || path
                .file_name()
                .is_some_and(|n| n.0.ends_with(b".hearthtmp"));
        assert!(
            engine_temp,
            "remove_temp on a path not the engine's: {path:?}"
        );
        let (p, name) = self.parent_of(path)?;
        let (real, _) = self.lookup(p, &name).ok_or(FsError::NotFound)?;
        self.dir_mut(p).remove(&real);
        Ok(())
    }

    fn remove_held(&mut self, path: &LocalPath) -> FsResult<()> {
        self.check_fault()?;
        assert!(
            path.0.len() > 2 && path.0[0].0 == b".hearthsync" && path.0[1].0 == b"held",
            "remove_held outside the holding area: {path:?}"
        );
        let (p, name) = self.parent_of(path)?;
        let (real, _) = self.lookup(p, &name).ok_or(FsError::NotFound)?;
        self.dir_mut(p).remove(&real);
        Ok(())
    }

    fn free_space(&mut self) -> FsResult<u64> {
        self.check_fault()?;
        Ok(self.free)
    }
}
