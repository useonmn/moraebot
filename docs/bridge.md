# Messenger bridge contract (version 1)

moraebot can talk to a messenger through a separate process, the bridge. The bridge holds the messenger tokens and the messenger connection. The core (the `moraebot` binary) keeps the access check, the inbox store, the lead, the lanes and memory. This document is the wire contract between them.

Words used below:

- **core**: the `moraebot` process.
- **bridge**: a process that speaks this contract, for example `moraebot-slack-bridge`.
- **link**: the unix socket connection between the two.

Version 1 is frozen at the first release that ships it, not at the merge that adds this file.

## Configuration

The core selects the bridge with `messenger = "bridge"` and a `[bridge]` section in `config.toml`:

```toml
messenger = "bridge"

[bridge]
socket = "/run/user/1000/moraebot/slack-bridge.sock"
owner_id = "U111AAAA"

[[bridge.allow]]
id = "U333CCCC"
kind = "user"
role = "chat"
```

- `socket` is an absolute path. It must fit in `sockaddr_un` (104 bytes on macOS, 108 on Linux).
- `owner_id` and `allow` work as they do for the other messengers. The core doesn't know the id format of the messenger behind the bridge, so it only checks that an id isn't empty.
- Access decisions stay in the core. The bridge gets `owner_id` and the allow ids in `hello_ok` and uses them only to pre-filter.

The bridge has its own config file with its token file paths, its channel and the same socket path. Default socket paths:

| OS | Path |
|---|---|
| Linux with `$XDG_RUNTIME_DIR` | `$XDG_RUNTIME_DIR/moraebot/slack-bridge.sock` |
| Linux without it | `<home>/run/slack-bridge.sock` |
| macOS | `~/Library/Application Support/moraebot/slack-bridge.sock` |

Setup writes the same absolute path into both files. Neither process works the path out at run time.

## Transport

- A unix stream socket. The bridge listens and the core connects. One core at a time.
- The socket file has mode 0600 and sits in a folder with mode 0700.
- Each side checks that the peer uid equals its own (`SO_PEERCRED` on Linux, `getpeereid` on macOS, both through tokio's `UnixStream::peer_cred()`). A mismatch closes the connection and is logged as `E_BRIDGE_PEER` at error level on every attempt.
- Either side can restart while the other keeps running. The core reconnects after 0.5 seconds, doubling up to 10 seconds.

## Framing

- UTF-8 JSON objects, one per line, each ended by `\n`.
- A line longer than 1,048,576 bytes is a protocol error and closes the connection.
- Every frame has a string field `t`, its type.
- Unknown fields are ignored.
- A frame with an unknown `t` and an `id` gets `err` with `E_BRIDGE_UNSUPPORTED`. Without an `id` it's ignored.
- Frames are never logged. Logs carry `t`, `id`, `re` and `seq` only, because message text and file URLs cross the link.

## Version

`proto` is the integer `1`. New optional fields, new capabilities and new frame types behind a capability keep `proto` at 1. Any other change raises it.

## Handshake

The bridge speaks first.

```json
{"t":"hello","proto":1,"bridge":"moraebot-slack-bridge","version":"0.9.0","platform":"slack","message_limit":3900,"hints":"Your replies are posted to Slack. ...","home":"C222BBBB","caps":{"threads":"implicit","dm":true,"edit":true,"files":true,"status":true,"reactions":false,"agent_mode":true},"spooled":3}
{"t":"hello_ok","proto":1,"core":"0.9.0","owner_id":"U111AAAA","allow":["U333CCCC","B444DDDD"],"window":32}
```

`hello` fields:

| Field | Meaning |
|---|---|
| `bridge`, `version` | Name and version of the bridge, for logs. |
| `platform` | The conversation key prefix, for example `slack`. Keys look like `slack:th:...` and `slack:dm:...`. |
| `message_limit` | The longest message the messenger takes, in characters. The core splits longer replies. |
| `hints` | Text the core gives the model about how replies are shown. |
| `home` | The send target for messages that belong to no conversation, for example the greeting fallback. Optional; see below. |
| `caps` | Capabilities, see below. |
| `spooled` | How many inbound messages wait in the spool. |

`hello_ok` fields:

| Field | Meaning |
|---|---|
| `core` | Core version, for logs. |
| `owner_id`, `allow` | The core's owner and allow list, for the bridge's pre-filter. |
| `window` | The most inbound messages the bridge may keep unacked on this connection. |

After `hello_ok` the bridge starts the spool handover and the core bumps `reconnects()`, so replies that failed while the link was down are sent again.

The core keeps the last `hello` in `<home>/bridge-hello.json` (mode 0600). When no bridge answers within 10 seconds of start, the core starts from that copy. With neither a bridge nor a copy, it fails with `E_ADAPTER_START` and `E_BRIDGE_DOWN` in the message, and the service manager retries.

Without `home`, the core has no channel for messages that belong to no conversation. The onboarding greeting and the omp version notice still go to the owner's DM when the bridge has the `dm` capability and the DM opens. They never fall back to a channel: the core sends no frame with an empty target and logs one warning with code `W_NO_HOME_CHANNEL` instead. A greeting that reached nobody is not retried; the omp version notice is tried again at the next start.

`platform`, `hints` and `home` come from the hello used at start. `message_limit` and `caps` follow the latest hello. A later hello with another `platform` is refused with `E_BRIDGE_PLATFORM` and the core stops, so the service manager starts it again with the new hello. Changed `hints` or `home` are logged and apply at the next start.

A `hello` with an empty `platform` or a `message_limit` below 100 gets `bye` with `E_BRIDGE_BAD_REQUEST`, and the core retries with backoff. A cached copy that fails the same check, or that carries another `proto`, is ignored. A handshake that keeps failing for the same cause is logged once, then at debug level until a connection is served again.

A side that can't go on sends `bye` and closes:

```json
{"t":"bye","code":"E_BRIDGE_PROTO","message":"the core speaks proto 1, the bridge sent 2"}
{"t":"bye","code":"E_BRIDGE_BUSY","message":"another core is connected"}
```

A second core gets `bye` with `E_BRIDGE_BUSY` in place of `hello`. The first one stays connected. A `hello` with another `proto` gets `bye` with `E_BRIDGE_PROTO` and no `reconnects()` bump.

## Capabilities

| Key | Values | Core behavior |
|---|---|---|
| `threads` | `"none"`, `"implicit"`, `"create"` | `implicit`: the root message id is the thread, so `create_thread` returns `from_message_id` with no frame. `create`: the core sends a `thread` request. `none`: `create_thread` fails with `E_ADAPTER_THREAD`, no frame. |
| `dm` | bool | false: `open_dm` fails with `E_ADAPTER_DM`, no frame. |
| `edit` | bool | false: `edit` fails with `E_ADAPTER_EDIT`, no frame. Lane news then posts a new message. |
| `files` | bool | false: `fetch_attachment` and `send_file` fail with `E_ADAPTER_FILES`, no frame. |
| `status` | bool | false: `start_typing` returns a handle that does nothing, no frame. |
| `reactions` | bool | Reserved. The core sends no `react` in version 1. |
| `agent_mode` | bool | Informational, logged at connect. |

## Inbound messages and the spool

```json
{"t":"message","seq":41,"message_id":"D111EEEE:1700000000.000100","author_id":"U111AAAA","author_is_bot":false,"channel_id":"D111EEEE","thread_id":"D111EEEE:1700000000.000100","is_dm":true,"text":"hello","attachments":[{"name":"report.pdf","size":48213,"mime":"application/pdf","ref":"https://files.slack.com/files-pri/T111-F111/download/report.pdf"}],"redelivery":false}
{"t":"ack","seq":41}
```

- The fields map one to one to the core's `InboundEvent` and `InboundAttachment`.
- `thread_id` is an opaque string, or `null` outside a thread. For a Slack DM the bridge fills `<channel>:<thread_ts or ts>`, so every DM is a thread.
- `ref` is opaque. The core sends it back unchanged in `fetch` and never logs it.
- `seq` has meaning only inside one connection.

Delivery is at least once:

1. The bridge writes a message to its spool before it acks its messenger, and deletes it only on `ack`.
2. After every `hello_ok` the bridge sends every unacked message again in `seq` order, then new ones as they arrive. `redelivery` is true when the message was already sent on an earlier connection.
3. The bridge keeps at most `window` messages unacked on a connection. A bridge that sends more breaks the contract: the core closes the connection without delivering the extra message.
4. The core sends `ack` after it handled the message, whatever the outcome: a stored row, a duplicate, a drop by the access check, a notice, or rejected files. It doesn't ack when the store failed. That message keeps its window slot and is sent again only on the next connection, which is when its attempt count moves.
5. The bridge stops redelivering a message after a fixed number of attempts (counted per connection), logs the drop and moves on, so one bad message can't block the spool.
6. The core dedupes on `(platform, message_id)`.
7. The bridge keeps spooling inbound while a download or any other request is in flight. The core handles events one at a time, and a slow event (a download) holds its window slot until its ack.

One known gap: a message from a listed sender (the chat runtime, not the owner) is deduped only in the core's memory. If the core dies after it started the answer and before it sent the `ack`, the redelivered message can be answered a second time. No storage is added for this.

## Requests

The core sends a request with a fresh `id`, an integer above zero that grows per connection. The bridge answers each with exactly one `ok` or `err` whose `re` is that id. Requests are multiplexed on the one socket: correlation is always by `id` and `re`, never by line order, and the bridge may answer them in any order.

```json
{"t":"send","id":7,"target":"D111EEEE:1700000000.000100","text":"done"}
{"t":"ok","re":7,"message_id":"D111EEEE:1700000002.000200"}

{"t":"edit","id":8,"target":"C222BBBB:1700000000.000100","message_id":"C222BBBB:1700000003.000300","text":"[lane 2] halfway"}
{"t":"ok","re":8}

{"t":"thread","id":9,"channel_id":"C222BBBB","from_message_id":"C222BBBB:1700000000.000100","name":"fix the build"}
{"t":"ok","re":9,"thread_id":"C222BBBB:1700000000.000100"}

{"t":"dm","id":10,"user_id":"U111AAAA"}
{"t":"ok","re":10,"target":"U111AAAA"}

{"t":"status","id":11,"target":"D111EEEE:1700000000.000100","state":"start"}
{"t":"ok","re":11}
{"t":"status","id":12,"target":"D111EEEE:1700000000.000100","state":"stop"}
{"t":"ok","re":12}

{"t":"file","id":14,"target":"C222BBBB:1700000000.000100","path":"/abs/work/out/report.pdf","name":"report.pdf"}
{"t":"ok","re":14,"message_id":"F111"}

{"t":"react","id":15,"target":"C222BBBB:1700000000.000100","message_id":"C222BBBB:1700000000.000100","emoji":"eyes"}
{"t":"err","re":15,"code":"E_BRIDGE_UNSUPPORTED","message":"reactions are not supported"}
```

- `send`, `edit`, `thread` and `dm` must be answered within 30 seconds. Past that the core gives up: it sends `cancel` and the call fails with the operation's default code. `fetch` and `file` have no such deadline; the core's stall watchdog and upload time limit bound them.
- The core does not wait for the answer to `status`, so a slow or failing status call never delays a turn. A failed `status` is logged by the bridge only.
- `status start` shows a working indicator for `target` until `stop` or until the connection closes. Keeping it alive (Slack drops a status after two minutes) is the bridge's job.
- `file`: the bridge reads `path` whole and uploads it. The core has already checked the path.
- `react` is reserved. Version 1 cores don't send it.

## Attachment download and progress

```json
{"t":"fetch","id":13,"attachment":{"name":"report.pdf","size":48213,"mime":"application/pdf","ref":"https://files.slack.com/files-pri/T111-F111/download/report.pdf"},"dest":"/abs/work/.moraebot-inbox/slack-th-D111EEEE-1700000000.000100/12/report.pdf","max_bytes":null}
{"t":"progress","re":13,"bytes":32768}
{"t":"progress","re":13,"bytes":48213}
{"t":"ok","re":13,"bytes":48213}
```

- `dest` is absolute. Its folder exists; the core creates it with mode 0700.
- The bridge writes through `<dest>.part` (mode 0600, created new) and renames it to `dest` on success. It never follows redirects.
- `max_bytes` is an integer, or `null` when the core sets no limit. Past the limit the bridge stops, removes the part file and answers `E_FILE_TOO_LARGE`. Inbox downloads of inbound attachments are sent with `null` today, so the bridge must apply its own size cap when `max_bytes` is `null`.
- The core checks the `bytes` of the final `ok` against `max_bytes` too, so a bridge that skips `progress` cannot pass the limit.
- Each `progress` frame carries the `re` of its `fetch`. `bytes` is the running total written. While bytes flow the bridge sends one at least every second.
- The bridge has its own stall guard: 120 seconds without a byte ends the download with `E_FILE_FETCH`.
- The core feeds each `progress` into its own stall watchdog, so a bridge that stops reporting is caught there too.

## Cancel

```json
{"t":"cancel","id":13}
```

`id` here names the request to stop, not a new request. The core sends it when it gives up on a request, for example on its stall watchdog or an upload time limit. The bridge stops the work and removes a partial file. A late `ok` or `err` for that id is ignored. `cancel` gets no answer.

## Events

```json
{"t":"event","name":"reconnected"}
```

The bridge's messenger connection came back up. The core bumps `reconnects()`, so replies that failed while the messenger was unreachable are sent again.

## Liveness

```json
{"t":"ping","n":1}
{"t":"pong","n":1}
```

The core sends `ping` every 15 seconds and the bridge answers with the same `n`. Either side closes a connection that was silent for 45 seconds.

## Errors

```json
{"t":"err","re":7,"code":"E_ADAPTER_SEND","message":"E_SLACK_API: channel_not_found"}
{"t":"err","re":13,"code":"E_FILE_TOO_LARGE","message":"download is larger than its size limit"}
```

- `code` is a contract code. Messenger detail goes into `message`.
- Codes the core acts on:
  - `E_ADAPTER_UPLOAD_DENIED`: the file is skipped.
  - `E_FILE_DENIED`: the user gets the "not allowed" notice.
  - `E_FILE_TOO_LARGE`: the user gets the size notice.
- Every other code is a plain failure of that call.
- Each operation passes only the codes of its own family. Any other code, link codes and the core-made `E_BRIDGE_DOWN` and `E_BRIDGE_PLATFORM` included, becomes the default code of the operation, with the original code kept at the front of the message. A bridge can't trigger the upload or file branches from the wrong operation.

| Request | Default code | Codes that pass |
|---|---|---|
| `send` | `E_ADAPTER_SEND` | `E_ADAPTER_SEND` |
| `edit` | `E_ADAPTER_EDIT` | `E_ADAPTER_EDIT` |
| `thread` | `E_ADAPTER_THREAD` | `E_ADAPTER_THREAD` |
| `dm` | `E_ADAPTER_DM` | `E_ADAPTER_DM` |
| `status` | `E_ADAPTER_TYPING` | `E_ADAPTER_TYPING` |
| `fetch` | `E_FILE_FETCH` | `E_ADAPTER_FILES`, `E_FILE_FETCH`, `E_FILE_HOST`, `E_FILE_REMOTE`, `E_FILE_WRITE`, `E_FILE_PATH`, `E_FILE_INCOMPLETE`, `E_FILE_DENIED`, `E_FILE_TOO_LARGE` |
| `file` | `E_ADAPTER_UPLOAD` | `E_ADAPTER_UPLOAD`, `E_ADAPTER_UPLOAD_DENIED`, `E_ADAPTER_FILES` |

Link codes:

| Code | Meaning |
|---|---|
| `E_BRIDGE_PROTO` | The two sides speak different `proto` versions. |
| `E_BRIDGE_BUSY` | Another core is connected. |
| `E_BRIDGE_STOPPING` | Sent in `bye` by a bridge that is shutting down. The core reconnects with backoff. |
| `E_BRIDGE_UNSUPPORTED` | Unknown frame type, or a capability the bridge lacks. |
| `E_BRIDGE_BAD_REQUEST` | A missing or invalid field. |
| `E_BRIDGE_PEER` | The peer uid differs. Logged only. |
| `E_BRIDGE_DOWN` | Made by the core: no link. |
| `E_BRIDGE_PLATFORM` | Made by the core: a hello changed `platform`. |

## With the link down

Every core call fails at once with the default code of its operation and `E_BRIDGE_DOWN` in the message. A request in flight when the link drops fails the same way. The reply row stays `reply_pending` and is sent again after the next `hello_ok`.

Outbound delivery is therefore at least once. A send the messenger accepted just before the link dropped is sent again after the reconnect. This is the same class of duplicate as a core crash between posting a reply and marking it done.

## The Slack bridge

`moraebot-slack-bridge` is the bridge for Slack. It ships next to `moraebot` and runs as its own service.

### What it reports

- `platform` is `slack`, so the conversation keys and sessions of the in-core Slack adapter (before the bridge) carry over.
- `message_limit` is 3900. `home` is the configured `channel_id`.
- `caps`: `threads: "implicit"`, `dm`, `edit`, `files` and `status` true, `reactions` false, `agent_mode` true.
- `dm` answers with the user id itself as the target: `chat.postMessage` to a user id posts into the app's DM with that user.
- A `fetch` without `max_bytes` is capped at 1 GiB.
- Slack messages with a subtype other than `file_share` and `thread_broadcast` are dropped, and the drop is logged at info level with the subtype.

### Agent mode

- Every DM is a thread. A root DM gets `thread_id` `<channel>:<ts>`, so the reply carries its `thread_ts`. A DM inside a thread gets `<channel>:<thread_ts>` and is answered in that thread.
- `status start` on a DM thread target (`D...:<ts>`) calls `assistant.threads.setStatus` with a non-empty status and repeats it every 90 seconds, because Slack removes a status after two minutes without a message. `stop` sends an empty status. When several `start`s for one target are open (a lead turn and a lane), the status stays until the last `stop`. When the core disconnects, the bridge clears every status it set.
- Every other target answers `ok` with no Slack call. Until a live check shows that `setStatus` works in channel threads, statuses are used in DM threads only.
- A failed `setStatus` is logged by the bridge and still answers `ok`.
- `setStatus` needs `chat:write`, or `assistant:write`. Some agent features need a paid Slack plan; threaded DM replies work on any plan.

### Files and state

| Path | Content |
|---|---|
| `<home>/slack-bridge.toml` | The bridge config (below). |
| `<home>/slack-bridge/lock` | An exclusive lock. A second bridge on the same home exits with `E_BRIDGE_RUNNING`. |
| `<home>/slack-bridge/spool/` | One file per inbound message not yet acked by a core (mode 0600, folder 0700). |
| `<home>/logs/slack-bridge.log.*` | The bridge log, next to the core's. |

`<home>` is the moraebot home (`~/.moraebot` unless `MORAEBOT_HOME` is set), the same folder the core uses.

The spool holds raw Slack payloads, message text and file URLs included. Only messages a core could take are written to it: a DM or a message in `channel_id`, not posted by the bot. Once a core has said `hello_ok`, the sender must also be its owner or a listed id; the bridge keeps that list in memory, so right after a bridge restart every sender is spooled until a core connects. A Slack event larger than one line (`MAX_LINE`) is refused by the WebSocket. A spooled message that would still encode longer than one line is removed with an error log instead of being sent. An entry leaves it on the core's `ack`, when the message is not for the core (another channel, an unlisted sender), after 5 attempts, or when it is older than 7 days. At 10,000 entries a new message is dropped with an error log. A message that cannot be written to the spool is not acked to Slack, so Slack delivers it again.

Bridge start errors:

| Code | Meaning |
|---|---|
| `E_BRIDGE_RUNNING` | Another bridge holds the lock. |
| `E_BRIDGE_STATE` | The state folder, the lock or the signal handlers cannot be set up. |
| `E_BRIDGE_SOCKET` | The socket cannot be bound, or its path holds something that is not a socket. |
| `E_BRIDGE_SPOOL` | The spool cannot be opened. |

### Running the bridge by hand

`slack-bridge.toml` names the socket, the channel the bridge reports as `home`, and the two token files:

```toml
socket = "/run/user/1000/moraebot/slack-bridge.sock"
channel_id = "C222BBBB"
bot_token_file = "/home/me/.moraebot/secrets/slack-bot.token"
app_token_file = "/home/me/.moraebot/secrets/slack-app.token"
```

- `socket` must be the same absolute path as `[bridge] socket` in the core's `config.toml`.
- `channel_id` starts with `C` or `G`. The token files must be regular files with mode 0600.
- The owner and the allow list are not here: the bridge gets them from the core in `hello_ok`.

Commands:

```sh
moraebot-slack-bridge check-config [--config PATH]   # OK/FAIL lines for the config and both token files
moraebot-slack-bridge run [--config PATH]            # runs until SIGTERM or SIGINT
moraebot-slack-bridge --version
```

`--config` defaults to `<home>/slack-bridge.toml`. It moves only the config: the lock, the spool (`<home>/slack-bridge/`) and the log (`<home>/logs/`) always use the moraebot home (`MORAEBOT_HOME`, else `~/.moraebot`). To run a second bridge, give it its own `MORAEBOT_HOME`, not just another `--config`; with the same home it fails with `E_BRIDGE_RUNNING`. Start the bridge first, then `moraebot run`. Either can restart while the other runs: the core reconnects, and messages that arrive while no core is connected wait in the spool. On SIGTERM or SIGINT the bridge says `bye` with `E_BRIDGE_STOPPING`, removes the socket file and exits 0.

### Moving a Slack config by hand

A `config.toml` with a `[slack]` section or `messenger = "slack"` fails with `E_SLACK_MOVED`, because Slack no longer runs inside the core. To move it:

1. In `config.toml`, set `messenger = "bridge"`.
2. Rename `[slack]` to `[bridge]` and `[[slack.allow]]` to `[[bridge.allow]]`. Keep `owner_id`, `max_bot_replies` and the allow entries.
3. Move `channel_id`, `bot_token_file` and `app_token_file` out of that section into a new `<home>/slack-bridge.toml` with mode 0600.
4. Add the same `socket` line to both files. Use the default path for your OS from [Configuration](#configuration).
5. Run `moraebot check-config` and `moraebot-slack-bridge check-config`, then start the bridge and the core.

Before:

```toml
messenger = "slack"

[slack]
owner_id = "U111AAAA"
channel_id = "C222BBBB"
bot_token_file = "/home/me/.moraebot/secrets/slack-bot.token"
app_token_file = "/home/me/.moraebot/secrets/slack-app.token"
```

After, `config.toml`:

```toml
messenger = "bridge"

[bridge]
socket = "/run/user/1000/moraebot/slack-bridge.sock"
owner_id = "U111AAAA"
```

and `slack-bridge.toml`:

```toml
socket = "/run/user/1000/moraebot/slack-bridge.sock"
channel_id = "C222BBBB"
bot_token_file = "/home/me/.moraebot/secrets/slack-bot.token"
app_token_file = "/home/me/.moraebot/secrets/slack-app.token"
```

`moraebot setup slack` writes both files in this layout for a new setup.

After the move, new DMs are threaded: each DM thread is its own conversation, with its own session and memory. An old `slack:dm:<channel>` conversation still gets its pending replies.
