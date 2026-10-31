use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;
use tempfile::TempDir;
use tiny_http::{Header, Response, Server, StatusCode};

#[derive(Clone)]
struct TestMemo {
    content: String,
    state: &'static str,
}

#[derive(Default)]
struct Database {
    memos: BTreeMap<String, TestMemo>,
    creates: usize,
    updates: usize,
    fail_list: bool,
    paginate: bool,
    fail_second_page: bool,
    fail_create_after_commit: bool,
    fail_update_after_commit: bool,
    fail_get: Option<String>,
    create_content_limit: Option<usize>,
}

struct Instance {
    url: String,
    db: Arc<Mutex<Database>>,
    stopped: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

impl Instance {
    fn new() -> Self {
        let server = Server::http("127.0.0.1:0").unwrap();
        let url = format!("http://{}", server.server_addr());
        let db = Arc::new(Mutex::new(Database::default()));
        let stopped = Arc::new(AtomicBool::new(false));
        let worker_db = Arc::clone(&db);
        let worker_stopped = Arc::clone(&stopped);
        let worker = thread::spawn(move || {
            while !worker_stopped.load(Ordering::Relaxed) {
                let Some(mut request) = server.recv_timeout(Duration::from_millis(20)).unwrap()
                else {
                    continue;
                };
                let authorized = request.headers().iter().any(|header| {
                    header.field.equiv("Authorization")
                        && header.value.as_str() == "Bearer test-token"
                });
                let method = request.method().as_str().to_owned();
                let url =
                    reqwest::Url::parse(&format!("http://localhost{}", request.url())).unwrap();
                let params: BTreeMap<_, _> = url.query_pairs().into_owned().collect();
                let mut body = String::new();
                if request.as_reader().read_to_string(&mut body).is_err() {
                    continue;
                }
                let mut db = worker_db.lock().unwrap();
                let (status, response) = if !authorized {
                    (401, json!({"message": "invalid test token"}))
                } else {
                    respond(&mut db, &method, url.path(), &params, &body)
                };
                drop(db);
                let response = Response::from_string(response.to_string())
                    .with_status_code(StatusCode(status))
                    .with_header(Header::from_bytes("Content-Type", "application/json").unwrap());
                let _ = request.respond(response);
            }
        });
        Self {
            url,
            db,
            stopped,
            worker: Some(worker),
        }
    }

    fn put(&self, id: &str, content: &str) {
        self.db.lock().unwrap().memos.insert(
            id.to_owned(),
            TestMemo {
                content: content.to_owned(),
                state: "NORMAL",
            },
        );
    }

    fn content(&self, id: &str) -> String {
        self.db.lock().unwrap().memos[id].content.clone()
    }

    fn command(&self, local: &TempDir, operation: &str) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_memos"));
        command
            .arg(operation)
            .current_dir(local.path())
            .env("MEMOS_URL", &self.url)
            .env("TOKEN", "test-token")
            .env("DATA_PATH", local.path().join("data"));
        command
    }

    fn sync(&self, local: &TempDir, expected_exit: i32) -> Output {
        let output = self.command(local, "sync").output().unwrap();
        assert_eq!(
            output.status.code(),
            Some(expected_exit),
            "stdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        output
    }
}

impl Drop for Instance {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Relaxed);
        let _ = self.worker.take().unwrap().join();
    }
}

fn wire_memo(id: &str, memo: &TestMemo) -> Value {
    json!({
        "name": format!("memos/{id}"),
        "content": memo.content,
        "creator": "users/1",
        "state": memo.state,
        "visibility": "PRIVATE",
        "createTime": "2026-01-01T00:00:00Z",
        "updateTime": "2026-01-01T00:00:00Z",
    })
}

fn respond(
    db: &mut Database,
    method: &str,
    path: &str,
    params: &BTreeMap<String, String>,
    body: &str,
) -> (u16, Value) {
    if method == "GET" && path == "/api/v1/instance/profile" {
        return (200, json!({"version": "0.31.0"}));
    }
    if method == "GET" && path == "/api/v1/auth/me" {
        return (200, json!({"user": {"name": "users/1"}}));
    }
    if method == "GET" && path == "/api/v1/memos" {
        if db.fail_list
            || (db.fail_second_page && params.get("pageToken").is_some_and(|p| p == "next"))
        {
            return (503, json!({"message": "injected listing failure"}));
        }
        if params.get("filter").map(String::as_str) != Some("creator == \"users/1\"") {
            return (400, json!({"message": "creator filter is required"}));
        }
        let state = params.get("state").map_or("NORMAL", String::as_str);
        let memos: Vec<_> = db
            .memos
            .iter()
            .filter(|(_, memo)| memo.state == state)
            .map(|(id, memo)| wire_memo(id, memo))
            .collect();
        if db.paginate && memos.len() > 1 {
            return if params.get("pageToken").is_some_and(|token| token == "next") {
                (200, json!({"memos": &memos[1..], "nextPageToken": ""}))
            } else {
                (200, json!({"memos": &memos[..1], "nextPageToken": "next"}))
            };
        }
        return (200, json!({"memos": memos, "nextPageToken": ""}));
    }
    if method == "POST" && path == "/api/v1/memos" {
        let Some(id) = params.get("memoId") else {
            return (400, json!({"message": "client-generated ID is required"}));
        };
        if db.memos.contains_key(id) {
            return (409, json!({"message": "memo already exists"}));
        }
        let Ok(body) = serde_json::from_str::<Value>(body) else {
            return (400, json!({"message": "invalid create body"}));
        };
        if body["visibility"] != "PRIVATE" || !body["content"].is_string() {
            return (400, json!({"message": "expected private Markdown memo"}));
        }
        let content = body["content"].as_str().unwrap();
        if let Some(limit) = db.create_content_limit
            && content.len() > limit
        {
            return (
                400,
                json!({"code": 3, "message": format!("content too long (max {limit} characters)")}),
            );
        }
        let memo = TestMemo {
            content: content.to_owned(),
            state: "NORMAL",
        };
        let response = wire_memo(id, &memo);
        db.memos.insert(id.clone(), memo);
        db.creates += 1;
        if std::mem::take(&mut db.fail_create_after_commit) {
            return (500, json!({"message": "response lost after commit"}));
        }
        return (200, response);
    }
    if let Some(id) = path.strip_prefix("/api/v1/memos/") {
        if method == "GET" && db.fail_get.as_deref() == Some(id) {
            return (503, json!({"message": "injected per-memo failure"}));
        }
        let Some(memo) = db.memos.get_mut(id) else {
            return (404, json!({"message": "memo not found"}));
        };
        if method == "GET" {
            return (200, wire_memo(id, memo));
        }
        if method == "PATCH" {
            if params.get("updateMask").map(String::as_str) != Some("content") {
                return (400, json!({"message": "content-only update mask required"}));
            }
            let Ok(body) = serde_json::from_str::<Value>(body) else {
                return (400, json!({"message": "invalid update body"}));
            };
            if !body["content"].is_string()
                || !body.as_object().is_some_and(|object| {
                    object.keys().all(|key| key == "name" || key == "content")
                })
            {
                return (400, json!({"message": "only content may be updated"}));
            }
            memo.content = body["content"].as_str().unwrap().to_owned();
            let response = wire_memo(id, memo);
            db.updates += 1;
            if std::mem::take(&mut db.fail_update_after_commit) {
                return (500, json!({"message": "update response lost after commit"}));
            }
            return (200, response);
        }
    }
    (404, json!({"message": "unexpected API route"}))
}

fn notes(root: &Path) -> Vec<PathBuf> {
    let mut result = Vec::new();
    let Ok(entries) = fs::read_dir(root) else {
        return result;
    };
    for entry in entries {
        let entry = entry.unwrap();
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        if entry.file_type().unwrap().is_dir() {
            result.extend(notes(&entry.path()));
        } else if entry.file_name() == "note.md" {
            result.push(entry.path());
        }
    }
    result.sort();
    result
}

fn note(local: &TempDir) -> PathBuf {
    let found = notes(&local.path().join("data"));
    assert_eq!(found.len(), 1, "expected one note, found {found:?}");
    found[0].clone()
}

fn content(path: &Path) -> String {
    let text = fs::read_to_string(path).unwrap();
    let (_, body) = text
        .strip_prefix("---\n")
        .and_then(|rest| rest.split_once("\n---\n"))
        .expect("note must have a metadata header");
    body.to_owned()
}

fn edit(path: &Path, body: &str) {
    let text = fs::read_to_string(path).unwrap();
    let (header, _) = text.split_once("\n---\n").unwrap();
    fs::write(path, format!("{header}\n---\n{body}")).unwrap();
}

#[test]
fn sync_round_trips_markdown_and_handles_renames_and_deletions() {
    let server = Instance::new();
    let local = TempDir::new().unwrap();
    let original =
        "---\nkind: note\n---\n\n# Groceries 🥕\n\n- [ ] milk\n```sh\nprintf '%s' hello\n```";
    server.put("groceries", original);
    server.sync(&local, 0);
    let path = note(&local);
    assert_eq!(content(&path), original);
    server.sync(&local, 0);
    assert_eq!(server.db.lock().unwrap().updates, 0);

    edit(&path, "# Edited in Vim\n");
    server.sync(&local, 0);
    assert_eq!(server.content("groceries"), "# Edited in Vim\n");
    server.put("groceries", "# Edited on phone\n");
    server.sync(&local, 0);
    assert_eq!(content(&path), "# Edited on phone\n");

    let renamed = local.path().join("data/renamed-by-user");
    fs::rename(path.parent().unwrap(), &renamed).unwrap();
    let path = renamed.join("note.md");
    edit(&path, "A note after renaming");
    server.sync(&local, 0);
    assert_eq!(server.content("groceries"), "A note after renaming");
    assert_eq!(note(&local), path);

    fs::remove_file(&path).unwrap();
    server.sync(&local, 0);
    assert_eq!(content(&path), "A note after renaming");
    server.db.lock().unwrap().memos.remove("groceries");
    edit(&path, "unsynced local work to preserve");
    server.sync(&local, 0);
    assert!(notes(&local.path().join("data")).is_empty());
    let trash = local.path().join("data/.memos/trash");
    assert!(fs::read_dir(&trash).unwrap().next().is_some());
    fn has_saved_content(path: &Path) -> bool {
        fs::read_dir(path).unwrap().any(|entry| {
            let path = entry.unwrap().path();
            if path.is_dir() {
                has_saved_content(&path)
            } else {
                fs::read_to_string(path)
                    .is_ok_and(|text| text.contains("unsynced local work to preserve"))
            }
        })
    }
    assert!(has_saved_content(&trash));
}

#[test]
fn conflicts_require_acknowledgment_of_the_latest_remote_version() {
    let server = Instance::new();
    let local = TempDir::new().unwrap();
    server.put("conflict", "base\n");
    server.sync(&local, 0);
    let path = note(&local);
    let folder = path.parent().unwrap();

    edit(&path, "same change\n");
    server.put("conflict", "same change\n");
    server.sync(&local, 0);
    assert_eq!(server.db.lock().unwrap().updates, 0);
    edit(&path, "local change\n");
    server.put("conflict", "phone change\n");
    server.sync(&local, 2);
    assert_eq!(content(&path), "local change\n");
    assert_eq!(content(&folder.join("remote.md")), "phone change\n");
    let diff = fs::read_to_string(folder.join("conflict.diff")).unwrap();
    assert!(diff.contains("-local change"));
    assert!(diff.contains("+phone change"));

    server.put("conflict", "newer phone change\n");
    server.sync(&local, 2);
    assert_eq!(
        fs::read_to_string(folder.join("conflict.diff")).unwrap(),
        diff
    );
    edit(&path, "merged with first phone version\n");
    fs::remove_file(folder.join("conflict.diff")).unwrap();
    server.sync(&local, 2);
    assert_eq!(server.content("conflict"), "newer phone change\n");
    assert_eq!(content(&path), "merged with first phone version\n");
    assert_eq!(content(&folder.join("remote.md")), "newer phone change\n");

    edit(&path, "fully merged\n");
    fs::remove_file(folder.join("conflict.diff")).unwrap();
    server.sync(&local, 0);
    assert_eq!(server.content("conflict"), "fully merged\n");
    assert!(!folder.join("remote.md").exists());
    assert!(!folder.join("conflict.diff").exists());
    server.sync(&local, 0);
    assert_eq!(server.db.lock().unwrap().updates, 1);

    edit(&path, "keep local unchanged\n");
    server.put("conflict", "different remote\n");
    server.sync(&local, 2);
    fs::remove_file(folder.join("conflict.diff")).unwrap();
    server.sync(&local, 0);
    assert_eq!(server.content("conflict"), "keep local unchanged\n");
}

#[test]
fn removing_a_conflicted_folder_restores_remote_and_preserves_local_work() {
    let server = Instance::new();
    let local = TempDir::new().unwrap();
    server.put("recover", "base");
    server.sync(&local, 0);
    let path = note(&local);
    edit(&path, "precious local work");
    server.put("recover", "remote edit");
    server.sync(&local, 2);
    fs::remove_dir_all(path.parent().unwrap()).unwrap();
    server.sync(&local, 2);
    let restored = note(&local);
    let folder = restored.parent().unwrap();
    assert_eq!(content(&restored), "remote edit");
    assert!(
        fs::read_to_string(folder.join("local.md"))
            .unwrap()
            .contains("precious local work")
    );
    assert!(folder.join("conflict.diff").exists());
    assert_eq!(server.db.lock().unwrap().updates, 0);
}

#[test]
fn new_notes_reuse_their_id_after_an_ambiguous_create_response() {
    let server = Instance::new();
    let local = TempDir::new().unwrap();
    let path = local.path().join("data/new-note/note.md");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, "# New note\n\nCreated locally.").unwrap();
    server.db.lock().unwrap().fail_create_after_commit = true;
    server.sync(&local, 1);
    let after_failure = fs::read_to_string(&path).unwrap();
    assert!(after_failure.contains("memos_id:"));
    server.sync(&local, 0);
    assert_eq!(fs::read_to_string(&path).unwrap(), after_failure);
    let db = server.db.lock().unwrap();
    assert_eq!(db.creates, 1);
    assert_eq!(db.memos.len(), 1);
    assert_eq!(
        db.memos.values().next().unwrap().content,
        "# New note\n\nCreated locally."
    );
}

#[test]
fn oversized_unicode_notes_report_server_byte_limits_and_retry_without_losing_content() {
    let server = Instance::new();
    let local = TempDir::new().unwrap();
    let path = local.path().join("data/dogs/note.md");
    let body = format!("# Dogs 🐕\n\n{}", "🐕".repeat(2200));
    assert!(body.chars().count() < 8192);
    assert!(body.len() > 8192);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, &body).unwrap();
    server.db.lock().unwrap().create_content_limit = Some(8192);

    let output = server.sync(&local, 1);
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("content too long (max 8192 characters)"));
    let after_failure = fs::read_to_string(&path).unwrap();
    assert!(after_failure.contains("memos_id:"));
    assert_eq!(content(&path), body);
    {
        let db = server.db.lock().unwrap();
        assert_eq!(db.creates, 0);
        assert!(db.memos.is_empty());
    }

    server.db.lock().unwrap().create_content_limit = Some(65536);
    server.sync(&local, 0);
    assert_eq!(fs::read_to_string(&path).unwrap(), after_failure);
    let db = server.db.lock().unwrap();
    assert_eq!(db.creates, 1);
    assert_eq!(db.memos.len(), 1);
    assert_eq!(db.memos.values().next().unwrap().content, body);
}

#[test]
fn interrupted_upload_is_reconciled_without_repeating_the_write() {
    let server = Instance::new();
    let local = TempDir::new().unwrap();
    server.put("update", "base");
    server.sync(&local, 0);
    let path = note(&local);
    edit(&path, "updated locally");
    server.db.lock().unwrap().fail_update_after_commit = true;
    server.sync(&local, 1);
    assert_eq!(server.content("update"), "updated locally");
    server.sync(&local, 0);
    assert_eq!(server.db.lock().unwrap().updates, 1);
    assert!(!path.parent().unwrap().join("conflict.diff").exists());
}

#[test]
fn pagination_includes_archived_notes_and_failed_lists_never_remove_files() {
    let server = Instance::new();
    let local = TempDir::new().unwrap();
    server.put("first", "first page");
    server.put("second", "second page");
    server.put("archived", "archived note");
    {
        let mut db = server.db.lock().unwrap();
        db.paginate = true;
        db.memos.get_mut("archived").unwrap().state = "ARCHIVED";
    }
    server.sync(&local, 0);
    let paths = notes(&local.path().join("data"));
    assert_eq!(paths.len(), 3);
    let saved: Vec<_> = paths.iter().map(|path| fs::read(path).unwrap()).collect();
    server.db.lock().unwrap().fail_second_page = true;
    server.sync(&local, 1);
    for (path, before) in paths.iter().zip(&saved) {
        assert_eq!(fs::read(path).unwrap(), *before);
    }
    {
        let mut db = server.db.lock().unwrap();
        db.fail_second_page = false;
        db.fail_list = true;
    }
    server.sync(&local, 1);
    assert_eq!(notes(&local.path().join("data")).len(), 3);
    assert_eq!(server.db.lock().unwrap().updates, 0);
}

#[test]
fn corrupt_state_and_duplicate_ids_fail_without_remote_writes() {
    let server = Instance::new();
    let local = TempDir::new().unwrap();
    server.put("safe", "original");
    server.sync(&local, 0);
    let path = note(&local);
    let copy = local.path().join("data/copied/note.md");
    fs::create_dir_all(copy.parent().unwrap()).unwrap();
    fs::copy(&path, &copy).unwrap();
    edit(&copy, "must not be uploaded");
    server.sync(&local, 1);
    assert_eq!(server.content("safe"), "original");
    fs::remove_dir_all(copy.parent().unwrap()).unwrap();
    fs::write(local.path().join("data/.memos/state.json"), "broken JSON").unwrap();
    edit(&path, "also must not be uploaded");
    server.sync(&local, 1);
    assert_eq!(server.content("safe"), "original");
    assert_eq!(server.db.lock().unwrap().updates, 0);
}

#[test]
fn overlapping_sync_exits_successfully_without_modifying_notes() {
    let server = Instance::new();
    let local = TempDir::new().unwrap();
    server.put("locked", "original");
    server.sync(&local, 0);
    let path = note(&local);
    edit(&path, "waiting for the next sync");
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(local.path().join("data/.memos/lock"))
        .unwrap();
    fs2::FileExt::lock_exclusive(&lock).unwrap();
    server.sync(&local, 0);
    assert_eq!(server.content("locked"), "original");
    assert_eq!(content(&path), "waiting for the next sync");
    drop(lock);
    server.sync(&local, 0);
    assert_eq!(server.content("locked"), "waiting for the next sync");
}

#[test]
fn errors_take_precedence_over_conflicts_and_other_notes_still_sync() {
    let server = Instance::new();
    let local = TempDir::new().unwrap();
    server.put("conflicting", "conflict baseline");
    server.put("failing", "failure baseline");
    server.put("healthy", "healthy baseline");
    server.sync(&local, 0);
    let paths = notes(&local.path().join("data"));
    let conflict = paths
        .iter()
        .find(|path| content(path) == "conflict baseline")
        .unwrap();
    let healthy = paths
        .iter()
        .find(|path| content(path) == "healthy baseline")
        .unwrap();
    edit(conflict, "local conflict");
    edit(healthy, "healthy local edit");
    server.put("conflicting", "remote conflict");
    server.db.lock().unwrap().fail_get = Some("failing".to_owned());
    let output = server.sync(&local, 1);
    let summary = String::from_utf8_lossy(&output.stdout);
    assert!(summary.contains("conflicts: 1") && summary.contains("errors: 1"));
    assert!(conflict.parent().unwrap().join("conflict.diff").exists());
    assert_eq!(server.content("healthy"), "healthy local edit");
    assert_eq!(server.content("conflicting"), "remote conflict");
    server.db.lock().unwrap().fail_get = None;
    server.sync(&local, 2);
}

#[test]
fn help_works_without_configuration_and_run_is_rejected() {
    let local = TempDir::new().unwrap();
    // Help must exit before loading credentials or parsing even a malformed .env.
    fs::write(local.path().join(".env"), "TOKEN=\"unterminated").unwrap();
    for args in [
        vec!["help"],
        vec!["--help"],
        vec!["help", "sync"],
        vec!["sync", "--help"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_memos"))
            .args(&args)
            .env_remove("MEMOS_URL")
            .env_remove("TOKEN")
            .env_remove("DATA_PATH")
            .current_dir(local.path())
            .output()
            .unwrap();
        assert!(output.status.success(), "help failed for {args:?}");
        let text = String::from_utf8_lossy(&output.stdout);
        for expected in [
            "Usage:",
            "MEMOS_URL",
            "TOKEN",
            "DATA_PATH",
            "note.md",
            "conflict.diff",
        ] {
            assert!(text.contains(expected), "missing {expected} for {args:?}");
        }
        assert!(output.stderr.is_empty());
    }
    let help = Command::new(env!("CARGO_BIN_EXE_memos"))
        .arg("--help")
        .current_dir(local.path())
        .output()
        .unwrap();
    assert!(help.status.success());
    let help = String::from_utf8_lossy(&help.stdout);
    assert!(
        help.lines()
            .any(|line| line.trim_start().starts_with("sync "))
    );
    assert!(
        !help
            .lines()
            .any(|line| line.trim_start().starts_with("run "))
    );
    let rejected = Command::new(env!("CARGO_BIN_EXE_memos"))
        .arg("run")
        .current_dir(local.path())
        .output()
        .unwrap();
    assert_eq!(rejected.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("unrecognized subcommand"));
}

fn command_without_config(local: &TempDir) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_memos"));
    command
        .arg("sync")
        .current_dir(local.path())
        .env_remove("MEMOS_URL")
        .env_remove("TOKEN")
        .env_remove("DATA_PATH");
    command
}

fn assert_sync_success(output: &Output) {
    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn explicit_configuration_works_without_environment_or_a_valid_dotenv() {
    let server = Instance::new();
    let local = TempDir::new().unwrap();
    server.put("flags", "configured explicitly");
    let data = local.path().join("explicit-data");
    for dotenv in [None, Some("MEMOS_URL=\"unterminated\n")] {
        if let Some(dotenv) = dotenv {
            fs::write(local.path().join(".env"), dotenv).unwrap();
        }
        let output = command_without_config(&local)
            .arg(format!("--url={}", server.url))
            .arg("--token=test-token")
            .arg(format!("--data-path={}", data.display()))
            .output()
            .unwrap();
        assert_sync_success(&output);
        let downloaded = notes(&data);
        assert_eq!(downloaded.len(), 1);
        assert_eq!(content(&downloaded[0]), "configured explicitly");
    }
}

#[test]
fn explicit_flags_override_conflicting_environment_and_dotenv_values() {
    let server = Instance::new();
    let local = TempDir::new().unwrap();
    server.put("precedence", "correct server selected");
    fs::write(
        local.path().join(".env"),
        "MEMOS_URL=invalid-dotenv-url\nTOKEN=wrong-dotenv-token\nDATA_PATH=wrong-dotenv-data\n",
    )
    .unwrap();
    let data = local.path().join("selected-data");
    let output = command_without_config(&local)
        .env("MEMOS_URL", "invalid-process-url")
        .env("TOKEN", "wrong-process-token")
        .env("DATA_PATH", "wrong-process-data")
        .arg(format!("--url={}", server.url))
        .arg("--token=test-token")
        .arg(format!("--data-path={}", data.display()))
        .output()
        .unwrap();
    assert_sync_success(&output);
    assert_eq!(notes(&data).len(), 1);
    assert!(!local.path().join("wrong-dotenv-data").exists());
    assert!(!local.path().join("wrong-process-data").exists());
}

#[test]
fn missing_flags_fall_back_to_environment_then_dotenv_for_each_setting() {
    let server = Instance::new();
    server.put("mixed", "mixed configuration");
    for scenario in 0..3 {
        let local = TempDir::new().unwrap();
        fs::write(
            local.path().join(".env"),
            format!(
                "MEMOS_URL={}\nTOKEN=test-token\nDATA_PATH=dotenv-data\n",
                server.url
            ),
        )
        .unwrap();
        let mut command = command_without_config(&local);
        let directory = match scenario {
            0 => {
                command
                    .arg("--token=test-token")
                    .env("TOKEN", "overridden-process-token")
                    .env("MEMOS_URL", &server.url);
                "dotenv-data"
            }
            1 => {
                command
                    .arg("--data-path=flag-data")
                    .env("DATA_PATH", "overridden-process-data")
                    .env("TOKEN", "test-token");
                "flag-data"
            }
            _ => {
                command
                    .arg(format!("--url={}", server.url))
                    .env("MEMOS_URL", "overridden-process-url")
                    .env("DATA_PATH", "process-data");
                "process-data"
            }
        };
        let output = command.output().unwrap();
        assert_sync_success(&output);
        let downloaded = notes(&local.path().join(directory));
        assert_eq!(downloaded.len(), 1);
        assert_eq!(content(&downloaded[0]), "mixed configuration");
    }
}

#[test]
fn empty_configuration_flags_fail_instead_of_falling_back_to_valid_environment() {
    let server = Instance::new();
    let local = TempDir::new().unwrap();
    server.put("untouched", "remote note");
    for flag in ["--url=", "--token=", "--data-path="] {
        let output = server.command(&local, "sync").arg(flag).output().unwrap();
        assert!(!output.status.success(), "accepted empty flag {flag}");
        assert!(!local.path().join("data").exists());
    }
    let db = server.db.lock().unwrap();
    assert_eq!(db.creates, 0);
    assert_eq!(db.updates, 0);
    assert_eq!(db.memos.len(), 1);
}
