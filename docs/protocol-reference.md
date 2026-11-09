# Local protocol reference

This is a reference for tools written in languages other than Rust. The wire types live in `crates/api/src/local.rs`. An operational overview and further endpoints are in [the agent guide](agent.md), and the protocol design is in [design/live.md](design/live.md). Behavior that this page does not specify is listed under "Open questions" at the end.

## Discovery and authentication

The running agent writes `local-api.json` in its per-user data directory (`craft-cloud-agent`). The environment variable `CRAFT_CLOUD_AGENT_DIR` overrides the directory.

The file has this schema:

```json
{ "schema": 1, "port": 12345, "token": "…", "pid": 1234 }
```

To call the agent:

- Use the base URL `http://127.0.0.1:<port>`.
- Send the header `Authorization: Bearer <token>`.

Notes:

- The file is readable by its owner only, and it is removed on a clean shutdown. If you need to detect stale data, validate `pid`.
- The agent binds to the loopback interface and requires the exact header `Host: 127.0.0.1:<port>`.
- The agent refuses any request that has an `Origin` header.
- The agent never accepts the token in a URL. Never log the token.
- The supported API version range is currently `1..=1`. `GET /v1/status` reports the version as `api`.

## HTTP calls

All JSON calls use the exact serde field names in `crates/api/src/local.rs`. Errors have the form `{ "error": "code", "message": "…" }` (`ErrorAnswer`).

The table lists the status codes and error codes of the API. The source has other calls that are left out here unless app integration commonly needs them.

| Method and path | Body | Success | Common errors |
| --- | --- | --- | --- |
| `GET /v1/status` | none | `Status` | `unauthorized` |
| `POST /v1/login` | `{"server":"https://host","user":"ana","password":"…","folder":null,"device_name":null,"adopt":false}` | `Status` | `wrong_password`, `already_signed_in`, `offline` |
| `POST /v1/join` | `{"join":"https://host/join/CODE","folder":null,"device_name":null,"adopt":false,"password":null}` | `Status` | `invalid_code`, `password_required`, `weak_password`, `folder_has_files` |
| `POST /v1/logout` | `{"local":false}` | `Status` | `not_signed_in`, `offline` |
| `POST /v1/pause`, `/v1/resume`, `/v1/quit` | empty | `Status` | `unauthorized` |
| `GET /v1/recents` | WebSocket upgrade | Recents events | `not_signed_in` |
| `GET /v1/docs` | WebSocket upgrade | Docs events and binary frames | `not_signed_in`, `not_found` |
| `PUT /v1/blobs[?for=<path>]` | raw `application/octet-stream` bytes | `{ "hash": "<64 lowercase hex>", "len": 123 }` | `too_large`, `offline`, `not_signed_in`, `read_only` |
| `GET /v1/blobs/<hash>[?for=<path>]` | none | raw bytes | `blob_not_found`, `offline`, `not_signed_in` |
| `GET /v1/devices` | none | device list | `not_signed_in` |
| `DELETE /v1/devices/<id>` | none | updated device list | `cannot_remove_self`, `not_found` |
| `POST /v1/pairings` | empty | `PairingInfo` | `not_signed_in` |
| `GET /v1/pairings/<id>` | none | `PairingStatus` | `not_found` |
| `DELETE /v1/pairings/<id>` | none | 204 | `not_found` |
| `GET /v1/usage` | none | `AccountUsage` | `not_signed_in`, `offline` |
| `POST /v1/autostart` | `{"on":true}` | `Status` | `autostart_failed` |
| `GET /v1/spaces` | none | `SpaceList` | `not_signed_in`, `agent_too_old` in the client mapping |
| `POST /v1/spaces/<id>/sync` | `{"on":true}` plus optional `folder`, `adopt` | `SpaceInfo` | `invalid`, `conflict` |

`Status` contains these fields: `api`, `signed_in`, an optional `account`, `server`, `folder`, `machine`, `online`, `run_state`, an optional `autostart`, `last_sync_ms`, `removed`, and `spaces`. Optional values can be absent or null. A client should ignore fields it does not know, because fields may be added.

## Docs WebSocket

Upgrade `GET /v1/docs` and authenticate with the bearer header.

Text frames carry JSON objects with a `type` tag. Binary frames have this layout:

```text
[kind: u8][id: u64, big-endian][payload]
```

The header is 9 bytes. `id` is the open ID. The app chooses it, and it must be unique on the connection.

An open request looks like this:

```json
{"type":"open","id":1,"path":"Notes/story.craft","app":"wordcraft","identity":true}
```

The optional field `file_hash` is the BLAKE3 hash of the file's content, in hex.

The agent answers in this order:

1. `opened`.
2. A binary `state` frame of kind 1, when state exists.
3. `members`, if the request had `identity: true`.

The agent may also report save frontiers in a `file` event.

For a file that was never live, the agent answers `needs_import`. The app then sends a binary frame of kind 4 (`IMPORT`) and receives `opened`. If the ordinary file flow is needed, the agent sends `not_live`, which carries the reason.

Example answers:

```json
{"type":"opened","id":1,"doc":"<doc-id>","state":{"online":true,"waiting":0,"bytes":42,"updates":0},"read_only":false,"space":"<space-id>"}
{"type":"members","id":1,"members":[{"user":"<uuid>","name":"Ana","colour":3,"self":true}]}
```

### Roles and read-only

`opened.read_only` reflects the account's role at the time of the open. The event `role {doc, read_only}` follows later role changes.

If the account may not write, the agent answers a write with `error {id, seq, code:"read_only", message}`. Do not retry until the role becomes writable.

### Presence

Presence is relayed to the other participants and is not persisted. The maximum size is 4 KiB.

- Without an `identity` request, the agent sends only frames of kind 3.
- With `identity: true`, outgoing presence from the app is still a kind 3 frame: `[3][id][app bytes]`. Incoming presence from other accounts is a kind 5 frame, whose payload starts with the 16 bytes of the sender's account ID, followed by the app bytes. It has no peer field.
- The Rust kit puts the sender's Loro peer ID (`u64`, big-endian) at the start of its app bytes. The agent does not interpret the app bytes, so another client may use any content.

### Binary frame kinds

| Kind | Name | Meaning |
| --- | --- | --- |
| 1 | state | The document's state. |
| 2 | update | A document update. |
| 3 | presence | Presence without identity. |
| 4 | import | The app's snapshot of a file that was never live. |
| 5 | identity presence | Presence with the sender's 16-byte account ID. |
| 6 | held update | An update the app held while it was a viewer, handed over when it closes. |

Every binary frame starts with the kind and the open ID.

### Acknowledgement and closing

An update is acknowledged with a text frame:

```json
{"type":"ack","id":1,"seq":1}
```

To close a document, send the following. The `doc` value is the server's document ID from `opened`, not the open ID.

```json
{"type":"close","doc":"<doc-id>"}
```

The agent drops an app that does not read for 10 seconds. The document stays with the agent.

To quit, send the close and read until the agent's close before shutting down the socket. On Linux a socket closed with unread data is reset, and the reset can discard the last changes the agent has not read yet.

### Other events

Other text events are `status`, `file`, `role`, `members`, and `error`. `DocsEvent` is non-exhaustive: ignore unknown future `type` values.

A `members` event carries `id` (the open ID) and a list of members. Each member has these fields:

- `user`: the account's UUID.
- `name`.
- `colour`: a palette index from 0 to 11.
- `self`: a boolean.

The Rust kit maps the palette index to `PaletteColour.light` and `PaletteColour.dark`.

## Recents WebSocket

Upgrade `GET /v1/recents`. A subscription looks like this:

```json
{"type":"subscribe","app":"wordcraft"}
```

To subscribe to the list of a team space, add the space:

```json
{"type":"subscribe","app":"wordcraft","space":"<space-id>"}
```

The initial full list, and each changed full list, arrives as `entries`. Each message has the app, the entries, an optional `push_stopped`, and an optional `space`.

To add an entry:

```json
{"type":"add","id":7,"app":"wordcraft","entry":{"path":"Notes/story.craft","title":null,"saved":true}}
```

The answer is `added`, which echoes the ID and the entry, or `error`, which carries a code and a message.

Paths are slash-separated and relative to the personal sync root. An absolute path is also accepted if it resolves inside a synced team folder.

## Blob calls

`PUT /v1/blobs` takes the raw bytes as the body, not JSON. The response contains `hash`, the BLAKE3 hex name of the manifest, and `len`, the length in bytes.

`GET /v1/blobs/<hash>` returns the bytes.

Add `?for=<absolute-or-relative-file-path>` to bind the upload or download to the space of that file. Without `for`, the personal space is used. The `for` parameter does not replace a permission check: the agent validates the path and the current membership. The content is verified and synced as a file blob.

## Compatibility and stability

The agent's `Status.api` is checked against `LOCAL_API_MIN..=LOCAL_API_MAX`. Both are currently 1.

An old agent reports an unknown call as `not_found` with the message `no such call`. The Rust client maps that case to `agent_too_old`.

These optional fields are explicitly defaulted, so an older agent may omit them: `opened.read_only`, `opened.space`, the recents `space`, the docs `identity`, and `changed_outside`. Clients must tolerate unknown `DocsEvent` variants.

The API source describes this as the local API for apps and the launcher. It does not promise stability to third parties. Treat the protocol as versioned but still evolving.

## Open questions

- No explicit third-party support or deprecation policy was found in the code or the docs.
