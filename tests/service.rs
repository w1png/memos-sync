#![cfg(any(target_os = "macos", target_os = "linux"))]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use tempfile::TempDir;

const BEGIN: &str = "# BEGIN memos-sync";
const END: &str = "# END memos-sync";

struct Fixture {
    _temp: TempDir,
    root: PathBuf,
    work: PathBuf,
    state: PathBuf,
    cron: PathBuf,
    token: String,
    marker: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = TempDir::new().unwrap();
        // macOS's system temporary directory may be reached through /var's symlink.
        let root = fs::canonicalize(temp.path()).unwrap();
        let work = root.join("work");
        let state = root.join("state with spaces");
        let bin = root.join("bin");
        let cron = root.join("crontab.txt");
        let marker = root.join("must-not-execute");
        let token = format!("test-token'$(touch {})/$HOME", marker.display());
        fs::create_dir_all(&work).unwrap();
        fs::create_dir_all(&bin).unwrap();
        fs::create_dir_all(root.join("home")).unwrap();
        let mock = bin.join("crontab");
        fs::write(
            &mock,
            r#"#!/bin/sh
case "$1" in
  -l)
    if [ "${MOCK_CRONTAB_READ_ERROR:-}" = 1 ]; then
      echo 'crontab: permission denied' >&2
      exit 2
    fi
    if [ ! -f "$MOCK_CRONTAB_FILE" ]; then
      echo 'no crontab for test-user' >&2
      exit 1
    fi
    /bin/cat "$MOCK_CRONTAB_FILE"
    ;;
  -)
    if [ "${MOCK_CRONTAB_WRITE_ERROR:-}" = 1 ]; then
      echo 'crontab: write failed' >&2
      exit 2
    fi
    /bin/cat > "$MOCK_CRONTAB_FILE"
    ;;
  *)
    echo 'unexpected mock crontab arguments' >&2
    exit 2
    ;;
esac
"#,
        )
        .unwrap();
        fs::set_permissions(mock, fs::Permissions::from_mode(0o700)).unwrap();
        let quoted_token = token
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('$', "\\$");
        fs::write(
            work.join(".env"),
            format!(
                "MEMOS_URL=https://example.invalid\nTOKEN=\"{quoted_token}\"\nDATA_PATH=\"./notes space\"\n"
            ),
        )
        .unwrap();
        Self {
            _temp: temp,
            root,
            work,
            state,
            cron,
            token,
            marker,
        }
    }

    fn command(&self, operation: &str) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_memos"));
        command
            .args(["service", operation])
            .current_dir(&self.work)
            .env_remove("MEMOS_URL")
            .env_remove("TOKEN")
            .env_remove("DATA_PATH")
            .env_remove("MOCK_CRONTAB_READ_ERROR")
            .env_remove("MOCK_CRONTAB_WRITE_ERROR")
            .env("HOME", self.root.join("home"))
            .env("XDG_STATE_HOME", &self.state)
            .env("MOCK_CRONTAB_FILE", &self.cron)
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", self.root.join("bin").display()),
            );
        command
    }

    fn run(&self, operation: &str) -> Output {
        let output = self.command(operation).output().unwrap();
        assert_success(&output);
        output
    }

    fn script(&self) -> PathBuf {
        self.state.join("memos/sync.sh")
    }

    fn cron(&self) -> String {
        fs::read_to_string(&self.cron).unwrap()
    }
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn assert_private(path: &Path) {
    assert_eq!(
        fs::metadata(path).unwrap().permissions().mode() & 0o777,
        0o700
    );
}

#[test]
fn install_is_idempotent_preserves_cron_and_quotes_private_configuration() {
    let fixture = Fixture::new();
    let unrelated = "MAILTO=owner@example.com\n# Existing job\n12 3 * * * /usr/bin/true\n";
    fs::write(&fixture.cron, unrelated).unwrap();
    let output = fixture.run("install");
    let installed = fixture.cron();
    assert!(installed.starts_with(unrelated));
    assert_eq!(installed.matches(BEGIN).count(), 1);
    assert_eq!(installed.matches(END).count(), 1);
    assert!(
        installed
            .lines()
            .any(|line| line.starts_with("*/5 * * * * "))
    );
    assert!(!installed.contains(&fixture.token));
    assert!(!String::from_utf8_lossy(&output.stdout).contains(&fixture.token));
    assert!(!String::from_utf8_lossy(&output.stderr).contains(&fixture.token));

    fixture.run("install");
    assert_eq!(fixture.cron(), installed);
    let script = fixture.script();
    assert_private(script.parent().unwrap());
    assert_private(&script);
    assert_success(
        &Command::new("/bin/sh")
            .arg("-n")
            .arg(&script)
            .output()
            .unwrap(),
    );
    let script = fs::read_to_string(script).unwrap();
    assert!(script.contains("sync.log"));
    assert!(script.contains(" sync"));
    assert!(script.contains(env!("CARGO_BIN_EXE_memos")));

    // Evaluate only the environment assignments, never the real sync invocation.
    let mut setup = script
        .lines()
        .filter(|line| line.starts_with("export "))
        .collect::<Vec<_>>()
        .join("\n");
    setup.push_str("\nprintf '%s\\n' \"$MEMOS_URL\" \"$TOKEN\" \"$DATA_PATH\"\n");
    let environment = Command::new("/bin/sh")
        .arg("-c")
        .arg(setup)
        .output()
        .unwrap();
    assert_success(&environment);
    let values = String::from_utf8(environment.stdout).unwrap();
    let values: Vec<_> = values.lines().collect();
    assert_eq!(values[0], "https://example.invalid");
    assert_eq!(values[1], fixture.token);
    assert_eq!(Path::new(values[2]), fixture.work.join("notes space"));
    assert!(
        !fixture.marker.exists(),
        "configuration was evaluated as shell code"
    );
}

#[test]
fn custom_schedule_replaces_one_managed_block_and_invalid_schedules_do_nothing() {
    let fixture = Fixture::new();
    fixture.run("install");
    let output = fixture
        .command("install")
        .args(["--schedule", "7 */2 * * 1-5"])
        .output()
        .unwrap();
    assert_success(&output);
    let installed = fixture.cron();
    assert!(
        installed
            .lines()
            .any(|line| line.starts_with("7 */2 * * 1-5 "))
    );
    assert_eq!(installed.matches(BEGIN).count(), 1);
    for schedule in [
        "* * * *",
        "61 * * * *",
        "* * * * *\n@reboot evil",
        "@reboot",
    ] {
        let output = fixture
            .command("install")
            .args(["--schedule", schedule])
            .output()
            .unwrap();
        assert!(
            !output.status.success(),
            "accepted invalid schedule {schedule:?}"
        );
        assert_eq!(fixture.cron(), installed);
    }
}

#[test]
fn failed_crontab_reads_leave_existing_jobs_and_runner_untouched() {
    let fixture = Fixture::new();
    fs::write(&fixture.cron, "5 4 * * * /usr/bin/true\n").unwrap();
    let output = fixture
        .command("install")
        .env("MOCK_CRONTAB_READ_ERROR", "1")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert_eq!(fixture.cron(), "5 4 * * * /usr/bin/true\n");
    assert!(!fixture.script().exists());
}

#[test]
fn failed_crontab_updates_restore_the_previous_runner_and_schedule() {
    let fixture = Fixture::new();
    fixture.run("install");
    let old_cron = fixture.cron();
    let old_script = fs::read(fixture.script()).unwrap();
    fs::write(
        fixture.work.join(".env"),
        "MEMOS_URL=https://changed.example.invalid\nTOKEN=changed-token\nDATA_PATH=./different\n",
    )
    .unwrap();
    let output = fixture
        .command("install")
        .args(["--schedule", "*/10 * * * *"])
        .env("MOCK_CRONTAB_WRITE_ERROR", "1")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert_eq!(fixture.cron(), old_cron);
    assert_eq!(fs::read(fixture.script()).unwrap(), old_script);
    assert_private(&fixture.script());
}

#[test]
fn uninstall_needs_no_configuration_and_keeps_unrelated_jobs_notes_and_logs() {
    let fixture = Fixture::new();
    let unrelated = "# User job\n1 2 * * * /usr/bin/true\n";
    fs::write(&fixture.cron, unrelated).unwrap();
    fixture.run("install");
    let log = fixture.state.join("memos/sync.log");
    fs::write(&log, "previous sync output\n").unwrap();
    let note = fixture.work.join("notes space/my-note/note.md");
    fs::create_dir_all(note.parent().unwrap()).unwrap();
    fs::write(&note, "local note\n").unwrap();
    fs::write(fixture.work.join(".env"), "invalid dotenv \"\n").unwrap();

    fixture.run("uninstall");
    assert_eq!(fixture.cron(), unrelated);
    assert!(!fixture.script().exists());
    assert_eq!(fs::read_to_string(&log).unwrap(), "previous sync output\n");
    assert_eq!(fs::read_to_string(&note).unwrap(), "local note\n");
    fixture.run("uninstall");
    assert_eq!(fixture.cron(), unrelated);
}

#[test]
fn default_state_directory_uses_home_and_empty_crontabs_are_supported() {
    let fixture = Fixture::new();
    let output = fixture
        .command("install")
        .env_remove("XDG_STATE_HOME")
        .output()
        .unwrap();
    assert_success(&output);
    let script = fixture.root.join("home/.local/state/memos/sync.sh");
    assert!(script.exists());
    assert_private(&script);
    assert_eq!(fixture.cron().matches(BEGIN).count(), 1);
    assert_success(
        &fixture
            .command("uninstall")
            .env_remove("XDG_STATE_HOME")
            .output()
            .unwrap(),
    );
    assert!(!script.exists());
    assert!(fixture.cron().trim().is_empty());
}
