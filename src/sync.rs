use crate::api::{Memo, MemoApi};
use crate::storage::{self, Conflict, Note, PendingWrite, Record, State, Store};
use anyhow::{Context, Result, ensure};
use similar::TextDiff;
use std::{
    collections::{BTreeMap, HashSet},
    fs,
    path::PathBuf,
};
use uuid::Uuid;

#[derive(Default)]
pub struct Report {
    pub downloaded: usize,
    pub uploaded: usize,
    pub trashed: usize,
    pub conflicts: Vec<PathBuf>,
    pub errors: Vec<anyhow::Error>,
}

fn checkpoint(store: &Store, state: &mut State, name: &str, record: &Record) -> Result<()> {
    state.memos.insert(name.to_owned(), record.clone());
    store.save(state)
}

fn artifact(record: &Record, filename: &str) -> PathBuf {
    record.path.with_file_name(filename)
}

fn cleanup(store: &Store, record: &Record) -> Result<()> {
    for filename in ["remote.md", "conflict.diff"] {
        let path = store.path(&artifact(record, filename))?;
        match fs::remove_file(path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

fn publish_conflict(
    store: &Store,
    state: &mut State,
    name: &str,
    record: &mut Record,
) -> Result<()> {
    let conflict = record.conflict.as_ref().context("Missing conflict state")?;
    let local = storage::render(name, &conflict.local);
    let remote = storage::render(name, &conflict.remote);
    let diff = TextDiff::from_lines(&local, &remote)
        .unified_diff()
        .header("note.md", "remote.md")
        .to_string();
    store.write(&artifact(record, "remote.md"), &remote)?;
    // An empty diff still acts as a marker until explicitly acknowledged.
    store.write(&artifact(record, "conflict.diff"), &diff)?;
    record.conflict.as_mut().unwrap().published = true;
    checkpoint(store, state, name, record)
}

fn conflict(
    store: &Store,
    state: &mut State,
    name: &str,
    record: &mut Record,
    local: &str,
    remote: &str,
) -> Result<()> {
    record.conflict = Some(Conflict {
        local: local.to_owned(),
        remote: remote.to_owned(),
        published: false,
    });
    // Persist intent first; a crash while writing artifacts cannot look like acknowledgment.
    checkpoint(store, state, name, record)?;
    publish_conflict(store, state, name, record)
}

fn finish(
    store: &Store,
    state: &mut State,
    name: &str,
    record: &mut Record,
    content: &str,
) -> Result<()> {
    let had_conflict = record.conflict.is_some();
    record.base = Some(content.to_owned());
    record.remote_seen = true;
    record.pending_write = None;
    record.conflict = None;
    record.cleanup_pending = had_conflict;
    checkpoint(store, state, name, record)?;
    if had_conflict {
        cleanup(store, record)?;
        record.cleanup_pending = false;
        checkpoint(store, state, name, record)?;
    }
    Ok(())
}

fn local_note(store: &Store, record: &Record, name: &str) -> Result<Option<Note>> {
    let Some(raw) = store.read(&record.path)? else {
        return Ok(None);
    };
    let (id, content) = storage::parse(&raw)?;
    ensure!(
        id.as_deref() == Some(name),
        "{} has missing or changed memos_id",
        record.path.display()
    );
    Ok(Some(Note {
        path: record.path.clone(),
        name: id,
        content,
        raw,
    }))
}

fn upload(
    api: &impl MemoApi,
    store: &Store,
    state: &mut State,
    record: &mut Record,
    local: &Note,
    remote: Option<&Memo>,
    report: &mut Report,
) -> Result<()> {
    let name = local.name.as_deref().context("Missing memo identity")?;
    let current = api.get_memo(name)?;
    ensure!(
        current.as_ref().map(|m| &m.content) == remote.map(|m| &m.content),
        "Remote memo changed during sync; retry to compare versions"
    );
    ensure!(
        store.read(&record.path)?.as_deref() == Some(&local.raw),
        "{} changed during sync; retry",
        record.path.display()
    );
    let create = remote.is_none();
    record.pending_write = Some(PendingWrite {
        content: local.content.clone(),
        create,
    });
    checkpoint(store, state, name, record)?;
    let saved = if create {
        api.create_memo(
            name.strip_prefix("memos/").context("Invalid memo name")?,
            &local.content,
        )?
    } else {
        api.update_memo(name, &local.content)?
    };
    ensure!(saved.name == name, "Server returned a different memo ID");
    ensure!(
        saved.content == local.content,
        "Server returned different content; keeping pending write for recovery"
    );
    report.uploaded += 1;
    finish(store, state, name, record, &saved.content)
}

fn sync_one(
    api: &impl MemoApi,
    store: &Store,
    state: &mut State,
    name: &str,
    report: &mut Report,
) -> Result<()> {
    let mut record = state.memos.get(name).context("Missing memo state")?.clone();
    if record.cleanup_pending {
        cleanup(store, &record)?;
        record.cleanup_pending = false;
        checkpoint(store, state, name, &record)?;
    }
    let remote = api.get_memo(name)?;
    if remote.is_some() && !record.remote_seen {
        record.remote_seen = true;
        checkpoint(store, state, name, &record)?;
    }
    // Ambiguous HTTP outcomes are reconciled before making another mutation.
    if let Some(pending) = record.pending_write.clone() {
        if remote
            .as_ref()
            .is_some_and(|r| r.content == pending.content)
        {
            finish(store, state, name, &mut record, &pending.content)?;
        } else if !pending.create || remote.is_some() {
            record.pending_write = None;
            checkpoint(store, state, name, &record)?;
        }
    }
    let local = local_note(store, &record, name)?;
    let Some(remote) = remote else {
        if !record.remote_seen
            && record.base.is_none()
            && record.pending_write.as_ref().is_some_and(|p| p.create)
        {
            let local = local.context("New local note disappeared before upload")?;
            return upload(api, store, state, &mut record, &local, None, report);
        }
        ensure!(
            record.base.is_some() || record.remote_seen,
            "Unknown memos_id {name} does not exist remotely; remove the header only if creating a new note"
        );
        store.trash(name, &record.path)?;
        state.memos.remove(name);
        store.save(state)?;
        report.trashed += 1;
        return Ok(());
    };
    let Some(local) = local else {
        if let Some(pending) = record.conflict.clone() {
            record.conflict = Some(Conflict {
                local: pending.local.clone(),
                remote: remote.content.clone(),
                published: false,
            });
            checkpoint(store, state, name, &record)?;
            let recovery = artifact(&record, "local.md");
            // Never overwrite a previous recovery copy edited by the user.
            if store.read(&recovery)?.is_none() {
                store.write(&recovery, &storage::render(name, &pending.local))?;
            }
            store.replace(
                name,
                &record.path,
                None,
                &storage::render(name, &remote.content),
            )?;
            report.downloaded += 1;
            return conflict(
                store,
                state,
                name,
                &mut record,
                &pending.local,
                &remote.content,
            );
        }
        store.replace(
            name,
            &record.path,
            None,
            &storage::render(name, &remote.content),
        )?;
        report.downloaded += 1;
        return finish(store, state, name, &mut record, &remote.content);
    };

    if let Some(pending) = record.conflict.clone() {
        if !pending.published {
            return publish_conflict(store, state, name, &mut record);
        }
        if store.read(&artifact(&record, "conflict.diff"))?.is_some() {
            if store.read(&artifact(&record, "remote.md"))?.is_none() {
                store.write(
                    &artifact(&record, "remote.md"),
                    &storage::render(name, &pending.remote),
                )?;
            }
            return Ok(());
        }
        if local.content == remote.content {
            return finish(store, state, name, &mut record, &remote.content);
        }
        if remote.content != pending.remote {
            return conflict(
                store,
                state,
                name,
                &mut record,
                &local.content,
                &remote.content,
            );
        }
        return upload(
            api,
            store,
            state,
            &mut record,
            &local,
            Some(&remote),
            report,
        );
    }

    if local.content == remote.content {
        if record.base.as_ref() != Some(&remote.content) {
            finish(store, state, name, &mut record, &remote.content)?;
        }
    } else if record.base.as_ref() == Some(&remote.content) {
        upload(
            api,
            store,
            state,
            &mut record,
            &local,
            Some(&remote),
            report,
        )?;
    } else if record.base.as_ref() == Some(&local.content) {
        store.replace(
            name,
            &record.path,
            Some(&local.raw),
            &storage::render(name, &remote.content),
        )?;
        report.downloaded += 1;
        finish(store, state, name, &mut record, &remote.content)?;
    } else {
        conflict(
            store,
            state,
            name,
            &mut record,
            &local.content,
            &remote.content,
        )?;
    }
    Ok(())
}

fn folder_name(content: &str, name: &str) -> String {
    let title = content
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("untitled")
        .trim()
        .trim_start_matches('#')
        .trim();
    let mut slug = String::new();
    for c in title.chars().take(60) {
        if slug.len() >= 100 {
            break;
        }
        if c.is_alphanumeric() {
            slug.extend(c.to_lowercase());
        } else if !slug.ends_with('-') {
            slug.push('-');
        }
    }
    let slug = slug.trim_matches('-');
    format!(
        "{}--{}",
        if slug.is_empty() { "untitled" } else { slug },
        name.trim_start_matches("memos/")
    )
}

pub fn synchronize(api: &impl MemoApi, store: &Store) -> Result<Report> {
    let identity = api.identity()?;
    let mut state = store.load(&identity)?;
    // Complete both listings and validate all local identities before any note mutation.
    let remote: BTreeMap<_, _> = api
        .list_memos()?
        .into_iter()
        .map(|memo| (memo.name.clone(), memo))
        .collect();
    let mut notes = store.scan()?;
    let mut seen = HashSet::new();
    let previous_paths: BTreeMap<_, _> = state
        .memos
        .iter()
        .map(|(name, record)| (record.path.clone(), name.clone()))
        .collect();
    for note in &mut notes {
        if let Some(known) = previous_paths.get(&note.path)
            && note.name.is_none()
        {
            note.name = Some(known.clone());
        }
        if let Some(name) = &note.name {
            ensure!(
                seen.insert(name.clone()),
                "Duplicate memos_id {name}; no files synced"
            );
        }
    }
    for note in &mut notes {
        let new = note.name.is_none();
        let name = note
            .name
            .clone()
            .unwrap_or_else(|| format!("memos/{}", Uuid::new_v4()));
        storage::validate_name(&name)?;
        if let Some(record) = state.memos.get_mut(&name) {
            if record.path != note.path
                && store
                    .read(&note.path.with_file_name("conflict.diff"))?
                    .is_none()
                && let Some(conflict) = &mut record.conflict
            {
                // Moving just note.md must not count as deleting the conflict marker.
                conflict.published = false;
            }
            record.path = note.path.clone();
        } else {
            state.memos.insert(
                name.clone(),
                Record {
                    path: note.path.clone(),
                    base: None,
                    remote_seen: false,
                    cleanup_pending: false,
                    pending_write: new.then(|| PendingWrite {
                        content: note.content.clone(),
                        create: true,
                    }),
                    conflict: None,
                },
            );
        }
        note.name = Some(name);
    }
    for (name, memo) in &remote {
        if !state.memos.contains_key(name) {
            let mut folder = PathBuf::from(folder_name(&memo.content, name));
            if store.path(&folder)?.exists() {
                folder = PathBuf::from(format!("{}--{}", folder.display(), Uuid::new_v4()));
            }
            state.memos.insert(
                name.clone(),
                Record {
                    path: folder.join("note.md"),
                    base: Some(memo.content.clone()),
                    remote_seen: true,
                    cleanup_pending: false,
                    pending_write: None,
                    conflict: None,
                },
            );
        }
    }
    let mut unique_paths = HashSet::new();
    for record in state.memos.values() {
        ensure!(
            unique_paths.insert(&record.path),
            "Two memos map to the same file; refusing sync"
        );
    }
    store.save(&state)?;
    // Journal IDs before injecting headers or issuing creates, including recovery after a crash here.
    for note in &notes {
        let name = note.name.as_ref().unwrap();
        let (existing, _) = storage::parse(&note.raw)?;
        if existing.is_none() {
            store.replace(
                name,
                &note.path,
                Some(&note.raw),
                &storage::render(name, &note.content),
            )?;
        }
    }
    let mut report = Report::default();
    let names: Vec<_> = state.memos.keys().cloned().collect();
    for name in names {
        if let Err(error) = sync_one(api, store, &mut state, &name, &mut report) {
            report.errors.push(error.context(format!("Syncing {name}")));
        }
    }
    report.conflicts = state
        .memos
        .values()
        .filter(|r| r.conflict.is_some())
        .map(|r| store.root.join(artifact(r, "conflict.diff")))
        .collect();
    Ok(report)
}
