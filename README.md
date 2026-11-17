# moraebot

moraebot lets you talk to a coding agent on your Mac from a chat app. You send a message in Discord or Slack, moraebot hands it to omp, a coding agent CLI, running on your Mac, and the reply comes back in the same conversation.

## Status

This is slice 3. It adds files: you can attach files to a message, and omp can send files back.

- Discord or Slack, one at a time. Other messengers come later.
- macOS only. It runs as a per-user LaunchAgent.
- One owner. Only you can talk to the bot (`access_mode = "owner_only"`).
- One task at a time. Messages wait in a queue and run in order.

## Read this first: yolo mode

> **Warning**
>
> moraebot runs omp without asking for approval. Anyone who can talk to the bot can run commands on this Mac, with your user's permissions.
>
> - Keep `access_mode = "owner_only"`. It's the default and the only mode for now.
> - Use a private channel that only you can see.
> - Turn on two-factor login for your Discord or Slack account. That account is now a key to your Mac.
> - The work folder is not a sandbox. omp can read and change files outside it.
> - Never share the bot token. Anyone holding it can pretend to be your bot.

The bot only runs while you're logged in to macOS and the Mac is awake.

## Install

You'll need macOS, a Discord or Slack account, and omp already installed and working in your terminal.

### 1. Install moraebot

```sh
curl -fsSL https://github.com/useonmn/moraebot/releases/latest/download/install.sh | sh
```

This downloads the latest release for Apple Silicon Macs, checks it against the release's SHA-256 checksum, makes sure it runs, and only then puts it at `~/.local/bin/moraebot`. If anything fails, your existing moraebot stays as it was. When `~/.local/bin` isn't on your `PATH` yet, the script prints the line to add to your shell startup file.

On a fresh install (no `config.toml` in `${MORAEBOT_HOME:-~/.moraebot}` yet), it ends with "Next steps:": `moraebot init`, `moraebot check-config`, `moraebot install-launchd`, then a line telling you to message the bot. If `moraebot` isn't on your `PATH`, those lines use its full path. An upgrade prints no next steps.

Two settings change what it does. Put them right before `sh`, not before `curl`:

- `MORAEBOT_VERSION` picks a release instead of the latest one.
- `MORAEBOT_INSTALL_DIR` puts the binary in another folder.

```sh
curl -fsSL https://github.com/useonmn/moraebot/releases/latest/download/install.sh | MORAEBOT_VERSION=v0.1.0 sh
```

Download it with curl like this, not with a browser. A file saved by a browser can be flagged by macOS Gatekeeper, and the unsigned binary may then refuse to open.

### 2. Build from source (optional)

Skip this if you used the line above. It's for developers who want to build moraebot themselves.

Install Rust:

```sh
brew install rust
```

Or use rustup if you prefer:

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

Then build moraebot:

```sh
git clone https://github.com/useonmn/moraebot.git
cd moraebot
cargo build --release
```

The binary ends up at `target/release/moraebot`. The commands below call it `moraebot`. Either run it by that path or copy it somewhere on your `PATH`.

### 3. Pick a messenger

moraebot talks to one messenger at a time: Discord or Slack. Each one has its own setup guide with the exact app settings, in the order the portal shows them:

- Discord: [docs/guides/discord.md](docs/guides/discord.md)
- Slack: [docs/guides/slack.md](docs/guides/slack.md)

Follow the guide for the messenger you want, then come back here. The guides are the only place the fixed settings are written down, so this README doesn't repeat them.

You can also print a guide in the terminal:

```sh
moraebot guide discord
moraebot guide slack
```

### 4. Set up with your agent (optional)

Each guide ends with an "Agent-assisted setup (optional)" section. It's a prompt you can hand to a coding agent so it walks you through the portal clicks. The agent never needs your tokens. You type those into moraebot yourself.

### 5. Run `moraebot init`

On the first run:

```sh
moraebot init
```

It asks a few questions, in order:

1. Which messenger to use (Discord or Slack).
2. That messenger's questions (see the list below). Tokens aren't echoed while you type or paste them. Each one is saved to its own file under `~/.moraebot/secrets/` with mode `600` (readable only by you).
3. The path to `omp` (found on your `PATH`, you confirm it) and the work folder. Press Enter to accept the default `~/moraebot-work`.

Then it creates `~/.moraebot` (mode `700`), writes `~/.moraebot/config.toml`, and runs the same checks as `check-config`. If a config already exists, it asks before overwriting it. The default answer is No.

When it writes a new config, it also leaves `~/.moraebot/onboarding.request`, so the bot says hello at its next start (see First conversation). If every check passed, it then prints the next steps: `moraebot check-config`, `moraebot install-launchd`, or `moraebot run` to run it in the terminal instead. Keeping the existing config leaves no request, and neither does `moraebot setup`.

### 6. Change or add a messenger

To switch messengers, or to fix one messenger's answers, run setup for it:

```sh
moraebot setup slack
moraebot setup discord
```

setup asks only that messenger's questions, keeps the rest of your config, and makes that messenger the active one. When a section already exists, its current ids are offered as defaults and pressing Enter keeps a saved token.

Discord asks:

1. Your Discord owner user id.
2. The Discord channel id.
3. The Discord bot token (saved to `~/.moraebot/secrets/discord.token`).

Slack first prints a link that creates the app with the manifest filled in, then asks:

1. Your Slack owner member id (starts with `U`).
2. The Slack channel id (starts with `C` or `G`).
3. The Slack bot token, `xoxb-...` (saved to `~/.moraebot/secrets/slack-bot.token`).
4. The Slack app-level token, `xapp-...` (saved to `~/.moraebot/secrets/slack-app.token`).

Restart moraebot after setup so it picks up the change.

### The config file

The written config looks roughly like this. You can edit it by hand later. Only the section named by `messenger` is used. The other one can stay in the file, so switching back is a one-word change.

```toml
# The active messenger: "discord" or "slack". Only one runs at a time.
messenger = "discord"
# Folder omp starts in. This is not a sandbox.
work_dir = "~/moraebot-work"
# Absolute path to omp.
omp_path = "<PATH_TO_OMP>"
# Only owner_only exists for now.
access_mode = "owner_only"

[discord]
# Your Discord user id. Only this account can talk to the bot.
owner_id = <YOUR_DISCORD_USER_ID>
# The one channel the bot listens in. Threads under it and your DMs also work.
channel_id = <YOUR_CHANNEL_ID>
# File holding the bot token. Must be mode 600.
token_file = "~/.moraebot/secrets/discord.token"

# [slack]
# # Your Slack member id. Only this account can talk to the bot.
# owner_id = "<YOUR_SLACK_MEMBER_ID>"
# # The one channel the bot listens in. Thread replies under it and your DMs also work.
# channel_id = "<YOUR_CHANNEL_ID>"
# # Token files. Both must be mode 600.
# bot_token_file = "~/.moraebot/secrets/slack-bot.token"
# app_token_file = "~/.moraebot/secrets/slack-app.token"

[omp]
# Optional. No time limit when left out.
# turn_timeout_minutes = 30
# Optional. No cost limit when left out.
# max_cost_usd = 5.0
```

Configs written by slice 1 (top-level `owner_id` and `token_file`, no `messenger` line) still load as Discord. You don't need to rewrite them.

Set `MORAEBOT_HOME` if you want moraebot's home somewhere other than `~/.moraebot`.

### 7. Check the config

```sh
moraebot check-config
```

Every line should start with `OK`. A `FAIL` line says what to fix, for example a token file that other users can read (`chmod 600 ~/.moraebot/secrets/slack-bot.token`). The command exits non-zero if anything fails. Use `--config <path>` to check a different file.

### 8. Start it in the background

```sh
moraebot install-launchd
```

This copies the binary to `~/.moraebot/bin/moraebot`, writes `~/Library/LaunchAgents/dev.moraebot.gateway.plist`, and loads it with `launchctl`. macOS keeps moraebot running from now on and starts it again after a crash or a new login. Add `--dry-run` to print the plist and the `launchctl` commands without running them.

You can also run it in the foreground, which is handy for a first try:

```sh
moraebot run
```

Stop it with Ctrl+C.

## Lead and lanes

One omp, the lead, answers every thread and DM of the active messenger. It knows which conversation each message came from, and its reply always goes back to that conversation. Because it's one omp, it remembers what was said in all of them.

- **Lanes for long work.** When a task is long, the lead hands it to a lane: a separate omp that works in the background while the lead keeps answering you. At most 3 lanes run at once. Lanes work in the same work folder as the lead, so two lanes could touch the same files; the lead is told not to give them the same files at the same time.
- **Done checks.** Every lane starts with a `done` line that says how to tell the work is finished. There are two kinds of check: `command ["program","arg"]` passes when the program exits with code 0, and `file <path>` passes when the path exists. Join several with `;`. When the lane thinks it's done, it calls `lane_done` with its evidence (what it did and how it checked it). moraebot then runs every check in the work folder, with the same environment omp gets minus the messenger variables (model-provider credentials stay, as they do for omp itself), and gives each command 120 s. If a check fails, the claim is refused with the reasons and the lane keeps working. There's no limit on how often it may try again. A lane that stops without a passed claim still goes to the lead: unchecked if it never claimed, or with `E_DONE_UNMET` and the reasons if its last claim failed. Anything a check command leaves running in its own process group is stopped when the check ends. A process that detached itself into a new session is not.
- **Lane results.** When a lane finishes, its result goes to the lead, not straight to you. The result says whether the lane passed its Done checks. The lead checks it and answers in the conversation that started the lane. A lane that finished can be given more work later; it picks up its own history.
- **Follow-ups while it works.** If you send another message in the same conversation while the lead is working on it, that message is folded into the answer it's writing, and you get one reply. Only one follow-up is folded in per answer; anything after that waits for the next turn. Messages from other conversations, and lane results, also wait their turn.
- **Restarts.** A lane that was running when moraebot stopped isn't resumed. You get one notice in the conversation that started it. Ask the lead to try again if you still need it.

The lead controls lanes only through three tools moraebot gives it: `lane_start`, `lane_message` and `lane_status`. Each lane runs a full omp, so three busy lanes cost about as much as four omp sessions.

## Memory

The lead has a small memory: plain markdown files in `~/.moraebot/memory/`. They tell it who it is and who you are.

```text
~/.moraebot/memory/
  system/persona.md     who the lead is and how it works
  system/human.md       what the lead knows about you
  system/identity.md    optional; you create it
```

- **Seeds.** On start, moraebot creates `memory/` and `memory/system/` (mode 700) and writes `persona.md` and `human.md` (mode 600) from short built-in texts, but only when they're missing. A file that's already there is never overwritten, so your edits stay.
- **File format.** Every file starts with a frontmatter block: a `---` line, `key: value` lines, and a closing `---` line. `description` is required (one line, at most 1024 characters). Other allowed keys are `read_only`, `limit`, `kind`, `aliases`, `provenance` and `expires`. `read_only`, `expires` and `provenance` work as described below. The rest are read and kept, but don't change anything yet. That includes `limit`: it has no effect.
- **When it's read.** Every time the lead starts, every `.md` file under `memory/system/` is put into one memory block at the end of the lead's system prompt: the persona first, then `identity.md` if it exists, then the other files sorted by path. A change takes effect at the next lead start, for example after a restart.
- **Bad files don't stop the lead.** Hidden files are skipped. A symlink, a non-regular file, or a file that's bigger than 16 KiB, not UTF-8, or has broken frontmatter shows up as a one-line marker with its path and an error code (such as `E_MEMORY_FRONTMATTER` or `E_MEMORY_FILE`) instead of its text. The whole block is kept under 64 KiB; files past that are left out with `E_MEMORY_BUDGET`. The log gets only the path and the code, never the content.
- **Read tools.** The lead (not the lanes) gets two tools. `memory_list` lists every memory file with its description and size. `memory_read` returns one file's frontmatter and body. Paths are relative to `memory/` and must start with `system/`, `reference/`, `notes/`, `people/` or `skills/`; anything else is refused with `E_MEMORY_PATH`.
- **Write tools.** The lead also gets six tools that change memory: `memory_create`, `memory_str_replace`, `memory_insert`, `memory_delete`, `memory_rename` and `memory_update_description`. Every call needs a `reason`: one line, at most 200 characters. A write that changes nothing is refused with `E_MEMORY_NOOP`. `persona.md` and `human.md` can be edited but not deleted or renamed (`E_MEMORY_PROTECTED`). Write paths may use only ASCII letters, digits, `.`, `_` and `-`.
- **Only while answering you.** The lead can write memory only in a turn that answers one of your messages. In a turn that handles a lane result, every write is refused with `E_MEMORY_ORIGIN`.
- **History.** Every write is recorded in `~/.moraebot/inbox.sqlite`, in two tables: `memory_commits` (when, who, why, and in which conversation) and `memory_snapshots` (the full file text after the change). The record is saved first and the file is changed after it, so a crash in between is repaired at the next start. There's no git. The first time this version starts, the memory files as they are then are recorded once as an import, including any edits made before.
- **The block uses recorded text.** The memory block is built from the recorded text, not from the files on disk. Changes show up at the next lead start.
- **Hand edits.** omp runs in yolo mode, so the lead's normal file tools, or you, can still change these files directly. Such an edit isn't used: the file counts as dirty, the block keeps the last recorded text and lists the dirty path at its end, and every lead write is refused with `E_MEMORY_DIRTY` until you accept it. A new `.md` file nobody recorded, a missing recorded file, and a symlink or other special file are dirty too.
- **Accepting hand edits.** `moraebot memory accept` records your hand edits as one commit, so they reach the block at the next start. It needs the inbox database, so stop the gateway first: `moraebot uninstall-launchd`, then `moraebot install-launchd` again afterwards, or Ctrl-C for `moraebot run`. While the gateway runs, it's refused with `E_ALREADY_RUNNING`. With no paths it accepts every dirty file; with paths it accepts only those, and a path that isn't dirty is refused with `E_MEMORY_NOOP`. `--reason` replaces the default reason. On success it prints one JSON line with the commit id and the accepted paths and exits 0. A refusal prints its code and exits 1, and nothing is recorded. A bad command line exits 2.

```sh
moraebot memory accept [--config PATH] [--reason TEXT] [PATH...]
```

- **Read-only files.** A file whose frontmatter has `read_only: "true"` (or `read_only: true`) can't be changed by the lead. `memory_str_replace`, `memory_insert`, `memory_update_description`, `memory_delete` (the file, or a folder holding it) and `memory_rename` (as the source) are refused with `E_MEMORY_READ_ONLY`, and nothing changes. Any other value, such as `"false"`, doesn't lock the file. The lead can add the line to lock a file, but it can't remove it again. You still can: edit the file by hand, then run `moraebot memory accept`.
- **Expiry.** `expires` takes a date like `2026-11-17` (that day at 00:00 UTC) or a time with a zone, like `2026-11-17T09:30:00Z` or `2026-11-17T18:30:00+09:00`. Fractions of a second are ignored. A value that can't be read, including a time without a zone, means the file never expires. Once that moment has passed, the file is left out of the block (and out of the notes index) from the next lead start, with no marker, and the log gets its path with `E_MEMORY_EXPIRED`. The file itself stays, and `memory_read` and `memory_list` still show it.
- **Secrets.** Memory refuses text that looks like a secret: AWS access keys, the first line of a private key (`-----BEGIN ... PRIVATE KEY-----`), `Authorization: Bearer` headers, `sk-...` keys, GitHub tokens (`ghp_...` and its siblings, `github_pat_...`), GitLab tokens (`glpat-...`), Slack tokens (`xoxb-...` and similar, `xapp-...`), and lines like `token=...` or `password=...` with a value of 8 characters or more. A lead write is refused with `E_MEMORY_SECRET` when the new file text, a new path (create, or the target of a rename) or the reason looks like that. So is `moraebot memory accept`, for a file's text, a new file's path, or `--reason`. The refusal names the field and the kind of secret, never the secret itself. Deleting or moving away a file with a secret-like path still works. The files imported at the first start of the M2 version were not scanned, and the block isn't redacted: if one of them holds a secret, remove it by hand, then run `moraebot memory accept`.
- **Notes index.** When memory has files outside `system/` and `skills/`, the block ends with an `<external_projection>` element that lists them by name only: one line per folder, in path order, without `.md`, and without their content. Names with a comma, a quote, or a leading or trailing space are written in JSON quotes. Read a note with `memory_read`. The index can take up to 40 KiB, on top of the 64 KiB for system files. Names that don't fit are left out in path order, and each folder that lost names gets one `<omitted_names dir="..." count="N" code="E_MEMORY_BUDGET"/>` line.
- **Repaired arguments.** Sometimes a tool call arrives with one argument swallowed by another, because a `<parameter name="...">` tag wasn't closed. If a required argument is missing and another text argument holds exactly that marker, moraebot splits it back out and checks the call as usual. A successful write that needed this carries `"repaired": ["<argument>", ...]` in its result JSON. A call with every argument present is never changed.
- **Claims.** Every write tool takes three optional arguments that say where the words came from. `source` is `user` (you approved the lead's proposal), `agent` (the lead's own view) or `inferred` (the lead's guess). With `source: user` the lead also gives `user_quote`, your exact words, and `quote_message`, the number from the `message=<id>` tag line of your message. `user_quote` or `quote_message` without `source: user` is refused with `E_MEMORY_ARGS`, and a quote that looks like a secret is refused with `E_MEMORY_SECRET`. A write without `source` records no claim and works as before, with one exception: changing a file whose `provenance:` line starts with `user:` or `quoted:` turns that line into `mixed:`.
- **When a claim counts as `user`.** moraebot checks the quote against the messages it stored, in this order: the quote and the message number are both there and the quote isn't blank; the quote is at most 300 characters; the message is yours, from this conversation, from the person this turn answers; it's the message this turn answers, or one you sent into the same turn while the lead worked; the quote appears in that message's text. Then the whole message must be one approval word: `응`, `네`, `예`, `좋아`, `그래`, `지워`, `지워줘`, `삭제해`, `삭제해줘`, `진행해`, `yes`, `ok`, `okay`, `do it`, `go ahead`, `delete it` or `yes delete it`. Spaces around it and one trailing `.` or `!` are fine, and English letter case doesn't matter. Last, the lead's reply right before your message is the proposal: no other message of the conversation came in between. If all of that holds, the claim is stored as `user`, with a short fingerprint of the proposal.
- **`quoted` and downgrades.** A write is never refused because a `user` claim fails. If the quote really is from your message but isn't the whole message, isn't an approval, or has no proposal right before it, the claim is stored as `quoted`. Any other failure stores it as `agent`. Either way the result JSON carries `"downgraded": true` and the code of the first check that failed: `E_MEMORY_QUOTE_MISSING`, `E_MEMORY_QUOTE_LONG`, `E_MEMORY_QUOTE_ROW`, `E_MEMORY_QUOTE_TURN`, `E_MEMORY_QUOTE_TEXT`, `E_MEMORY_QUOTE_APPROVAL` or `E_MEMORY_QUOTE_PROPOSAL`. The full result is kept in the `claim_json` column of `memory_commits`.
- **The `provenance:` line.** When a write names a `source`, moraebot writes the file's `provenance:` frontmatter line itself. The lead can't set or change it (`E_MEMORY_PROVENANCE`). The line starts with one of five words: `user:`, `quoted:`, `agent:`, `inferred:` or `mixed:`. Changing a `user:` or `quoted:` file with a different kind, or a line of any other shape, gives `mixed:`. `agent:` and `inferred:` simply replace each other. Writing the line rewrites the whole frontmatter in a standard form, for example `read_only: "true"` becomes `read_only: true`, which means the same. Renames and deletes keep the text as it is, so the claim goes only into the history. The lead's memory block shows the line as a `provenance="..."` attribute.
- **Limits of claims.** If you edit a message in the messenger later, moraebot still compares against the text it first stored. A reply the lead sent before this version can't be a proposal, so the first approval after upgrading is stored as `quoted`. A `provenance:` line is a record of where words came from, not a permission for anything.
- **What M4 doesn't do.** No recall, no automatic learning, lanes still get no memory, and no file needs a quote yet. Later slices of the memory roadmap add those.
- **The pinned block.** One lead process keeps the memory block it started with, byte for byte, even when memory changes under it. That keeps the system prompt identical from message to message, so the model's prompt cache stays valid. A memory change alone never restarts the lead; it's announced in a notice instead (below).
- **Re-pinning.** The block is rebuilt at only two points. After omp compacts the conversation (a finished automatic or manual compaction, not an aborted or skipped one), the next message renders the block again and restarts the lead only if the new block differs from the pinned one. After `moraebot memory refresh`, the next message always restarts the lead, even when the block is the same. A restart happens only right before moraebot sends the lead its next message, when omp's last turn has finished and omp has said it's idle. If it isn't idle yet, the restart waits for a later message. The new lead uses the same session folder, so the conversation history is kept. The log line `omp lead re-pinned by restart` carries the `reason` (`compaction` or `refresh`) and `restart_ms`.
- **`moraebot memory refresh`.** This works while the gateway runs. It doesn't open the inbox database; it only creates `~/.moraebot/memory/.refresh` (mode 600) and prints `{"refresh":"requested"}`. Every lead start removes that file. If the memory folder doesn't exist yet, it prints `E_MEMORY_LAYOUT` and exits 1.

```sh
moraebot memory refresh [--config PATH]
```

- **Notices.** When one of your messages reaches the lead and there's something to report, a notice block goes right after the message's tag line, before your text. Lane results never get one. The block is built when the message is actually sent, and each fact is reported once. Its lines:
  - `compaction count=<n>`: the conversation was compacted `n` times since the last notice.
  - `nudge owner_turns=<n>`: the lead has handled `n` of your messages since it last saved memory. It appears when `n` is 10, 20, 30 and so on. It isn't added to a follow-up folded into a running answer.
  - `soul path=<path> writer=<w> commit=<id>`: `system/persona.md` or `system/identity.md` was changed by someone other than the lead.
  - `changed kind=<added|updated|removed> path=<path> writer=<w> commit=<id>`: a memory file changed by someone other than the lead, one line per path in path order, at most 20, then `changed more=<n>`.
  - `expired path=<path>`: a file's `expires` time passed since the last notice, at most 20, then `expired more=<n>`.

  Paths are written as JSON strings. An example:

```text
[moraebot conversation=<key> message=<id>]
[moraebot memory-notice]
compaction count=1
changed kind=updated path="notes/plan.md" writer=human commit=7
expired path="notes/trip.md"
[moraebot memory-notice end]
your message text
```

- **Lead writes aren't announced.** The lead already sees its own writes in the tool results, so they never show up as `changed` or `soul` lines. `moraebot memory accept` can't run while the gateway runs, so for now there's no other writer and those two lines stay empty in normal use. Later slices add other writers.
- **`soul_edit`.** When a lead write changes `system/persona.md` or `system/identity.md`, its result JSON carries `"soul_edit": true`, and the lead is told to tell you about the change. Other writes' results are unchanged.
- **Boundaries.** `system/boundaries.md` is your list of things the lead must never do. Every change to it needs your approval reply: creating, editing, inserting a line, changing the description, deleting, and renaming to or from that path. The lead has to write with `source: user`, and the claim has to pass every check above as `user`. A partial quote (`quoted`), any other source, or no source is refused with `E_MEMORY_QUOTE`, and nothing changes. The file also counts as a soul file, so a lead write to it carries `"soul_edit": true` and another writer's change shows as a `soul` notice line. Your own hand edit followed by `moraebot memory accept` still works.
- **Agreements.** Agreements are lasting rules the lead keeps for you. They don't live in a file. They're rows in the inbox database, in the tables `memory_agreements` and `memory_agreement_breaks`. The lead gets two tools for them:
  - `memory_agreement_record` saves one rule with a `scope` (`global` or `project`), the rule `text` and a `reason`. It takes the same optional `source`, `user_quote` and `quote_message` as the write tools.
  - `memory_agreement_break` reports that the lead broke rule `#id`, with a `reason`.

  Both work only in a turn that answers one of your messages (`E_MEMORY_ORIGIN` otherwise). A rule is one line of at most 240 characters. Secret-like text and memory block markers are refused. A rule is `human` only when the lead records it with `source: user` and the claim passes as `user`. With no source, `agent` or `inferred`, it's `agent`. A `source: user` claim that fails isn't downgraded; it's refused with `E_MEMORY_AGREEMENT`. A rule that differs from an existing one only in letter case or spacing is refused with `E_MEMORY_AGREEMENT_EXISTS` and its number. The ledger holds at most 64 rules and 12 KiB of rule text in total (`MAX_AGREEMENT_BYTES`), and a full ledger refuses new rules with `E_MEMORY_AGREEMENT_FULL`. A break counts once per message of yours: reporting the same rule again in the same turn is refused with `E_MEMORY_AGREEMENT_REPEAT` and the count stays. An unknown number gives `E_MEMORY_AGREEMENT_UNKNOWN`.
- **NEEDS-HUMAN.** At 3 breaks a rule is marked `NEEDS-HUMAN:`. The count keeps going up after that. The next message of yours that reaches the lead carries one notice line per newly marked rule, once only, even across restarts:

```text
agreement needs-human id=3 broken=3 text="never force push"
```

  The lead is told not to break such a rule again, and to stop and ask you if a task can only be done by breaking it.
- **Agreements in the block.** When there's at least one rule, the block has an `<agreements>` element right after `<self>`: a short header line, then one line per rule, yours (`human`) first, then by number:

```text
- #1 [global] [human] never force push
- #3 [project] [agent] [broken x3] NEEDS-HUMAN: run the tests before reporting done
```

  A new rule or a new count doesn't restart the lead. It shows in the block after the next compaction or `moraebot memory refresh`. Until then the lead knows from its tool results and the notice line. With no rules the block is the same as before.
- **The guard.** The database itself refuses to lower a break count, clear `NEEDS-HUMAN`, clear the noticed mark, change a rule's scope, author or text, delete a rule, or change or delete a break record. `system/agreements.md` is reserved: creating, editing, or renaming to or from it with the memory tools is refused with `E_MEMORY_AGREEMENT`.
- **Known gap until M17.** Two things are missing on purpose for now:
  1. There's no owner command to list, acknowledge or drop agreements yet. The only way for you to fix the ledger is to stop moraebot, open the inbox database (`~/.moraebot/inbox.sqlite`) with `sqlite3`, drop the guard triggers (`memory_agreements_guard`, `memory_agreements_no_delete`, `memory_agreement_breaks_no_update`, `memory_agreement_breaks_no_delete`), edit the rows, and start moraebot again. The triggers come back on the next start.
  2. The lead can fill the ledger up to the cap (64 agreements, 12 KiB of text in total) without your approval. The agreements element takes its block room before the memory files. The 12 KiB cap counts raw text only, so the block also stops listing rules once the element reaches about 16 KiB and shows an `omitted` budget marker instead. Persona and human keep their room.

**Size budget.** moraebot estimates how big your always-on memory is: the bytes of every recorded `system/` file that hasn't expired (files shown only as an error marker count too), plus the `<agreements>` element, divided by 4. The budget is 30000 estimated tokens, so about 120000 bytes. At 80% of it (24000 tokens, 96000 bytes) the block's `<memory_metadata>` element ends with one extra line, right before its closing tag:

```text
<pressure tokens="24000" budget="30000" percent="80" code="E_MEMORY_PRESSURE">...</pressure>
```

The numbers are machine-readable, and the text after them asks the lead to make the files shorter or move details into `notes/`. Below 80% the block is unchanged. The line's bytes count against the block's 64 KiB, so it never pushes the block past its limit. Once the estimate reaches the budget, the next message of yours that reaches the lead carries one notice line, `pressure over_budget tokens=<t> budget=<b> percent=<p>`, after any `agreement` and `onboarding` lines, and the log gets one warning with `code=E_MEMORY_PRESSURE`, the token estimate and the budget, but no file text or path. That happens once per crossing: not again while memory stays over budget, and again only after it dropped below and went over once more. A lead restart doesn't repeat it, but a restart of moraebot itself does if memory is still over. Nothing is sent to you in the messenger. One catch: the block itself is cut at 64 KiB, which is only about 16400 tokens, so by the time the 80% line appears some `system/` files are already left out of the block as `omitted`.

**People and patches.** Facts about people other than you go in `people/<slug>/`, with two files. `card.md` says who the person is: its frontmatter has `kind: person`, and every non-blank body line starts with `IDENTITY: `, `ATTRIBUTE: `, `RELATIONSHIP: <relation>: <other slug>` or `INSTRUCTION: `. `observations.md` holds dated facts under one `## Explicit` heading, one per line, like `- [2026-11-18] likes tea <!-- n=1 -->`. Each file takes at most 40 entries, and each line at most 200 characters. A slug uses only lowercase ASCII letters, digits and `-`, starts with a letter or digit, and is at most 40 characters long, so a name in another script goes in the card, not the slug. A write that leaves either file in a wrong shape is refused with `E_MEMORY_PERSON`, and so is any write that would put new text under `people/human/`. Your own card stays `system/human.md`, and it isn't format-checked. Other file names under `people/` stay free-form. The lead gets two more tools. `memory_observe` adds one fact about a person: if the same fact is already listed (letter case and spacing don't count), it bumps that line's `n=` and sets its date to today (UTC) instead of adding a line, and it creates the card and the observations file when they're missing. Its result JSON carries `"n"`. With the slug `human` it writes to the `## Explicit` section of `system/human.md`. `memory_apply_patch` takes a patch in the Codex format (`*** Begin Patch` ... `*** End Patch`) that can add, update, move and delete several files at once, up to 64 KiB and 20 files. A hunk must match exactly, or else match with trailing spaces ignored. It's all or nothing: each file goes through every rule its single-file tool has, and the first refusal changes no file and records nothing. A patch that can't be read or applied is refused with `E_MEMORY_PATCH`, naming only the file, the patch line or the hunk number, never their text. An accepted patch is one commit. `moraebot memory accept` doesn't check the person format, so your hand edits always go through. Memory skills aren't supported yet: a test run showed omp 18.22.0 does load a skill folder listed under `skills.customDirectories` in the overlay file, but moraebot doesn't use that yet, and a folder under `skills/` is still just notes.

**omp's own memory is off.** omp has a memory feature of its own. moraebot turns it off for every omp it starts (the lead and every lane): before each start it writes `~/.moraebot/omp/config-overlay.yml` (mode 600) with exactly this text, and points omp at it with the `PI_CONFIG_FILES` environment variable:

```yaml
memory:
  backend: "off"
autolearn:
  enabled: false
```

If your environment already sets `PI_CONFIG_FILES`, moraebot replaces it for its omp children. Your own `~/.omp` folder and the work folder get no file, so omp you run yourself is unchanged.

## First conversation

After `moraebot init` writes a new config, the bot starts with a short first meeting. It asks what to call it, how it should talk to you, and what it should know about you.

- **The greeting.** At the next start, moraebot finds `~/.moraebot/onboarding.request`, removes it, and posts one fixed hello in the configured channel (`channel_id`). The lead doesn't write it. It's sent once: a restart doesn't repeat it, and a greeting that fails to send isn't tried again. If the request file can't be removed, no greeting is sent and the log gets `E_ONBOARDING_REQUEST`.
- **The questions.** Reply in the channel (a thread opens as usual) or in a DM. While the first meeting runs, each of your messages carries a notice line `onboarding turn=<n>` for the lead. The lead says hello at turn 1, then asks one question per message.
- **Where answers go.** The lead saves each answer with the normal memory tools, quoting your exact words: the name as one line `Name: <name>` in `system/identity.md`, the way of talking in `system/persona.md`, and facts about you in `system/human.md`. Because it quotes you, the file's `provenance:` line starts with `quoted:`. It tells you what it changed.
- **Skip.** Reply `skip` and the lead stops asking.
- **Ending.** When the answers are saved, or you skipped, the lead calls `onboarding_finish` with `done` or `skipped`. Outside a running first meeting that's refused with `E_ONBOARDING_STATE`. `done` also leaves the memory refresh request, so the lead re-pins its memory before your next message, and the new name and tone show from then on. `skipped` changes nothing.
- **The 10-turn limit.** The notice line stops after your 10th message, even if the lead never ended the meeting. It can still end it later.
- **An existing home.** A home you already use has no request file, so nothing happens after an upgrade. To get the first meeting anyway, run `moraebot onboarding` once and restart moraebot. It prints `{"onboarding":"requested"}`. Running it again later starts a fresh meeting.

The state lives in one row of the `onboarding` table in `~/.moraebot/inbox.sqlite`: `active`, `done` or `skipped`.

## Behavior hints

moraebot gives omp a short built-in note about the messenger it's answering in, for example how long a reply may be and which formatting renders. To replace it, write your own text to:

```text
~/.moraebot/hints/discord.md
~/.moraebot/hints/slack.md
```

- The file for the active messenger replaces the built-in text.
- It can be at most 8 KiB. A bigger file fails `check-config`.
- An empty file turns the hints off.
- Restart moraebot to apply a change.

The built-in hints also tell omp how files work here: where received files are saved and how to send one back. An override file replaces all of that text, so keep those file sentences in your own file if you write one.

The lead also gets a built-in role note: how to read the tag line at the start of each input, when to answer directly and when to start a lane, and how to use the lane tools. To replace it, write your own text to `~/.moraebot/hints/lead.md`. The same rules apply (8 KiB at most, empty turns it off, restart to apply). If you write one, keep the lane tool names (`lane_start`, `lane_message`, `lane_status`), the description of the `done` field, and the description of the tag line and its lane result tags (`done`, `failed code=E_DONE_UNMET`, `result`), or the lead won't know how to use lanes, what a lane result means, or where a message came from. The messenger hints file still applies on top of it.

## Logs

Logs live in `~/.moraebot/logs/`. moraebot writes `moraebot.log` files there, plus the stdout and stderr files that launchd captures. Message text is never written to the logs.

Warnings and errors from the libraries moraebot uses (for Discord, Slack and the network) appear in the log only as a short line naming the library (libraries that write through the common `log` interface, such as the network library, are named `log`), without their details, so message text, file names, file links and tokens never reach the log.

```sh
ls ~/.moraebot/logs/
tail -f ~/.moraebot/logs/moraebot.log*
```

## First-use checklist

After installing, check these by hand once:

- [ ] **Real round trip.** Send a short message in the bot's channel. A reply comes back.
- [ ] **Strangers get nothing.** Send a message from a second Discord account. The bot doesn't reply and doesn't store it.
- [ ] **Restart notice.** Send something long, then restart moraebot mid-task (`launchctl kickstart -k gui/$(id -u)/dev.moraebot.gateway`). After it comes back you get one notice saying the task was interrupted.
- [ ] **Send a file.** Attach a small file to a message. omp's answer shows it read the file.
- [ ] **Get a file back.** Ask omp to send you a file from the work folder. It shows up in the same thread, after the reply text.
- [ ] **Lane result in the same thread.** In a thread, ask for something long, such as a task with several steps. The lead says it started a lane, and later the lane's result is answered in that same thread.
- [ ] **A lane claims done, a failing Done check is refused, and the lead reports the outcome.** Ask for long work whose Done line can't pass at first, for example a file the lane still has to create. The lane's first claim is refused, it keeps working, and the lead's answer in that thread says whether the lane's checks passed.
- [ ] **Follow-up while it works.** While the lead is still answering in a thread, send a second message in that thread. You get one reply that covers both.
- [ ] **Memory.** After a hand edit of a memory file, the lead's block shows it as dirty until `moraebot memory accept`; after accept and a restart the edit is in the block.
- [ ] **Memory claims.** Answer a lead proposal with `ok`; when the lead saves it with `source: user`, the file's `provenance:` line starts with `user:`.
- [ ] **Memory guards.** A file with `read_only: "true"` is refused when the lead tries to change it; a note outside `system/` shows by name at the end of the lead's block.
- [ ] **Memory re-pin.** After a long conversation, check the log for `omp lead re-pinned by restart` and its `restart_ms`; run `moraebot memory refresh` once and see the next reply arrive normally.
- [ ] **Agreements.** Tell the lead a lasting rule, approve its proposal, and check that the reply's tool result shows `author` `human`; after `moraebot memory refresh`, the next reply's block lists it under `<agreements>`.
- [ ] **First conversation.** After the first start, reply to the bot's greeting (or reply skip).
- [ ] **omp works under launchd.** launchd starts moraebot with a minimal environment, not your shell's. After your first message, check the logs and confirm omp reached its model. If it can't find its credentials or tools, fix that before relying on the bot.

If you use Slack, also check these:

- [ ] **Invite the bot.** In the bot's channel, run `/invite @<bot name>`. Without it the bot sees nothing there.
- [ ] **Channel post.** Post in the channel. The reply arrives in a thread under your message.
- [ ] **Thread reply.** Reply in that thread. The answer stays in the same thread and the same conversation.
- [ ] **DM.** Send the bot a direct message. A reply comes back in the DM.
- [ ] **Strangers get nothing.** Post from a second Slack account. The bot doesn't reply and doesn't store it.
- [ ] **Reconnect (optional).** For a quick test, add `&debug_reconnects=true` to the end of the Socket Mode `wss` URL (for example in a debug build). Slack then drops the connection on purpose, and the logs show moraebot reconnecting within 360 seconds.

## Files

You can attach files to a message, and omp can send files back to you.

**Files you send.** When a message from you has attachments, moraebot first stores the message, then downloads each file into the work folder:

```text
<work_dir>/.moraebot-inbox/<conversation>/<message>/<file name>
```

The file name is cleaned up first, so it can't point outside that folder. moraebot then hands omp your text plus a short list of the saved paths. A message with only files and no text works too.

- At most 10 files per message. A message with more gets one notice and isn't run.
- moraebot sets no size limit. The messenger's own upload limit still applies.
- Received files are written to disk in small pieces, so a large download doesn't need much memory.
- While a file downloads, your other incoming messages wait. They wait for as long as the file keeps arriving. A download fails only after 2 minutes with no data at all, so a large, slow file holds later messages until it's done.
- If any file of a message can't be received (a dropped download, a full disk, a missing permission), the message isn't run and you get one notice. Its folder under `.moraebot-inbox` can be deleted by hand.
- `.moraebot-inbox` is never cleaned up automatically. Delete old folders yourself when you no longer need them.
- If the work folder is a git repository, add `.moraebot-inbox/` to its `.gitignore` so received files don't end up in a commit.

**Files omp sends back.** omp has a `send_file` tool. When it calls it with a path, the file is sent to the same thread after the reply text.

- Only files inside the work folder can be sent, and never anything inside the moraebot home. If `work_dir` is set somewhere under the moraebot home, every `send_file` is refused.
- A file to send is read whole into memory and sent in one go, the way other chat bots do it. Sending it uses extra memory about equal to its size on Slack, and about twice its size on Discord, because the Discord library keeps a second copy while it sends.
- Later incoming messages also wait while files are sent.
- One file's upload may take up to 30 minutes. After that it's skipped with one notice and not tried again.
- Other short failures, such as a dropped connection, are retried at the next reconnect. Each file gets at most 3 attempts in total, restarts included. After that it's skipped with one notice. A file can still arrive after that notice, if a later attempt went through.
- If a stop or crash (running out of memory included) cut an upload short, that file is skipped with one notice instead of being sent twice.
- If the messenger refuses a file, for example because it's over the upload limit, the file is skipped with one notice.

**Privacy.** The log never holds file contents, file names or tokens. Detailed logs from other libraries are always off; only their warnings and errors are kept, as described under Logs.

**Memory files.** moraebot keeps its own files in its home too:

```text
~/.moraebot/memory/system/persona.md
~/.moraebot/memory/system/human.md
~/.moraebot/omp/config-overlay.yml
```

The first two are the lead's memory (see Memory). The last one turns omp's own memory off and is rewritten whenever its text differs. `~/.moraebot/memory/.refresh` may also show up for a moment: it's the request file `moraebot memory refresh` leaves, removed at the next lead start.

**First conversation files.** `~/.moraebot/onboarding.request` is the request `moraebot init` or `moraebot onboarding` leaves; the next start removes it and greets. The first meeting's state is one row in the `onboarding` table of `~/.moraebot/inbox.sqlite`.

**Missing permission.** If the bot lacks the file permission, every file you send or ask for shows up as a file notice. Do the upgrade steps below to fix it.

## Upgrading from an earlier version

Re-run the install line from Install, then `moraebot install-launchd` if you use the background service:

```sh
moraebot install-launchd
```

Run `install-launchd` from a terminal, not by asking the bot. It restarts the bot's own process.

Files need one more permission than earlier versions asked for. If you set up the bot before this version:

- Discord: invite the bot again with the invite link from [docs/guides/discord.md](docs/guides/discord.md), or give its role the Attach Files permission in the channel.
- Slack: update the app from the manifest in [docs/guides/slack.md](docs/guides/slack.md), then reinstall it to your workspace.

The guides hold the exact settings. Restart moraebot afterwards.

This version replaces the separate omp per thread or DM with one lead (see Lead and lanes). After the upgrade:

- The lead starts with an empty history. It doesn't pick up what earlier per-thread sessions remembered.
- The old `~/.moraebot/conversations/*/session` folders are kept as they were and are no longer used. Delete them by hand if you don't need them.
- Your stored messages and settings keep working without any change.

## Stop and uninstall

```sh
moraebot uninstall-launchd
```

This unloads the LaunchAgent and removes its plist. To remove everything else too:

```sh
rm -rf ~/.moraebot
rm ~/.local/bin/moraebot
```

That deletes the installed binary, the config, the token file, the message database, and the logs. Your work folder is left alone. Finally, delete the Discord application or the Slack app if you no longer need the bot.
