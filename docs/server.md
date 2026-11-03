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

## Docker

`packaging/docker/` holds a hardened container image of the server and a Docker Compose deployment: the server
behind Caddy, which terminates TLS. By default the database (SQLite) and the blobs live on a Docker volume; Postgres
and S3 are overlays. It needs Docker Compose 2.32 or newer on Linux and builds the image locally: there is no image
to pull and nothing is published to a registry. The first build wants about 4 GiB of memory for the Docker engine (it
uses two compiler jobs).

### Quick start

```sh
cd packaging/docker
./install.sh --domain cloud.example.org --admin you
```

`install.sh` checks Docker and the free disk space, writes `.env` (mode 0600, never committed) with random database
passwords, builds the image, starts the stack and creates the first admin. The admin's password is asked for without
echo, or read from a pipe, and reaches the server through standard input only. On a terminal it asks for the
settings you did not give; `./install.sh --help` lists the options (`--tls acme|internal|custom`, `--email`,
`--db sqlite|postgres`, `--http-port`, `--https-port`, `--no-admin`, `--no-start`). It never overwrites an existing
`.env`. Afterwards `docker compose ps` and `docker compose logs -f server` show the state, and the domain's
`/api/info` answers through Caddy.

To set it up by hand, copy `.env.example` to `.env`, fill it in, and run `docker compose build` and
`docker compose up -d --wait`.

### What runs

| Service | Image | Notes |
|---|---|---|
| `server` | built from `packaging/docker/Dockerfile` | distroless `cc`, uid 65532, plain HTTP on port 8080 inside the stack |
| `caddy` | `caddy:2.11-alpine` | the only service with published ports (80 and 443); TLS and the redirect from HTTP |
| `secrets-init` | `caddy:2.11-alpine` | one-shot, no network: renders `config.toml` and writes the secret files |
| `db`, `db-init` | `postgres:17-alpine` | only with `compose.postgres.yaml` |

Every base image is pinned by the digest of its multi-arch index, with the tag in a comment next to it. To update
one, change the tag and the digest together (`docker buildx imagetools inspect <image>:<tag>` prints the index
digest).

Every service has a read-only root filesystem, no capabilities (`cap_drop: [ALL]`), `no-new-privileges`, memory and
process limits, and a `noexec` tmpfs for `/tmp`. The exceptions are explained next to each service: Postgres and
`secrets-init` start as root and need a few capabilities to fix ownership, Caddy needs to bind ports 80 and 443, and
`secrets-init` (in the Postgres and S3 overlays) is not read-only because Compose writes its secret files into the
root filesystem.

Three networks keep the parts apart: `edge` (published ports and outbound access, Caddy only), `frontend` (internal;
Caddy to the server) and `backend` (internal; the server to Postgres, Caddy is not attached).

### Configuration and secrets

The server reads only its TOML file. `config.toml.in` is the template and `render-config.sh` fills it. The
`secrets-init` service renders it into the `secrets` volume (mode 0400, owner 65532) and the server mounts it
read-only at `/etc/craft-cloud/config.toml`. The file holds the database URL (with the password, for Postgres) and,
for S3, the keys. The server's `Debug` output hides the URL's credentials, and `docker inspect` of the running
services shows no secret, since none is in an environment or a Compose file. The values are checked against short
lists of allowed characters before they are written, so a value cannot break the TOML or add a setting.

To change a setting in `.env` (`CC_*`, see `.env.example`), edit the file, run `docker compose run --rm secrets-init`
and then `docker compose up -d`.

The server believes `X-Forwarded-For` only from Caddy. `trusted_proxies` takes single addresses, no networks, so
Caddy has a fixed address (`CC_CADDY_IP`, `172.29.215.2` in the network `CC_FRONTEND_SUBNET`, `172.29.215.0/24`). If
these defaults clash with a network on your host, change both together.

### TLS modes

Set `CC_TLS_MODE` in `.env`:

- `acme` (default): a public certificate from Let's Encrypt. The domain must resolve to the host, and the ports must
  be reachable from the internet (with other ports, set `CC_HTTP_PORT`, `CC_HTTPS_PORT` and a matching
  `CC_PUBLIC_URL`).
- `internal`: a certificate from Caddy's own CA, for tests. Clients do not trust it. There is no HSTS in this mode.
- `custom`: your own certificate. Put `fullchain.pem` and `private.key` in `packaging/docker/tls/` (or `CC_TLS_DIR`),
  readable by root only, and restart Caddy to load a renewed one.

Caddy sets a request header timeout of 10 seconds, as the checklist above asks. It writes no access log, because URLs hold invite
codes (`/join/<code>`).

### Postgres

```sh
./install.sh --domain cloud.example.org --db postgres
```

or add `COMPOSE_FILE=compose.yaml:compose.postgres.yaml` to `.env`. This adds a Postgres container and a one-shot
`db-init` that creates the role `craft_cloud`. The server migrates its database when it opens it, so that role owns
the database `craft_cloud` and its schema and may change them. That is less separation than an owner role plus a
runtime role. The role is not a superuser and has no other privileges; `PUBLIC` may not connect to the database or
create in its schema, and the role cannot connect to `postgres` or `template1`. The superuser password exists only
for the Postgres container and `db-init`. Blobs stay on the data volume.

To rotate the database password, change `CC_SECRET_DB_PASSWORD` in `.env` (characters from `A-Za-z0-9._~-`, at least
32), then run `docker compose run --rm secrets-init`, `docker compose up -d db-init` and
`docker compose restart server`. The superuser password cannot be changed that way: Postgres applies it only when
the data directory is created. Change it with `ALTER ROLE postgres` first and then in `.env`.

The connection between the server and Postgres is not encrypted (the server's `sqlx` has no TLS feature). It never
leaves the internal `backend` network. An external Postgres over TLS is not covered.

### S3

`compose.s3.yaml` replaces the disk storage with an S3 bucket: fill in the `CC_S3_*` and `CC_SECRET_S3_*` lines of
`.env.example` in `.env` and add `compose.s3.yaml` to `COMPOSE_FILE`. The keys end up in `config.toml` like the
database password, not in the server's environment. The server joins the `edge` network to reach the endpoint. The
image holds the CA certificates the server needs for HTTPS endpoints. This overlay has only been written, not yet
run against a real S3 service.

### The first admin and more accounts

`install.sh` creates the first admin. By hand, with the password on standard input:

```sh
printf '%s\n' 'a password' | docker compose run --rm --no-deps -T server user --config /etc/craft-cloud/config.toml add --admin you
```

More accounts: the same line without `--admin`, or `docker compose run --rm --no-deps server user --config
/etc/craft-cloud/config.toml invite name`, which prints the whole join string because `public_url` is set. The
other commands of this guide (`device`, `reset-epoch`, `live`) run the same way.

### Data, upgrading and backups

The data is in the volume `data` (`/var/lib/craft-cloud` in the container): `meta.db` (with SQLite) and `blobs/`.
The image creates the directory for uid 65532 with mode 0700, so a new named volume gets that owner. A bind mount
instead of the volume must be prepared first: `chown 65532:65532 DIR && chmod 0700 DIR`.

To upgrade, update the source, then run `docker compose build server` and `docker compose up -d --wait`. The server
migrates the database when it opens it and refuses a database newer than itself, so back up before an upgrade and do
not go back to an older image without the backup.

There is no backup automation yet. Stop the server (`docker compose stop server`), copy the volume (SQLite:
`meta.db`, `meta.db-wal`, `meta.db-shm` and `blobs/` together; Postgres: `pg_dump` from the `db` container plus
`blobs/`), and start it again. Restoring is described above, including `reset-epoch`.

`docker compose down` keeps the volumes. `docker compose down -v` deletes all data.

### Troubleshooting

- `secrets-init` fails with a message such as `PUBLIC_URL has a value that is empty or has characters that are not
  allowed`: a setting in `.env` is wrong. The message names the setting and never shows the value.
- The server exits at once with a permission error on `/var/lib/craft-cloud`: the data directory is not owned by
  65532 (see bind mounts above).
- `server` is not healthy: `docker compose logs server`. The check is `craft-cloud-server healthcheck`, a
  `GET /api/info` request to `127.0.0.1:8080` inside the container.
- All devices share one sign-in budget: `trusted_proxies` does not contain Caddy's address. `CC_CADDY_IP` must lie in
  `CC_FRONTEND_SUBNET`; run `docker compose run --rm secrets-init` after changing either.

### Not covered

Postgres over TLS, TLS in the server itself (Caddy terminates it), Kubernetes or Helm files, a registry image and
release automation, and backup automation (copy the data as above until the server can create backups itself).
