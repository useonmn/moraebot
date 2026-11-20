# Slack setup guide

This guide connects moraebot to a Slack workspace you control. It's the only place the fixed Slack settings are written down. The README and `moraebot guide slack` point here.

Click paths in Slack's settings pages change from time to time. Where a path isn't confirmed in Slack's own docs, it's marked "(click path may differ)".

## 1. What moraebot needs and why

moraebot talks to Slack through **Socket Mode**. It opens a connection out to Slack and keeps it open. There's no public server, no request URL and no open port on your machine.

The Slack connection lives in its own process, `moraebot-slack-bridge`, which runs as a second service next to `moraebot`. The bridge holds the Slack tokens and the channel id in `<home>/slack-bridge.toml`. The core keeps the owner and the access list in `config.toml` and talks to the bridge over a local socket. Either one can restart while the other keeps running. Messages that arrive while the core is down wait on disk in the bridge. [`../bridge.md`](../bridge.md) has the details.

Bot scopes:

| Scope | Why |
| --- | --- |
| `assistant:write` | Run as a Slack agent: the agent entry point at the top of Slack and the "thinking" status in a DM thread. See section 7. |
| `chat:write` | Post replies as the bot. |
| `channels:history` | Read messages in public channels the bot was added to. Needed for `message.channels`. |
| `groups:history` | Same for private channels. Needed for `message.groups`. |
| `im:history` | Read your DMs to the bot. Needed for `message.im`. |
| `files:read` | Download files you attach. |
| `files:write` | Upload files the agent sends back. |

Events:

| Event | Why |
| --- | --- |
| `message.channels` | Every message in a public channel the bot is in, thread replies included. |
| `message.groups` | Same for private channels. |
| `message.im` | Your DMs to the bot. Slack doesn't send DMs any other way. |

Left out on purpose:

- `app_mention` and `app_mentions:read`. The bot already gets every message in its channel, so mentions add nothing. Fewer permissions is safer.
- `im:write` and `chat:write.public`. moraebot only answers inside conversations you start, in channels it was invited to.
- Interactivity, slash commands and token rotation.

moraebot answers only your member id. Messages from anyone else, from bots, and from other channels are dropped. Slack doesn't enforce this; moraebot does.

## 2. Create the app

Pick one:

- Run `moraebot setup slack`. It prints a link that opens Slack's app creation page with the manifest already filled in.
- Or open <https://api.slack.com/apps?new_app=1>, choose to create the app **from a manifest**, pick your workspace, paste the contents of [`slack-manifest.json`](slack-manifest.json), click **Next**, check the summary and click **Create**.

## 3. Install the app and copy the bot token

1. In the app settings, open **Install App** and click **Install to Workspace**, then **Allow** (click path may differ).
2. Open **OAuth & Permissions** and copy the **Bot User OAuth Token**. It starts with `xoxb-`.

Don't paste it into chat. `moraebot setup slack` asks for it with a hidden prompt.

## 4. Create the app-level token

1. Open **Basic Information** and scroll to **App-Level Tokens**.
2. Click **Generate Token and Scopes**, give it any name, add the scope `connections:write`, and generate it.
3. Copy the token. It starts with `xapp-`.

Leave token rotation off. Rotating tokens need refresh handling that moraebot doesn't do.

## 5. Copy your member id and the channel id

- **Member id** (starts with `U`): open your own profile, click the three-dot menu, choose **Copy member ID** (click path may differ).
- **Channel id** (starts with `C`): click the channel name at the top, open **About**, and copy the id at the bottom (click path may differ).

## 6. Invite the bot to the channel

In the channel, type:

```text
/invite @moraebot
```

The bot only sees messages in channels it was added to. DMs work without this step.

## 7. Changing scopes later

If you change scopes or events in the manifest, reinstall the app to the workspace. Slack applies new scopes to an existing install only after a reinstall.

### Agents

The manifest turns on Slack's agent view (`features.agent_view`) and asks for `assistant:write`. With it, moraebot shows up as an agent at the top of Slack, every DM gets its answer in a thread, and the thread shows a status while moraebot works. Each DM thread is its own conversation, with its own session and memory.

> **This switch can't be undone.** Once an app uses `agent_view`, Slack doesn't let it go back to the older `assistant_view`. Read this before you update an existing app. A new app created from the manifest has nothing to switch back to.

To turn it on for an existing app:

1. Open your app at <https://api.slack.com/apps>, then **App Manifest** (click path may differ).
2. Replace the manifest with [`slack-manifest.json`](slack-manifest.json) and save. You can also flip the **Agents** toggle in the app settings instead (click path may differ).
3. Reinstall the app to the workspace, as above.
4. Hard refresh Slack (reload the desktop app or the browser tab) so the agent entry point appears.

Slack says some agent features need a paid plan, even where the settings page shows them on any plan. Threaded DM replies work on every plan.

### Upgrading from an earlier version

Update your app's manifest to match [`slack-manifest.json`](slack-manifest.json), then reinstall the app as described above. Your tokens and channel stay the same. What changed:

- Earlier versions didn't ask for `files:read` and `files:write`, so files you attach couldn't be downloaded and files the agent sends back couldn't be uploaded.
- Earlier versions had no `assistant:write` and no `agent_view`. Read the warning under **Agents** before you save the new manifest.

A `config.toml` from before the bridge still has a `[slack]` section. Run `moraebot setup slack` once to move it, then the install command it prints. See [README.md](../../README.md#upgrading-from-an-earlier-version).

## 8. Fixed settings

The agent and the README both check against this list. Anything else is a mistake.

- [ ] App created from `slack-manifest.json`, unchanged.
- [ ] Socket Mode on. No request URL anywhere.
- [ ] Bot scopes are exactly `assistant:write`, `channels:history`, `chat:write`, `files:read`, `files:write`, `groups:history`, `im:history`.
- [ ] Agent view (`agent_view`) on.
- [ ] Bot events are exactly `message.channels`, `message.groups`, `message.im`.
- [ ] Messages tab on, and users can send messages to the app.
- [ ] Token rotation off.
- [ ] App-level token has only `connections:write`.
- [ ] Bot invited to the one channel moraebot should answer in.
- [ ] Tokens saved only in `<home>/secrets/slack-bot.token` and `<home>/secrets/slack-app.token`, mode `600`, and named only in `<home>/slack-bridge.toml`. `config.toml` has no `[slack]` section and no token path.

`<home>` is `~/.moraebot`, or `$MORAEBOT_HOME` if you set it.

## Agent-assisted setup (optional)

An agent can do most of the clicking for you in a browser. It runs only when you ask for it.

| Human jobs | Agent jobs |
| --- | --- |
| Sign in to Slack. | Open the app creation page and fill in the manifest. |
| Finish 2FA and any CAPTCHA. | Click through the settings in sections 2 to 6. |
| Approve the workspace install. | Read each token from the page and write it straight to `<home>/secrets/slack-bot.token` or `<home>/secrets/slack-app.token`, mode `600`, inside a `700` folder. |
| Confirm choices like the workspace and the channel. | Run `moraebot setup slack` and press Enter to keep the saved tokens. |
| | Run `moraebot check-config`. |
| | Ask you to post a test message. |

Secret rules for the agent:

- Never print, paste or echo a token. Never pass it as a command argument or an environment variable. Never put it in chat or logs.
- Strip spaces and line breaks around a token before saving it.
- If you paste a token into chat, the agent writes it to the file at once and tells you to regenerate it, because chat isn't private.

Restart both services last, the bridge first:

```sh
launchctl kickstart -k gui/$(id -u)/dev.moraebot.gateway.slack-bridge
launchctl kickstart -k gui/$(id -u)/dev.moraebot.gateway
```

On Linux:

```sh
systemctl --user restart moraebot-slack-bridge.service
systemctl --user restart moraebot.service
```

If the services aren't installed yet, run `moraebot install-launchd` (macOS) or `moraebot install-systemd` (Linux) instead. Each installs both.

Restarting the core interrupts any task that's running, so the agent warns you first. Restarting only the bridge doesn't: messages wait in its spool.

Every step needs evidence, such as command output or the test reply in Slack. A step that only "ran" isn't done.
