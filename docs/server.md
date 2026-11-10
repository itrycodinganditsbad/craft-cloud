# Running the server

Set up the server first; the agent ([`agent.md`](agent.md)) syncs folders with it.

```sh
cargo build --release -p craft-cloud-server
# Create a data directory (SQLite + local disk) and the first admin. The password is read from stdin.
printf '%s\n' 'choose a password' | ./target/release/craft-cloud-server init --dir ./cc-data --admin you
./target/release/craft-cloud-server serve --config ./cc-data/config.toml
```

The server speaks plain HTTP on `127.0.0.1:8080`. To reach it from other machines, put a TLS reverse proxy
(Caddy, nginx, Traefik …) in front of it:

1. Set `listen` to the proxy-facing address, and set `behind_proxy = true`.
2. Put the proxy's address in `trusted_proxies`. The server refuses to start behind a proxy without it,
   since every client would then share the proxy's address and a few wrong passwords would lock everyone
   out of signing in. If that is really what you want, set `ignore_forwarded_for = true` instead (and leave
   `trusted_proxies` empty: the server refuses both together).
3. Make the proxy set a request header timeout. The server cuts off request bodies that are too slow to
   arrive itself, and limits each account's uploads, so one slow device only slows its own account.

For a non-loopback `--listen`, `init` also needs `--behind-proxy` and the proxy's address, for example
`init --dir ./cc-data --admin you --listen 0.0.0.0:8080 --behind-proxy --trusted-proxy 10.0.0.2`.

For Postgres or S3, edit `[database]` and `[storage]` in `config.toml`.

The server compresses what it stores with zstd where that saves at least 5% (text, code and many project files
shrink a lot; photos, video and archives are stored as they are). Computers and apps never notice: every blob is
checked against its ID as before, and downloads are the original bytes. `compression = "none"` in `[storage]`
turns it off for new uploads; blobs already compressed stay compressed and are still read.

Three more settings in `[storage]` tune it:

```toml
compression = "zstd"          # or "none"
compression_level = 3         # zstd level of new writes, 1 to 19
compression_min_saving = 5    # keep a compressed blob only when it is this many percent smaller (1 to 50)
```

`compression_level` and `compression_min_saving` are checked when the server starts and refused with the line
number when they are out of range. Levels 20 to 22 are not offered: they use windows of 32 to 128 MiB, and the reader
caps a stored stream's window at 8 MiB so that a damaged object can't make the server allocate more. Chunks are at most
4 MiB and compressed one by one, so level 19's 8 MiB window already spans a whole chunk; long-distance matching or a
bigger window would find nothing more in one chunk. `compression = "none"` wins over any level: with it, no blob is
compressed whatever `compression_level` says.

**What a level costs.** Higher levels make smaller objects but compress more slowly; reading is as fast at every level
(decompression speed hardly depends on the level). Every upload of 64 KiB or more is compressed on a blocking thread of its own, so
uploads don't hold up the rest of the server. One core, 1 MiB
pieces, this repository's text, programs and a synthetic 3D mesh (`cargo test -p craft-cloud-store --release --test
level_bench -- --ignored --nocapture` reproduces the table on your machine):

| level | text and code | programs and libraries | 3D mesh buffers | bytes that don't compress |
|---|---|---|---|---|
| 1 | 71.7 % saved, 442 MiB/s | 66.0 %, 538 MiB/s | 65.2 %, 360 MiB/s | 0 %, 2261 MiB/s |
| 3 (default) | 74.7 %, 341 MiB/s | 69.3 %, 374 MiB/s | 68.9 %, 238 MiB/s | 0 %, 1404 MiB/s |
| 5 | 76.4 %, 141 MiB/s | 70.2 %, 175 MiB/s | 68.0 %, 120 MiB/s | 0 %, 803 MiB/s |
| 9 | 77.8 %, 62 MiB/s | 71.2 %, 83 MiB/s | 76.0 %, 78 MiB/s | 0 %, 380 MiB/s |
| 12 | 78.1 %, 45 MiB/s | 71.4 %, 42 MiB/s | 77.1 %, 41 MiB/s | 0 %, 417 MiB/s |
| 15 | 78.5 %, 19 MiB/s | 71.4 %, 19 MiB/s | 78.6 %, 14 MiB/s | 0 %, 138 MiB/s |
| 17 | 79.6 %, 10 MiB/s | 72.9 %, 10 MiB/s | 86.8 %, 8 MiB/s | 0 %, 24 MiB/s |
| 19 | 79.8 %, 6 MiB/s | 75.0 %, 5 MiB/s | 84.0 %, 3 MiB/s | 0 %, 19 MiB/s |

Each cell is the saving and the compression speed in MiB of plain data per second on one core of a desktop
machine that was also busy building. The "programs" are this project's own debug-info-rich libraries, which
compress better than release executables do; the "3D mesh" is synthetic. Decompression ran at 500 to 1,100 MiB/s at
every level. On about 440 GiB of real data (documents, downloads, media and project folders), level 3 saved 84 % on
text, 48 % on 3D and game assets, 33 % on programs, 23 % on PSD and PNG (PSD 49 % at level 9) and under 3 % on video,
audio and archives, which stay plain.

Level 3 (the default) compresses at 240 to 370 MiB/s per core, which is faster than most uploads arrive, and several
uploads compress on different cores at once. It gets most of what the higher levels gain: level 19 saves 1 to 5
percentage points more on text and programs for 50 to 70 times the CPU, and the incompressible bytes still cost
the full compression time before the server finds out that they don't shrink. Level 19 at 3 to 6 MiB/s per core
would make a big first sync wait on the CPU. So keep `compression_level` low for uploads and spend the slow levels
on data at rest instead, which is what the next section does. Raising it makes sense on a server that mostly receives
small, text-heavy changes and has cores to spare.

### Going further on data at rest

Three commands, run on the server's host with the same `--config`, look at and improve what is stored. They are safe
while the server runs and never delete anything.

- `craft-cloud-server store estimate [--sample 100] [--levels 1,3,9,19] --config …` takes a random sample of stored
  chunks, compresses each at every level without storing anything, and prints what each level would save (in percent
  of the data's size) and how fast it compresses. The result is kept with its date, so the admin overview can show
  "at level 19: NN % possible" without scanning anything when it is opened. It writes nothing but that one value.
- `craft-cloud-server store recompress --level 19 [--dry-run] [--limit-bytes N] --config …` rewrites stored objects
  at a higher level, only where that makes them at least 1% smaller. For each object it reads and checks the stored
  bytes against the ID, compresses them, decompresses the result and checks it against the ID again, and only then
  replaces the object (the same atomic replace uploads use). An object that cannot be read or does not check out is
  left exactly as it is and listed in the report; the command then exits with an error. `--dry-run` only reports.
  `--limit-bytes` stops after reading that many bytes of objects, to work in slices. It can be stopped at any time
  and started again: every blob it finished is recorded at its level and skipped next time, so a second run at the
  same level costs one database scan. It refuses to run when the config says `compression = "none"`.
- `craft-cloud-server store measure --config …` fills in the stored size of blobs written before sizes were tracked.
  It asks the store for each object's length (a `HEAD` on S3) and reads no content.

Recompressing while the server runs is safe because an object's ID is the hash of its plain bytes: a rewrite changes
how the same bytes are stored, never which bytes. On disk the new object is written next to the old one and renamed
over it, so a reader that has the old file open keeps reading the old bytes and the next reader gets the new ones. On S3
a `PUT` replaces the object atomically, and S3 is strongly consistent for reads after a write. If an upload of the same
chunk lands at the same moment, both writes hold the same plain bytes and the last one stays. Expect the
cost to be CPU: run it with `nice`, and in slices with `--limit-bytes` on a busy server.

The overview (`GET /api/v1/admin/storage`) shows the algorithm and level in use, the logical size of the blobs, the
physical size where it is known, how much of the data that covers, and the saving in percent. Blobs written before
this version have no recorded size until they are uploaded again, recompressed or measured, so the saving is
reported only over the blobs it is known for, and the share it covers is shown with it.

**Before going back to an older server version:** a server from before compression refuses compressed blobs, and a
blob once stored compressed stays that way (nothing converts it back, except that the same blob uploaded again
with `compression = "none"` is written plain). Going back is only possible if
`compression = "none"` was set before the first upload under the newer version. Otherwise, stay on a version that
reads compressed blobs. The same holds for recompressed objects. The database also gains columns and a table
(`blobs.physical_size`, `blobs.compress_level`, `server_state`); an older server refuses a database made by a newer
one, so going back means restoring the database from a backup taken before.

Quotas (`stored_bytes_per_user` below) count each blob at its original size, so turning compression on or off, or
recompressing, doesn't change what an account is charged.

To cap what one account may store, set `stored_bytes_per_user` (in bytes; there is no cap by default) in
`config.toml`. It counts every chunk and manifest an account uploaded once, and nothing frees space until
garbage collection arrives, including what a refused file stored before it was refused. A file that would go
over it is skipped by the agent, which says so; the rest of the folder keeps syncing. Nearly full counts as
full: with less than 4 MiB (one chunk) left, the agent pauses and says the account's storage is full.
A cap under 4 MiB is refused when the server starts, since not even one chunk would fit. Per-user quotas on
file sizes come later, with history.

Live documents have a cap of their own: `live_bytes_per_user` (in bytes; 1 GiB, `1_073_741_824`, by default) is
the most one account's live documents (recent-file lists and documents of files) may hold together. It counts
every update in their logs, including the ones a snapshot has replaced: those are kept until retention rules
arrive, so the total only goes down when the admin resets a document (`live reset-doc`). An update that would go
over it is refused; the agent keeps it on the machine and tries again later, and the app shows its changes as not
yet on the server. Snapshots of live documents are uploads like files' content, so they count under
`stored_bytes_per_user` instead. An account may also hold updates in at most 10,000 documents of files; the first
change to one more is refused the same way.

To add accounts: `printf '%s\n' 'their password' | craft-cloud-server user --config cc-data/config.toml add name`.
`--config` can come before or after `add`.

Or invite someone, with no password to hand over: `craft-cloud-server user --config cc-data/config.toml invite name`
prints a join string (`cloud.example.org/ABCD-EFGH-JKMN-PQRS`) and its link, good once for 7 days, to enter on their
first computer (`craft-cloud-agent join …`). It makes the account if there is none; they choose its password when
they join. For an account that exists, the invite just adds a computer, without its password. Set
`public_url = "https://cloud.example.org"` (the address computers reach the server at) at the top of `config.toml`
to have the whole join string printed; without it, `invite` prints the code and you put the address in front. A new
invite replaces the old one, and `user invite --revoke name` ends it. Every later computer joins with a code from one
that is signed in (`craft-cloud-agent add-computer`, see [`agent.md`](agent.md)). Codes that don't work are refused alike (wrong,
expired or used), and an address that tries ten in a minute is held off for the rest of it.

## Restoring a backup

With the server stopped, restore the database and the chunk storage from the backup, then run
`craft-cloud-server reset-epoch --config cc-data/config.toml --all`, and only then start `serve` again. Every
account then starts a new epoch, so each synced machine reconciles with the restored state instead of trusting its
own change counter. `reset-epoch --config … name` resets one account. (A reset run while the server is up still
works: open live connections end at their next ping, within 30 s, and connect again.)

## A full recents document

An app's recents log holds at most 16 MiB of updates per account. Once it is full, machines keep their new recents
locally and say so. `craft-cloud-server live --config cc-data/config.toml reset-doc name wordcraft` empties that log;
every machine then sends up what the emptied log lacks, so nothing any machine holds is lost, and file syncing isn't
affected. A document whose own history is over 16 MiB stays full until compacting the log is supported. An account
may have 64 live documents holding updates and 1,024 in all (one per app today); past that, new ones stay on the
machines. Live documents can't be deleted yet.
