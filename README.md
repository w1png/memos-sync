# memos

A small Rust CLI that syncs Memos notes with local Markdown files. Edit in Vim,
make quick changes on your phone, and sync both ways. Supports Memos 0.31.x.

## Build and configure

Install Rust and Make, then build:

```sh
make build
cp .env.example .env
```

Edit `.env` with your instance URL and personal access token:

```dotenv
MEMOS_URL="https://notes.example.com"
TOKEN="your-personal-access-token"
DATA_PATH="./data"
```

The app reads `.env` from the working directory. Sync flags override environment
variables, which override `.env` values. `DATA_PATH` defaults to `./data`.
Keep your token private.

## Use

Run a sync from the checkout, or install the binary:

```sh
make run
make install
export PATH="$HOME/.local/bin:$PATH" # If this directory is not already on PATH.
memos help
memos sync
memos sync --url=https://notes.example.com --token=your-token --data-path=./data
```

`make` defaults to `make build`. Installation defaults to `~/.local/bin`; use
`make install PREFIX=/your/prefix` to choose another location and add its `bin`
directory to your PATH.

Each memo gets a folder containing `note.md`. Edit its Markdown body and keep
the `memos_id` header intact. Images and attachments stay hosted in Memos.
To create a new private memo locally, write a note without a metadata header:

```sh
mkdir -p data/my-note
vim data/my-note/note.md
memos sync
```

## Resolve conflicts

If both copies change, the app leaves `note.md` untouched and adds `remote.md`
and `conflict.diff` beside it. Compare the versions, edit `note.md`, delete
`conflict.diff`, and sync again. If Memos changed again, a new diff appears.

Deleting a local file downloads it again. Delete notes in Memos to remove them;
their local folders move into `.memos/trash/`. Keep the `.memos` folder with your
notes because it contains sync history and recovery data.

## Automatic syncing

On macOS or Linux, install a job in your user's crontab:

```sh
make install-service
make install-service CRON_SCHEDULE='*/10 * * * *'
```

The default schedule is every five minutes; the second command changes it to
every ten minutes. On Linux, install and enable your distribution's cron daemon
first; this works on systems using systemd or OpenRC.

Service installation reads URL, token, and data path only from the current `.env`
file. It saves a private launcher under `~/.local/state/memos/` (or
`$XDG_STATE_HOME/memos/`) with explicit sync flags and an absolute notes path.
Crontab calls the launcher; credentials stay in the private script. Run
`make install-service` again after changing `.env` to update the saved job. Use
the same `PREFIX` for later management if you chose a custom install location.

```sh
make uninstall-service # Stop automatic syncing; keep the binary.
make uninstall         # Remove the cron job first, then the binary.
```

Both commands leave your notes and sync history intact.
