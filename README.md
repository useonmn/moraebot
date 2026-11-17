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
- **File format.** Every file starts with a frontmatter block: a `---` line, `key: value` lines, and a closing `---` line. `description` is required (one line, at most 1024 characters). Other allowed keys are `read_only`, `limit`, `kind`, `aliases`, `provenance` and `expires`; they're read and kept, but don't change anything yet.
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

- **What M2 doesn't do.** No recall, no automatic learning, and lanes still get no memory. Later slices of the memory roadmap add those.

**omp's own memory is off.** omp has a memory feature of its own. moraebot turns it off for every omp it starts (the lead and every lane): before each start it writes `~/.moraebot/omp/config-overlay.yml` (mode 600) with exactly this text, and points omp at it with the `PI_CONFIG_FILES` environment variable:

```yaml
memory:
  backend: "off"
autolearn:
  enabled: false
```

If your environment already sets `PI_CONFIG_FILES`, moraebot replaces it for its omp children. Your own `~/.omp` folder and the work folder get no file, so omp you run yourself is unchanged.

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

The first two are the lead's memory (see Memory). The last one turns omp's own memory off and is rewritten whenever its text differs.

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
