# moraebot

moraebot lets you talk to a coding agent on your Mac from a chat app. You send a message in Discord, moraebot hands it to omp, a coding agent CLI, running on your Mac, and the reply comes back in the same conversation.

## Status

This is slice 1, the smallest version that works end to end.

- Discord only. Other messengers come later.
- macOS only. It runs as a per-user LaunchAgent.
- One owner. Only you can talk to the bot (`access_mode = "owner_only"`).
- One task at a time. Messages wait in a queue and run in order.

## Read this first: yolo mode

> **Warning**
>
> moraebot runs omp without asking for approval. Anyone who can talk to the bot can run commands on this Mac, with your user's permissions.
>
> - Keep `access_mode = "owner_only"`. It's the default and the only mode in slice 1.
> - Use a private channel that only you can see.
> - Turn on two-factor login for your Discord account. That account is now a key to your Mac.
> - The work folder is not a sandbox. omp can read and change files outside it.
> - Never share the bot token. Anyone holding it can pretend to be your bot.

The bot only runs while you're logged in to macOS and the Mac is awake.

## Install

You'll need macOS, a Discord account, and omp already installed and working in your terminal.

### 1. Install Rust

```sh
brew install rust
```

Or use rustup if you prefer:

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

### 2. Build moraebot

```sh
git clone https://github.com/<YOUR_GITHUB_USER>/moraebot.git
cd moraebot
cargo build --release
```

The binary ends up at `target/release/moraebot`. The commands below call it `moraebot`. Either run it by that path or copy it somewhere on your `PATH`.

### 3. Create the Discord application and bot

Click paths in the Discord Developer Portal are given as a guide (screen names may change).

1. Open <https://discord.com/developers/applications> and click **New Application**. Give it a name.
2. On the **General Information** page, copy the **Application ID**. You'll need it for the invite link.
3. Open the **Bot** page. Click **Reset Token** and copy the token. Discord shows it only once. Keep it somewhere safe for step 6.
4. On the same **Bot** page, under **Privileged Gateway Intents**, turn on **Message Content Intent** and save. Without it the bot sees empty messages.

### 4. Invite the bot to your server

Put your Application ID into this link and open it in a browser:

```text
https://discord.com/oauth2/authorize?client_id=<APPLICATION_ID>&scope=bot&permissions=309237713920
```

`309237713920` grants exactly these permissions: View Channels, Send Messages, Read Message History, Create Public Threads, and Send Messages in Threads. Pick your server and confirm.

Then create a private channel for the bot (or reuse one only you can see) and make sure the bot can see it.

### 5. Copy your user id and the channel id

Discord ids are long numbers. To copy them, turn on Developer Mode first (screen names may change):

1. In the Discord app, open **User Settings > Advanced** and turn on **Developer Mode**.
2. Right-click your own name and choose **Copy User ID**. That's your owner id.
3. Right-click the bot's channel and choose **Copy Channel ID**.

### 6. Run `moraebot init`

```sh
moraebot init
```

It asks a few questions, in order:

1. Which messenger to use. Only Discord is available in slice 1.
2. Your owner user id, the channel id, and the bot token. The token isn't echoed while you type or paste it. It's saved to `~/.moraebot/secrets/discord.token` with mode `600` (readable only by you).
3. The path to `omp` (found on your `PATH`, you confirm it) and the work folder. Press Enter to accept the default `~/moraebot-work`.

Then it creates `~/.moraebot` (mode `700`), writes `~/.moraebot/config.toml`, and runs the same checks as `check-config`. If a config already exists, it asks before overwriting it. The default answer is No.

The written config looks roughly like this. You can edit it by hand later.

```toml
# Your Discord user id. Only this account can talk to the bot.
owner_id = <YOUR_DISCORD_USER_ID>
# File holding the bot token. Must be mode 600.
token_file = "~/.moraebot/secrets/discord.token"
# Folder omp starts in. This is not a sandbox.
work_dir = "~/moraebot-work"
# Absolute path to omp.
omp_path = "<PATH_TO_OMP>"
# Only owner_only exists in slice 1.
access_mode = "owner_only"

[discord]
# The one channel the bot listens in. Threads under it and your DMs also work.
channel_id = <YOUR_CHANNEL_ID>

[omp]
# Optional. No time limit when left out.
# turn_timeout_minutes = 30
# Optional. No cost limit when left out.
# max_cost_usd = 5.0
```

Set `MORAEBOT_HOME` if you want moraebot's home somewhere other than `~/.moraebot`.

### 7. Check the config

```sh
moraebot check-config
```

Every line should start with `OK`. A `FAIL` line says what to fix, for example a token file that other users can read (`chmod 600 ~/.moraebot/secrets/discord.token`). The command exits non-zero if anything fails. Use `--config <path>` to check a different file.

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

## Logs

Logs live in `~/.moraebot/logs/`. moraebot writes `moraebot.log` files there, plus the stdout and stderr files that launchd captures. Message text is never written to the logs.

```sh
ls ~/.moraebot/logs/
tail -f ~/.moraebot/logs/moraebot.log*
```

## First-use checklist

After installing, check these by hand once:

- [ ] **Real round trip.** Send a short message in the bot's channel. A reply comes back.
- [ ] **Strangers get nothing.** Send a message from a second Discord account. The bot doesn't reply and doesn't store it.
- [ ] **Restart notice.** Send something long, then restart moraebot mid-task (`launchctl kickstart -k gui/$(id -u)/dev.moraebot.gateway`). After it comes back you get one notice saying the task was interrupted.
- [ ] **omp works under launchd.** launchd starts moraebot with a minimal environment, not your shell's. After your first message, check the logs and confirm omp reached its model. If it can't find its credentials or tools, fix that before relying on the bot.

## Stop and uninstall

```sh
moraebot uninstall-launchd
```

This unloads the LaunchAgent and removes its plist. To remove everything else too:

```sh
rm -rf ~/.moraebot
```

That deletes the config, the token file, the message database, and the logs. Your work folder is left alone. Finally, delete the application in the Discord Developer Portal if you no longer need the bot.
