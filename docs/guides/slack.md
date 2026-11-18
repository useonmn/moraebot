# Slack setup guide

This guide connects moraebot to a Slack workspace you control. It's the only place the fixed Slack settings are written down. The README and `moraebot guide slack` point here.

Click paths in Slack's settings pages change from time to time. Where a path isn't confirmed in Slack's own docs, it's marked "(click path may differ)".

## 1. What moraebot needs and why

moraebot talks to Slack through **Socket Mode**. It opens a connection out to Slack and keeps it open. There's no public server, no request URL and no open port on your Mac.

Bot scopes:

| Scope | Why |
| --- | --- |
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

### Upgrading from an earlier version

Earlier versions didn't ask for `files:read` and `files:write`, so files you attach can't be downloaded and files the agent sends back can't be uploaded. Update your app's manifest to match [`slack-manifest.json`](slack-manifest.json), then reinstall the app as described above. Your tokens and channel stay the same.

## 8. Fixed settings

The agent and the README both check against this list. Anything else is a mistake.

- [ ] App created from `slack-manifest.json`, unchanged.
- [ ] Socket Mode on. No request URL anywhere.
- [ ] Bot scopes are exactly `channels:history`, `chat:write`, `files:read`, `files:write`, `groups:history`, `im:history`.
- [ ] Bot events are exactly `message.channels`, `message.groups`, `message.im`.
- [ ] Messages tab on, and users can send messages to the app.
- [ ] Token rotation off.
- [ ] App-level token has only `connections:write`.
- [ ] Bot invited to the one channel moraebot should answer in.
- [ ] Tokens saved only in `<home>/secrets/slack-bot.token` and `<home>/secrets/slack-app.token`, mode `600`.

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

Restart the gateway last:

```sh
launchctl kickstart -k gui/$(id -u)/dev.moraebot.gateway
```

On Linux:

```sh
systemctl --user restart moraebot.service
```

A restart interrupts any task that's running, so the agent warns you first.

Every step needs evidence, such as command output or the test reply in Slack. A step that only "ran" isn't done.
