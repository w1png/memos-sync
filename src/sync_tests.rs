use crate::api::{Memo, MemoApi};
use crate::storage::{self, PendingWrite, Record, State, Store};
use crate::sync::synchronize;
use anyhow::{Result, bail};
use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

#[derive(Default)]
struct FakeApi {
    memos: RefCell<BTreeMap<String, String>>,
    creates: Cell<usize>,
    updates: Cell<usize>,
    gets: Cell<usize>,
    fail_list: Cell<bool>,
    fail_after_write: Cell<bool>,
    edit_on_second_get: RefCell<Option<(PathBuf, String)>>,
}

impl FakeApi {
    fn put(&self, name: &str, content: &str) {
        self.memos.borrow_mut().insert(name.into(), content.into());
    }

    fn written(&self, name: &str, content: &str) -> Result<Memo> {
        self.put(name, content);
        if self.fail_after_write.replace(false) {
            bail!("Simulated lost HTTP response");
        }
        Ok(Memo {
            name: name.into(),
            content: content.into(),
        })
    }
}

impl MemoApi for FakeApi {
    fn identity(&self) -> Result<String> {
        Ok("test-instance#users/1".into())
    }

    fn list_memos(&self) -> Result<Vec<Memo>> {
        if self.fail_list.get() {
            bail!("Simulated failed second page");
        }
        Ok(self
            .memos
            .borrow()
            .iter()
            .map(|(name, content)| Memo {
                name: name.clone(),
                content: content.clone(),
            })
            .collect())
    }

    fn get_memo(&self, name: &str) -> Result<Option<Memo>> {
        self.gets.set(self.gets.get() + 1);
        if self.gets.get() == 2
            && let Some((path, content)) = self.edit_on_second_get.borrow_mut().take()
        {
            fs::write(path, content)?;
        }
        Ok(self.memos.borrow().get(name).map(|content| Memo {
            name: name.into(),
            content: content.clone(),
        }))
    }

    fn create_memo(&self, id: &str, content: &str) -> Result<Memo> {
        self.creates.set(self.creates.get() + 1);
        self.written(&format!("memos/{id}"), content)
    }

    fn update_memo(&self, name: &str, content: &str) -> Result<Memo> {
        self.updates.set(self.updates.get() + 1);
        self.written(name, content)
    }
}

fn seed(store: &Store, base: Option<&str>, pending: Option<PendingWrite>, local: &str) {
    store
        .write(
            std::path::Path::new("test/note.md"),
            &storage::render("memos/test", local),
        )
        .unwrap();
    let state = State {
        version: 1,
        identity: "test-instance#users/1".into(),
        memos: BTreeMap::from([(
            "memos/test".into(),
            Record {
                path: "test/note.md".into(),
                base: base.map(str::to_owned),
                pending_write: pending,
                conflict: None,
                remote_seen: base.is_some(),
                cleanup_pending: false,
            },
        )]),
    };
    store.save(&state).unwrap();
}

#[test]
fn unknown_header_after_lost_state_never_overwrites_either_version() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap().unwrap();
    let local = storage::render("memos/test", "Vim copy");
    store
        .write(std::path::Path::new("test/note.md"), &local)
        .unwrap();
    let api = FakeApi::default();
    api.put("memos/test", "Phone copy");
    let report = synchronize(&api, &store).unwrap();
    assert!(report.errors.is_empty());
    assert_eq!(report.conflicts.len(), 1);
    assert_eq!(api.creates.get() + api.updates.get(), 0);
    assert_eq!(
        fs::read_to_string(dir.path().join("test/note.md")).unwrap(),
        local
    );
    assert_eq!(api.memos.borrow()["memos/test"], "Phone copy");
}

#[test]
fn corrupt_history_fails_before_mutating_notes_or_server() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap().unwrap();
    store
        .write(std::path::Path::new("new/note.md"), "new local note")
        .unwrap();
    store
        .write(std::path::Path::new(".memos/state.json"), "{broken")
        .unwrap();
    let api = FakeApi::default();
    assert!(synchronize(&api, &store).is_err());
    assert_eq!(api.creates.get() + api.updates.get(), 0);
    assert_eq!(
        fs::read_to_string(dir.path().join("new/note.md")).unwrap(),
        "new local note"
    );
    assert_eq!(
        fs::read_to_string(dir.path().join(".memos/state.json")).unwrap(),
        "{broken"
    );
}

#[test]
fn duplicate_identity_fails_before_any_note_mutation() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap().unwrap();
    let body = storage::render("memos/test", "copy");
    for path in ["a/note.md", "b/note.md"] {
        store.write(std::path::Path::new(path), &body).unwrap();
    }
    let api = FakeApi::default();
    api.put("memos/test", "remote");
    assert!(synchronize(&api, &store).is_err());
    assert_eq!(api.creates.get() + api.updates.get(), 0);
    assert!(!dir.path().join(".memos/state.json").exists());
    assert_eq!(
        fs::read_to_string(dir.path().join("a/note.md")).unwrap(),
        body
    );
}

#[test]
fn incomplete_remote_listing_does_not_assign_ids_or_touch_existing_state() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap().unwrap();
    seed(&store, Some("base"), None, "local");
    let before = fs::read(dir.path().join(".memos/state.json")).unwrap();
    store
        .write(std::path::Path::new("new/note.md"), "new local note")
        .unwrap();
    let api = FakeApi::default();
    api.fail_list.set(true);
    assert!(synchronize(&api, &store).is_err());
    assert_eq!(api.creates.get() + api.updates.get(), 0);
    assert_eq!(
        fs::read(dir.path().join(".memos/state.json")).unwrap(),
        before
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("new/note.md")).unwrap(),
        "new local note"
    );
}

#[test]
fn successful_write_with_lost_response_is_not_sent_twice() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap().unwrap();
    seed(&store, Some("base"), None, "local edit");
    let api = FakeApi::default();
    api.put("memos/test", "base");
    api.fail_after_write.set(true);
    let first = synchronize(&api, &store).unwrap();
    assert_eq!(first.errors.len(), 1);
    assert_eq!(api.memos.borrow()["memos/test"], "local edit");
    let second = synchronize(&api, &store).unwrap();
    assert!(second.errors.is_empty());
    assert!(second.conflicts.is_empty());
    assert_eq!(api.updates.get(), 1);
    let state = store.load("test-instance#users/1").unwrap();
    assert_eq!(
        state.memos["memos/test"].base.as_deref(),
        Some("local edit")
    );
    assert!(state.memos["memos/test"].pending_write.is_none());
}

#[test]
fn ambiguous_create_reuses_its_persisted_id_and_latest_local_content() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap().unwrap();
    seed(
        &store,
        None,
        Some(PendingWrite {
            content: "old attempt".into(),
            create: true,
        }),
        "latest edit",
    );
    let api = FakeApi::default();
    let report = synchronize(&api, &store).unwrap();
    assert!(report.errors.is_empty());
    assert_eq!(api.creates.get(), 1);
    assert_eq!(api.memos.borrow()["memos/test"], "latest edit");
    assert_eq!(api.memos.borrow().len(), 1);
}

#[test]
fn local_edit_during_remote_recheck_is_preserved_and_upload_deferred() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap().unwrap();
    seed(&store, Some("base"), None, "first local edit");
    let api = FakeApi::default();
    api.put("memos/test", "base");
    *api.edit_on_second_get.borrow_mut() = Some((
        dir.path().join("test/note.md"),
        storage::render("memos/test", "second local edit"),
    ));
    let report = synchronize(&api, &store).unwrap();
    assert_eq!(report.errors.len(), 1);
    assert_eq!(api.updates.get(), 0);
    assert_eq!(api.memos.borrow()["memos/test"], "base");
    let (_, local) =
        storage::parse(&fs::read_to_string(dir.path().join("test/note.md")).unwrap()).unwrap();
    assert_eq!(local, "second local edit");
}

#[test]
fn pending_create_observed_remotely_cannot_resurrect_after_remote_deletion() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap().unwrap();
    seed(
        &store,
        None,
        Some(PendingWrite {
            content: "attempted".into(),
            create: true,
        }),
        "attempted",
    );
    let api = FakeApi::default();
    api.put("memos/test", "changed on phone after create");
    let first = synchronize(&api, &store).unwrap();
    assert!(first.errors.is_empty());
    assert_eq!(api.creates.get(), 0);
    api.memos.borrow_mut().remove("memos/test");
    let second = synchronize(&api, &store).unwrap();
    assert!(second.errors.is_empty());
    assert_eq!(api.creates.get(), 0, "a remote deletion was resurrected");
    assert!(!api.memos.borrow().contains_key("memos/test"));
    assert_eq!(second.trashed, 1);
}

#[test]
fn stale_conflict_artifacts_are_cleaned_after_interrupted_successful_resolution() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap().unwrap();
    seed(&store, Some("resolved"), None, "resolved");
    let mut state = store.load("test-instance#users/1").unwrap();
    state.memos.get_mut("memos/test").unwrap().cleanup_pending = true;
    store.save(&state).unwrap();
    store
        .write(std::path::Path::new("test/conflict.diff"), "stale diff")
        .unwrap();
    store
        .write(std::path::Path::new("test/remote.md"), "stale remote")
        .unwrap();
    let api = FakeApi::default();
    api.put("memos/test", "resolved");
    let report = synchronize(&api, &store).unwrap();
    assert!(report.errors.is_empty());
    assert!(report.conflicts.is_empty());
    assert_eq!(api.updates.get() + api.creates.get(), 0);
    assert!(!dir.path().join("test/conflict.diff").exists());
    assert!(!dir.path().join("test/remote.md").exists());
    assert!(!store.load("test-instance#users/1").unwrap().memos["memos/test"].cleanup_pending);
}

#[test]
fn swapping_two_memo_folder_names_preserves_both_identities() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap().unwrap();
    let api = FakeApi::default();
    api.put("memos/a", "Apple");
    api.put("memos/b", "Banana");
    assert!(synchronize(&api, &store).unwrap().errors.is_empty());
    let state = store.load("test-instance#users/1").unwrap();
    let original_a = state.memos["memos/a"].path.clone();
    let original_b = state.memos["memos/b"].path.clone();
    let parent_a = dir.path().join(original_a.parent().unwrap());
    let parent_b = dir.path().join(original_b.parent().unwrap());
    let temporary = dir.path().join("temporary");
    fs::rename(&parent_a, &temporary).unwrap();
    fs::rename(&parent_b, &parent_a).unwrap();
    fs::rename(&temporary, &parent_b).unwrap();
    let report = synchronize(&api, &store).unwrap();
    assert!(report.errors.is_empty());
    assert!(report.conflicts.is_empty());
    assert_eq!(report.downloaded + report.uploaded, 0);
    assert_eq!(api.updates.get() + api.creates.get(), 0);
    let state = store.load("test-instance#users/1").unwrap();
    assert_eq!(state.memos["memos/a"].path, original_b);
    assert_eq!(state.memos["memos/b"].path, original_a);
}

#[test]
fn moving_only_a_conflicted_note_republishes_marker_instead_of_uploading() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap().unwrap();
    seed(&store, Some("base"), None, "Vim edit");
    let api = FakeApi::default();
    api.put("memos/test", "Phone edit");
    let first = synchronize(&api, &store).unwrap();
    assert!(first.errors.is_empty());
    assert_eq!(first.conflicts.len(), 1);
    fs::create_dir(dir.path().join("moved")).unwrap();
    fs::rename(
        dir.path().join("test/note.md"),
        dir.path().join("moved/note.md"),
    )
    .unwrap();
    let second = synchronize(&api, &store).unwrap();
    assert!(second.errors.is_empty());
    assert_eq!(second.conflicts, [store.root.join("moved/conflict.diff")]);
    assert!(dir.path().join("moved/conflict.diff").exists());
    assert!(dir.path().join("moved/remote.md").exists());
    assert_eq!(api.creates.get() + api.updates.get(), 0);
    assert_eq!(api.memos.borrow()["memos/test"], "Phone edit");
    let (_, local) =
        storage::parse(&fs::read_to_string(dir.path().join("moved/note.md")).unwrap()).unwrap();
    assert_eq!(local, "Vim edit");
}
