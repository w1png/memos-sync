use std::fs::{self, File};
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail, ensure};

use crate::config::Config;

const BEGIN: &str = "# BEGIN memos-sync";
const END: &str = "# END memos-sync";

#[derive(clap::Subcommand)]
pub enum ServiceCommand {
    /// Install or replace the current user's cron sync job.
    Install {
        #[arg(long, default_value = "*/5 * * * *")]
        schedule: String,
    },
    /// Remove the current user's managed cron job; keep notes and logs.
    Uninstall,
}

pub fn execute(command: ServiceCommand) -> Result<i32> {
    let platform = platform()?;
    match command {
        ServiceCommand::Install { schedule } => {
            let schedule = validate_schedule(&schedule)?;
            match dotenvy::from_path(".env") {
                Ok(_) => {}
                Err(dotenvy::Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error).context("Cannot load .env"),
            }
            let config = Config::from_env()?;
            let cwd = std::env::current_dir()?;
            let data = if config.data_path.is_absolute() {
                config.data_path.clone()
            } else {
                cwd.join(&config.data_path)
            };
            let executable = std::env::current_exe().context("Cannot locate the memos binary")?;
            let state = state_directory()?;
            let script = state.join("sync.sh");
            let entry = format!("{schedule} {}", cron_path(&script)?);
            let previous_cron = read_crontab()?;
            let next_cron = replace_block(&previous_cron, Some(&entry))?;
            let uid = current_uid()?;
            prepare_directory(&state, uid)?;
            validate_wrapper(&script, uid)?;
            validate_wrapper(&state.join("sync.log"), uid)
                .context("Cannot safely append to the sync log")?;
            let previous_script = match fs::read(&script) {
                Ok(bytes) => Some(bytes),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => return Err(error).context("Cannot read the existing sync wrapper"),
            };
            let wrapper = wrapper(&config, &data, &executable, &state)?;
            atomic_write(&script, wrapper.as_bytes())?;
            if let Err(error) = write_crontab(&next_cron) {
                let rollback = match previous_script {
                    Some(bytes) => atomic_write(&script, &bytes),
                    None => fs::remove_file(&script).context("Cannot remove the new sync wrapper"),
                };
                if let Err(rollback_error) = rollback {
                    return Err(error).context(format!(
                        "Sync wrapper rollback also failed: {rollback_error}"
                    ));
                }
                return Err(error);
            }
            println!("Installed cron sync ({platform}); schedule: {schedule}");
            println!("Wrapper: {}", script.display());
            println!("Log: {}", state.join("sync.log").display());
        }
        ServiceCommand::Uninstall => {
            let state = state_directory()?;
            check_path(&state)?;
            let script = state.join("sync.sh");
            let previous_cron = match read_crontab() {
                Ok(contents) => contents,
                Err(error)
                    if error
                        .downcast_ref::<std::io::Error>()
                        .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound)
                        && fs::symlink_metadata(&script)
                            .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
                {
                    println!("No service installed ({platform}).");
                    return Ok(0);
                }
                Err(error) => return Err(error),
            };
            let next_cron = replace_block(&previous_cron, None)?;
            let uid = current_uid()?;
            if state.exists() {
                validate_directory(&state, uid)?;
                validate_wrapper(&script, uid)?;
            }
            if previous_cron != next_cron {
                write_crontab(&next_cron)?;
            }
            match fs::remove_file(&script) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error).context("Cannot remove the sync wrapper"),
            }
            println!("Removed cron sync ({platform}); notes and logs were kept.");
        }
    }
    Ok(0)
}

fn platform() -> Result<&'static str> {
    let linux = std::env::consts::OS == "linux";
    let systemd = linux && Path::new("/run/systemd/system").is_dir();
    let openrc = linux
        && !systemd
        && Command::new("rc-service")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success());
    classify_platform(std::env::consts::OS, systemd, openrc)
}

fn classify_platform(os: &str, systemd: bool, openrc: bool) -> Result<&'static str> {
    match os {
        "macos" => Ok("macOS / cron"),
        "linux" => {
            if systemd {
                Ok("Linux / systemd / cron")
            } else if openrc {
                Ok("Linux / OpenRC / cron")
            } else {
                Ok("Linux / cron")
            }
        }
        os => bail!("Service installation supports macOS and Linux; detected {os}"),
    }
}

fn validate_schedule(schedule: &str) -> Result<String> {
    ensure!(
        !schedule.contains(['\n', '\r']),
        "Cron schedule must be one line with five fields"
    );
    let fields: Vec<_> = schedule.split_whitespace().collect();
    ensure!(
        fields.len() == 5
            && fields.iter().all(|field| {
                field
                    .bytes()
                    .all(|b| b.is_ascii_digit() || b"*,-/".contains(&b))
            }),
        "Cron schedule must have five numeric fields using only digits, *, comma, dash, and slash"
    );
    for (field, (minimum, maximum)) in
        fields
            .iter()
            .zip([(0, 59), (0, 23), (1, 31), (1, 12), (0, 7)])
    {
        for item in field.split(',') {
            let (range, step) = item
                .split_once('/')
                .map_or((item, None), |(range, step)| (range, Some(step)));
            if let Some(step) = step {
                ensure!(
                    step.parse::<u32>().is_ok_and(|step| step > 0),
                    "Cron steps must be positive integers"
                );
            }
            if range == "*" {
                continue;
            }
            let (first, last) = range.split_once('-').unwrap_or((range, range));
            let first: u32 = first.parse().context("Invalid numeric cron field")?;
            let last: u32 = last.parse().context("Invalid numeric cron field")?;
            ensure!(
                first >= minimum && last <= maximum && first <= last,
                "Cron field is outside its allowed range ({minimum}-{maximum})"
            );
        }
    }
    Ok(fields.join(" "))
}

fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn path_text(path: &Path) -> Result<&str> {
    path.to_str().context("Service paths must be valid UTF-8")
}

fn cron_path(path: &Path) -> Result<String> {
    let text = path_text(path)?;
    ensure!(
        !text.contains(['\n', '\r']),
        "Cron wrapper path must be one line"
    );
    Ok(quote(text).replace('%', "\\%"))
}

fn wrapper(config: &Config, data: &Path, executable: &Path, state: &Path) -> Result<String> {
    Ok(format!(
        "#!/bin/sh\n# Managed by memos service install. Contains credentials; keep private.\numask 077\nexport MEMOS_URL={}\nexport TOKEN={}\nexport DATA_PATH={}\ncd {} || exit 1\nexec {} sync >> {} 2>&1\n",
        quote(&config.url),
        quote(&config.token),
        quote(path_text(data)?),
        quote(path_text(state)?),
        quote(path_text(executable)?),
        quote(path_text(&state.join("sync.log"))?),
    ))
}

fn state_directory() -> Result<PathBuf> {
    let base = match std::env::var_os("XDG_STATE_HOME") {
        Some(path) if !path.is_empty() => PathBuf::from(path),
        _ => PathBuf::from(std::env::var_os("HOME").context("HOME is required")?)
            .join(".local/state"),
    };
    ensure!(
        base.is_absolute(),
        "State directory must be an absolute path"
    );
    let path = resolve_existing_ancestor(&base)?.join("memos");
    check_path(&path)?;
    Ok(path)
}

fn resolve_existing_ancestor(path: &Path) -> Result<PathBuf> {
    ensure!(
        !path
            .components()
            .any(|part| matches!(part, Component::ParentDir)),
        "Service state path must not contain .."
    );
    let mut ancestor = path;
    let mut missing = Vec::new();
    while !ancestor.try_exists()? {
        missing.push(
            ancestor
                .file_name()
                .context("Invalid service state path")?
                .to_owned(),
        );
        ancestor = ancestor
            .parent()
            .context("Cannot resolve service state path")?;
    }
    let mut resolved = ancestor
        .canonicalize()
        .context("Cannot resolve service state path")?;
    for component in missing.into_iter().rev() {
        resolved.push(component);
    }
    Ok(resolved)
}

fn check_path(path: &Path) -> Result<()> {
    let mut current = PathBuf::new();
    for part in path.components() {
        ensure!(
            !matches!(part, Component::ParentDir),
            "Service paths must not contain .."
        );
        current.push(part);
        match fs::symlink_metadata(&current) {
            Ok(metadata) => ensure!(
                !metadata.is_symlink(),
                "Service path must not contain symlinks: {}",
                current.display()
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("Cannot inspect service path"),
        }
    }
    Ok(())
}

fn current_uid() -> Result<u32> {
    let output = Command::new("id")
        .arg("-u")
        .output()
        .context("Cannot determine current user")?;
    ensure!(output.status.success(), "Cannot determine current user");
    std::str::from_utf8(&output.stdout)?
        .trim()
        .parse()
        .context("Invalid current user ID")
}

#[cfg(unix)]
fn validate_directory(path: &Path, uid: u32) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_dir() && !metadata.is_symlink(),
        "Service state must be a regular directory"
    );
    ensure!(
        metadata.uid() == uid,
        "Service state directory belongs to another user"
    );
    Ok(())
}

#[cfg(not(unix))]
fn validate_directory(_: &Path, _: u32) -> Result<()> {
    bail!("Service installation requires a Unix platform")
}

fn prepare_directory(path: &Path, uid: u32) -> Result<()> {
    check_path(path)?;
    fs::create_dir_all(path)?;
    validate_directory(path, uid)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

#[cfg(unix)]
fn validate_wrapper(path: &Path, uid: u32) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            ensure!(
                metadata.is_file() && !metadata.is_symlink(),
                "Sync wrapper must be a regular file"
            );
            ensure!(
                metadata.uid() == uid,
                "Sync wrapper belongs to another user"
            );
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("Cannot inspect sync wrapper"),
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_wrapper(_: &Path, _: u32) -> Result<()> {
    bail!("Service installation requires a Unix platform")
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("Missing service directory")?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        temp.as_file()
            .set_permissions(fs::Permissions::from_mode(0o700))?;
    }
    temp.write_all(bytes)?;
    temp.as_file().sync_all()?;
    temp.persist(path)
        .map_err(|error| error.error)
        .context("Cannot save sync wrapper")?;
    File::open(parent)?.sync_all()?;
    Ok(())
}

fn read_crontab() -> Result<String> {
    let output = Command::new("crontab")
        .arg("-l")
        .env("LC_ALL", "C")
        .output()
        .context("Cannot run crontab; install a cron implementation first")?;
    if output.status.success() {
        return String::from_utf8(output.stdout).context("Existing crontab is not UTF-8");
    }
    let error = String::from_utf8_lossy(&output.stderr);
    let error = error.trim();
    let message = error.strip_prefix("crontab: ").unwrap_or(error);
    if output.status.code() == Some(1)
        && !message.contains('\n')
        && message.starts_with("no crontab for ")
    {
        return Ok(String::new());
    }
    bail!(
        "Cannot read the current user's crontab ({}); it was left unchanged",
        output.status
    )
}

fn write_crontab(contents: &str) -> Result<()> {
    let mut child = Command::new("crontab")
        .arg("-")
        .env("LC_ALL", "C")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("Cannot run crontab")?;
    let write = child
        .stdin
        .take()
        .context("Cannot open crontab input")?
        .write_all(contents.as_bytes());
    let output = child
        .wait_with_output()
        .context("Cannot wait for crontab")?;
    ensure!(
        output.status.success(),
        "Cannot update the current user's crontab ({}); sync wrapper will be restored",
        output.status
    );
    write.context("Cannot write the crontab input")
}

fn replace_block(contents: &str, entry: Option<&str>) -> Result<String> {
    let mut output = String::new();
    let mut inside = false;
    for line in contents.split_inclusive('\n') {
        let marker = line.trim_end_matches(['\n', '\r']);
        if marker == BEGIN {
            ensure!(
                !inside,
                "Nested memos cron markers; refusing to change crontab"
            );
            inside = true;
        } else if marker == END {
            ensure!(
                inside,
                "Unmatched memos cron marker; refusing to change crontab"
            );
            inside = false;
        } else if !inside {
            output.push_str(line);
        }
    }
    ensure!(
        !inside,
        "Unclosed memos cron marker; refusing to change crontab"
    );
    if let Some(entry) = entry {
        if !output.is_empty() && !output.ends_with('\n') {
            output.push('\n');
        }
        output.push_str(&format!("{BEGIN}\n{entry}\n{END}\n"));
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cron_block_replacement_is_idempotent_and_preserves_other_jobs() {
        let original = "SHELL=/bin/sh\n15 * * * * /bin/other\n# a comment\n";
        let once = replace_block(original, Some("*/5 * * * * '/home/me/sync.sh'")).unwrap();
        assert_eq!(
            replace_block(&once, Some("*/5 * * * * '/home/me/sync.sh'")).unwrap(),
            once
        );
        assert_eq!(replace_block(&once, None).unwrap(), original);
        let changed = replace_block(&once, Some("0 * * * * '/new/path'")).unwrap();
        assert!(changed.starts_with(original));
        assert_eq!(changed.matches(BEGIN).count(), 1);
        assert!(!changed.contains("/home/me/sync.sh"));
    }

    #[test]
    fn duplicate_owned_blocks_are_removed_but_malformed_markers_fail_closed() {
        let double = format!("{BEGIN}\na\n{END}\nother\n{BEGIN}\nb\n{END}\n");
        assert_eq!(replace_block(&double, None).unwrap(), "other\n");
        for malformed in [
            format!("{BEGIN}\na\n"),
            format!("{END}\n"),
            format!("{BEGIN}\n{BEGIN}\n{END}\n"),
        ] {
            assert!(replace_block(&malformed, None).is_err());
        }
    }

    #[test]
    fn schedules_allow_standard_numeric_expressions_and_reject_injection() {
        assert_eq!(validate_schedule("*/5  *\t* * *").unwrap(), "*/5 * * * *");
        assert!(validate_schedule("0,30 8-18/2 * * 1-5").is_ok());
        for invalid in [
            "@reboot",
            "* * * *",
            "* * * * * extra",
            "* * * * *\n/bin/evil",
            "* * * * $(id)",
            "0 0 * * MON",
            "* * * * %",
            "61 * * * *",
            "* 24 * * *",
            "* * 0 * *",
            "* * * 13 *",
            "* * * * 8",
            "*/0 * * * *",
            "5-3 * * * *",
            "",
        ] {
            assert!(validate_schedule(invalid).is_err(), "accepted {invalid}");
        }
    }

    #[test]
    fn classifies_macos_and_linux_init_variants() {
        assert_eq!(
            classify_platform("macos", false, false).unwrap(),
            "macOS / cron"
        );
        assert_eq!(
            classify_platform("linux", true, false).unwrap(),
            "Linux / systemd / cron"
        );
        assert_eq!(
            classify_platform("linux", false, true).unwrap(),
            "Linux / OpenRC / cron"
        );
        assert_eq!(
            classify_platform("linux", false, false).unwrap(),
            "Linux / cron"
        );
        assert!(classify_platform("windows", false, false).is_err());
    }

    #[test]
    fn wrapper_quotes_credentials_and_paths_as_literal_shell_strings() {
        let config = Config {
            url: "https://example.com".into(),
            token: "abc'$(touch /tmp/never)$HOME".into(),
            data_path: "unused".into(),
        };
        let script = wrapper(
            &config,
            Path::new("/home/me/my notes"),
            Path::new("/home/me/bin/memos"),
            Path::new("/home/me/state"),
        )
        .unwrap();
        assert!(script.contains("export TOKEN='abc'\\''$(touch /tmp/never)$HOME'"));
        assert!(script.contains("export DATA_PATH='/home/me/my notes'"));
        assert!(script.contains("cd '/home/me/state' || exit 1"));
        assert!(
            script.ends_with("exec '/home/me/bin/memos' sync >> '/home/me/state/sync.log' 2>&1\n")
        );
        assert_eq!(
            cron_path(Path::new("/home/me/100% ready/sync.sh")).unwrap(),
            "'/home/me/100\\% ready/sync.sh'"
        );
    }

    #[cfg(unix)]
    #[test]
    fn service_paths_refuse_symlinks() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let canonical = dir.path().canonicalize().unwrap();
        fs::create_dir(canonical.join("real")).unwrap();
        symlink(canonical.join("real"), canonical.join("linked")).unwrap();
        assert!(check_path(&canonical.join("linked/memos")).is_err());
        assert!(check_path(&canonical.join("real/memos")).is_ok());
    }
}
