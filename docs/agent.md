# The agent on the command line

With a server running, sign in on each machine and sync:

```sh
cargo build --release -p craft-cloud-agent
# The password is read from stdin. The folder is created if it doesn't exist.
printf '%s\n' 'your password' | ./target/release/craft-cloud-agent login https://cloud.example.org --user you --folder ~/'Craft Cloud'
./target/release/craft-cloud-agent run       # keep it in step until Ctrl-C
./target/release/craft-cloud-agent status    # account, folder, whether `run` runs and what it does, last pass
./target/release/craft-cloud-agent sync      # or: one pass by hand
```

- `run` syncs whenever something changes in the folder (it watches it) or on the server (it waits on the
  server's change feed), and at least every 10 minutes. A file that is still being written (its size or
  time changed within the last 2 seconds, or on Windows a program still has it open for writing) waits
  until it settles; a file that keeps changing (a log, a database held open) is looked at less and less
  often, at least every 5 minutes. When the server can't be reached it tries again with growing pauses, up to 5 minutes.
  If something needs you (the folder or its marker is missing, the device was signed out) it pauses and
  `status` says why; a missing folder or marker is looked at again every minute, and syncing goes on by
  itself once it is back. Ctrl-C lets a running pass finish and stops; a second Ctrl-C stops at once.
- While `run` runs, `sync --approve-deletes` from another terminal hands the approval to it: exactly the
  deletions `status` listed. If more came since, it lists them instead, approves nothing and exits 2; check
  the list and run it again. Every list of waiting deletions comes with its set (`set 3f9a…`), also in
  `run`'s output: `sync --approve-deletes=<set>` approves exactly that set if it is the one waiting, without
  `status` first, and otherwise shows the waiting set, approves nothing and exits 2. A plain `sync` says the
  agent is already running.
- A delete held back because a link appeared that may be the file, moved with its folder ("held while … is a
  link", or "held while any of … is a link" when several links appeared with that name), comes with a set of
  its own. If you really deleted the file and none of the links listed is it, `sync --approve-deletes=<set>`
  with that set lets the delete go; nothing else (the bare flag included) releases it.
- `craft-cloud-agent autostart on` makes the agent start by itself at every login (`join --autostart` does it
  when joining; `autostart off` and signing out undo it): on Windows a `Craft Cloud` value under
  `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` that runs `craft-cloud-agent-bg.exe run --log`, the build
  without a console window; on macOS `~/Library/LaunchAgents/ch.craftcloud.agent.plist`; on Linux the systemd user
  unit `craft-cloud-agent.service` (a unit of that name made by hand is taken over), or
  `~/.config/autostart/craft-cloud-agent.desktop` without systemd. Each runs `run --log`, which writes what the agent
  says to `run.log` in its data folder (rotated at 10 MiB). After a crash it is started again (after 30 s), but not
  after a clean quit, and on Linux not after exit code 3. `status` through the agent's local API says whether it is on. The entry belongs to the agent with the
  default data folder: one run with `--data-dir` or `CRAFT_CLOUD_AGENT_DIR` elsewhere (a spare folder) never sets,
  replaces or removes it.
- To sync a folder that already has files, add `--adopt` to `login`.
- `sync` exits 0 when done; 1 when it failed and can simply run again (offline, server busy); 2 when
  deletions wait for approval (`sync --approve-deletes` releases exactly the ones it listed); 3 when it
  needs you (the folder or its `.craft-cloud` marker is missing, another agent is running, the device was
  signed out, and so on). The message says what to do.
- HTTPS is required, except to a server on the same machine. Proxy settings are ignored for now.
- `logout` signs the device out on the server and forgets the account here; your files stay. Use
  `logout --local` if the server is gone.
- `run` also starts without an account: it waits for a sign-in from the Craft Cloud app (or `join`, `login`),
  then syncs. A second `run` for the same data folder says another agent runs and exits 0. A damaged account file
  found as `run` starts keeps it waiting (its status says what is wrong) until it is put right or a sign-out
  forgets it.
  While `run` runs, `login` and `logout` go through it (it holds the data folder), and a sign-out stops
  syncing and keeps the files.
- Instead of a password, a machine can join with a code. On a machine that is signed in and running,
  `craft-cloud-agent add-computer` prints a join string (`cloud.example.org/K7M4-Q9TX`, good once for 10 minutes)
  and waits; on the new machine, `craft-cloud-agent join cloud.example.org/K7M4-Q9TX` (with `--folder`, `--name`
  and `--adopt` as for `login`) signs it in. An admin's invite (`user invite`, in [`server.md`](server.md)) works the same way; for a new
  account, `join` asks for its password twice on standard input. A code that doesn't work says so, without saying
  why: make a new one.
- Through the running agent: `pause` and `resume` syncing (a pause holds across restarts; recents keep working),
  `quit` the agent, and `devices` to list the account's machines (`devices remove <id>` takes one out; to take this
  machine out, use `logout`).
- Recent files, per app, shared by every machine of the account (through the running agent, as an app
  sees them):

  ```sh
  ./target/release/craft-cloud-agent recents wordcraft                          # the list, newest first
  ./target/release/craft-cloud-agent recents wordcraft add Letters/doc1.docx    # a path in the synced folder
  ./target/release/craft-cloud-agent recents wordcraft --watch                  # again after every change
  ```

  An entry added offline is kept and reaches the other machines once the server is back. Each list keeps
  the 100 files opened last.
- A file of the synced folder as a live plain-text document (through the running agent, as an app opens one; a
  test tool for live documents):

  ```sh
  ./target/release/craft-cloud-agent doc Notes/todo.txt                    # its text
  ./target/release/craft-cloud-agent doc Notes/todo.txt --append "eggs"    # add to it, on every machine
  ./target/release/craft-cloud-agent doc Notes/todo.txt --watch            # again after every change
  ```

  The first machine to open a file makes it a live document from the file's content (UTF-8 text only); the others
  join it. Changes go to every machine that has it open as they are made, and edits made offline merge when the
  machine is back. `doc` never writes the file: what it shows is the live document.
