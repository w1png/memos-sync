use anyhow::{Context, Result, bail, ensure};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Component, Path, PathBuf},
};
use uuid::Uuid;

#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct State {
    pub version: u32,
    pub identity: String,
    pub memos: BTreeMap<String, Record>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Record {
    pub path: PathBuf,
    pub base: Option<String>,
    #[serde(default)]
    pub remote_seen: bool,
    #[serde(default)]
    pub cleanup_pending: bool,
    pub pending_write: Option<PendingWrite>,
    pub conflict: Option<Conflict>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PendingWrite {
    pub content: String,
    pub create: bool,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Conflict {
    pub local: String,
    pub remote: String,
    pub published: bool,
}

pub struct Store {
    pub root: PathBuf,
    _lock: File,
}

#[derive(Debug)]
pub struct Note {
    pub path: PathBuf,
    pub name: Option<String>,
    pub content: String,
    pub raw: String,
}

pub fn validate_name(name: &str) -> Result<()> {
    let id = name
        .strip_prefix("memos/")
        .context("Invalid memos_id; expected memos/<id>")?;
    ensure!(
        !id.is_empty()
            && id.len() <= 128
            && id
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_'),
        "Invalid memo ID"
    );
    Ok(())
}

pub fn render(name: &str, content: &str) -> String {
    format!("---\nmemos_id: {name}\n---\n{content}")
}

pub fn parse(raw: &str) -> Result<(Option<String>, String)> {
    let (start, delimiter) = if raw.starts_with("---\r\n") {
        (5, "\r\n---\r\n")
    } else if raw.starts_with("---\n") {
        (4, "\n---\n")
    } else {
        return Ok((None, raw.to_owned()));
    };
    let Some(end) = raw[start..].find(delimiter) else {
        ensure!(!raw.contains("memos_id:"), "Unclosed sync metadata header");
        return Ok((None, raw.to_owned()));
    };
    let header = &raw[start..start + end];
    if !header.lines().any(|line| line.starts_with("memos_id:")) {
        return Ok((None, raw.to_owned()));
    }
    let metadata: serde_yaml_ng::Value =
        serde_yaml_ng::from_str(header).context("Invalid sync metadata")?;
    let name = metadata
        .get("memos_id")
        .and_then(|v| v.as_str())
        .context("memos_id must be a string")?
        .to_owned();
    validate_name(&name)?;
    Ok((Some(name), raw[start + end + delimiter.len()..].to_owned()))
}

impl Store {
    pub fn open(root: &Path) -> Result<Option<Self>> {
        fs::create_dir_all(root).context("Cannot create data directory")?;
        let root = root.canonicalize()?;
        let hidden = root.join(".memos");
        ensure!(!hidden.is_symlink(), ".memos must not be a symlink");
        fs::create_dir_all(&hidden)?;
        let lock_path = hidden.join("lock");
        ensure!(!lock_path.is_symlink(), "Lock must not be a symlink");
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(lock_path)?;
        match lock.try_lock_exclusive() {
            Ok(()) => Ok(Some(Self { root, _lock: lock })),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    pub fn path(&self, relative: &Path) -> Result<PathBuf> {
        ensure!(
            !relative.as_os_str().is_empty()
                && relative
                    .components()
                    .all(|c| matches!(c, Component::Normal(_))),
            "Unsafe relative path: {}",
            relative.display()
        );
        let mut path = self.root.clone();
        for part in relative.components() {
            path.push(part);
            ensure!(!path.is_symlink(), "Refusing symlink: {}", path.display());
        }
        Ok(path)
    }

    pub fn read(&self, relative: &Path) -> Result<Option<String>> {
        let path = self.path(relative)?;
        match fs::read_to_string(&path) {
            Ok(value) => Ok(Some(value)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("Cannot read {}", path.display())),
        }
    }

    pub fn write(&self, relative: &Path, content: &str) -> Result<()> {
        let path = self.path(relative)?;
        let parent = path.parent().context("Missing parent directory")?;
        fs::create_dir_all(parent)?;
        let mut temp = tempfile::NamedTempFile::new_in(parent)?;
        temp.write_all(content.as_bytes())?;
        temp.as_file().sync_all()?;
        temp.persist(&path)
            .map_err(|e| e.error)
            .with_context(|| format!("Cannot write {}", path.display()))?;
        File::open(parent)?.sync_all()?;
        Ok(())
    }

    pub fn replace(
        &self,
        name: &str,
        relative: &Path,
        expected: Option<&str>,
        content: &str,
    ) -> Result<()> {
        ensure!(
            self.read(relative)?.as_deref() == expected,
            "{} changed during sync; try again",
            relative.display()
        );
        if let Some(old) = expected {
            let backup = PathBuf::from(".memos/backups")
                .join(name.strip_prefix("memos/").context("Invalid name")?)
                .join(format!("{}.md", Uuid::new_v4()));
            self.write(&backup, old)?;
        }
        ensure!(
            self.read(relative)?.as_deref() == expected,
            "{} changed during sync; try again",
            relative.display()
        );
        self.write(relative, content)
    }

    pub fn load(&self, identity: &str) -> Result<State> {
        let Some(raw) = self.read(Path::new(".memos/state.json"))? else {
            return Ok(State {
                version: 1,
                identity: identity.to_owned(),
                memos: BTreeMap::new(),
            });
        };
        let state: State = serde_json::from_str(&raw)
            .context("Invalid .memos/state.json; refusing to overwrite sync history")?;
        ensure!(state.version == 1, "Unsupported state version");
        ensure!(
            state.identity == identity,
            "Data directory belongs to another Memos instance or user"
        );
        let mut paths = std::collections::HashSet::new();
        for (name, record) in &state.memos {
            validate_name(name)?;
            self.path(&record.path)?;
            ensure!(
                record.path.file_name().is_some_and(|f| f == "note.md")
                    && record.path.components().count() > 1,
                "Invalid stored note path"
            );
            ensure!(
                !record
                    .path
                    .components()
                    .any(|c| c.as_os_str().to_string_lossy().starts_with('.')),
                "Hidden note path in state"
            );
            ensure!(paths.insert(&record.path), "Duplicate note paths in state");
        }
        Ok(state)
    }

    pub fn save(&self, state: &State) -> Result<()> {
        self.write(
            Path::new(".memos/state.json"),
            &serde_json::to_string_pretty(state)?,
        )
    }

    pub fn scan(&self) -> Result<Vec<Note>> {
        fn walk(store: &Store, dir: &Path, notes: &mut Vec<Note>) -> Result<()> {
            for entry in fs::read_dir(dir)? {
                let entry = entry?;
                if entry.file_name().to_string_lossy().starts_with('.') {
                    continue;
                }
                let kind = entry.file_type()?;
                if kind.is_symlink() {
                    continue;
                }
                if kind.is_dir() {
                    walk(store, &entry.path(), notes)?;
                } else if kind.is_file() && entry.file_name() == "note.md" {
                    let relative = entry.path().strip_prefix(&store.root)?.to_path_buf();
                    ensure!(
                        relative.components().count() > 1,
                        "Put note.md inside its own folder"
                    );
                    let raw = fs::read_to_string(entry.path())?;
                    let (name, content) = parse(&raw)
                        .with_context(|| format!("Invalid note {}", relative.display()))?;
                    notes.push(Note {
                        path: relative,
                        name,
                        content,
                        raw,
                    });
                }
            }
            Ok(())
        }
        let mut notes = Vec::new();
        walk(self, &self.root, &mut notes)?;
        notes.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(notes)
    }

    pub fn trash(&self, name: &str, relative: &Path) -> Result<()> {
        let parent = relative.parent().context("Missing memo folder")?;
        let source = self.path(parent)?;
        if !source.exists() {
            return Ok(());
        }
        for note in self.scan()? {
            if note.path != relative && note.path.starts_with(parent) {
                bail!("Cannot trash a folder containing another memo");
            }
        }
        let target = self.path(&PathBuf::from(".memos/trash").join(format!(
            "{}--{}",
            name.trim_start_matches("memos/"),
            Uuid::new_v4()
        )))?;
        fs::create_dir_all(target.parent().unwrap())?;
        fs::rename(source, target)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn markdown_bytes_round_trip() {
        for body in [
            "",
            "# Hello\n\n- [ ] task\n",
            "---\ntitle: hello\n---\nbody\r\n",
            "Привет 👋\nno final newline",
        ] {
            let raw = render("memos/test", body);
            assert_eq!(
                parse(&raw).unwrap(),
                (Some("memos/test".into()), body.into())
            );
        }
        assert_eq!(
            parse("---\ntitle: x\n---\nbody").unwrap().1,
            "---\ntitle: x\n---\nbody"
        );
        assert!(parse("---\nmemos_id: ../oops\n---\nbody").is_err());
        assert!(parse("---\nmemos_id: memos/x").is_err());
    }
    #[test]
    fn writes_are_checked_and_backed_up() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap().unwrap();
        let path = Path::new("test/note.md");
        store.write(path, "before").unwrap();
        assert!(
            store
                .replace("memos/test", path, Some("wrong"), "after")
                .is_err()
        );
        assert_eq!(store.read(path).unwrap().unwrap(), "before");
        store
            .replace("memos/test", path, Some("before"), "after")
            .unwrap();
        assert_eq!(store.read(path).unwrap().unwrap(), "after");
        assert!(store.path(Path::new("../outside")).is_err());
        assert!(Store::open(dir.path()).unwrap().is_none());
    }
}
