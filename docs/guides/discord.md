# Discord setup guide

This guide connects moraebot to a Discord server you control. It's the only place the fixed Discord settings are written down. The README and `moraebot guide discord` point here.

All of this happens in the Discord Developer Portal at <https://discord.com/developers/applications>. Screen labels that aren't confirmed in Discord's docs are marked "(label may differ slightly)".

## 1. Create the application

Click **New Application** and give it a name. On **General Information**, copy the **Application ID**. You need it for the invite link.

## 2. Installation page

- **Installation Contexts**: Guild Install only. Uncheck User Install.
- **Default Install Settings** for Guild Install: scopes `bot` only.
- Permissions: View Channels, Send Messages, Read Message History, Create Public Threads, Send Messages in Threads. As one number that's `309237713920`.

Threads don't inherit Send Messages from their channel, so Send Messages in Threads is needed too.

## 3. Bot page

- **Public Bot**: OFF (label may differ slightly). Only you can add the bot to a server.
- **Requires OAuth2 Code Grant**: OFF (label may differ slightly). With it on, a plain bot invite doesn't work.
- Under **Privileged Gateway Intents**:
  - **Message Content Intent**: ON. Without it the bot sees empty messages.
  - **Presence Intent**: OFF (label may differ slightly).
  - **Server Members Intent**: OFF (label may differ slightly).
- Click **Reset Token** and copy the token. Discord shows it once. Don't paste it into chat; `moraebot setup discord` asks for it with a hidden prompt.

## 4. Invite the bot

Use the portal's Install Link, or put your Application ID into this link:

```text
https://discord.com/oauth2/authorize?client_id=<APPLICATION_ID>&scope=bot&permissions=309237713920
```

Pick your server and confirm. Then create a private channel for the bot, or reuse one only you can see, and give the bot **View Channel** on it. Discord hides channels a bot can't view.

## 5. Copy your user id and the channel id

1. In the Discord app, open **User Settings > Advanced** and turn on **Developer Mode**.
2. Right-click your own name and choose **Copy User ID**.
3. Right-click the bot's channel and choose **Copy Channel ID**.

## 6. A limit to know about

Discord allows 1000 logins (IDENTIFY calls) per bot per 24 hours. Going over resets the bot token. A crash loop that restarts the bot over and over can hit this, so check the logs if the bot keeps restarting.

## 7. Fixed settings

The agent and the README both check against this list.

- [ ] Installation Contexts: Guild Install only.
- [ ] Install scopes: `bot` only.
- [ ] Permissions integer `309237713920`, nothing more.
- [ ] Public Bot OFF, Requires OAuth2 Code Grant OFF.
- [ ] Message Content Intent ON, Presence Intent OFF, Server Members Intent OFF.
- [ ] Bot has View Channel on one private channel.
- [ ] Token saved only in `<home>/secrets/discord.token`, mode `600`.

`<home>` is `~/.moraebot`, or `$MORAEBOT_HOME` if you set it.

## Agent-assisted setup (optional)

An agent can do most of the clicking for you in a browser. It runs only when you ask for it.

| Human jobs | Agent jobs |
| --- | --- |
| Sign in to Discord. | Open the Developer Portal pages. |
| Finish 2FA and any CAPTCHA. | Click through the settings in sections 1 to 4. |
| Approve adding the bot to your server. | Read the token from the page and write it straight to `<home>/secrets/discord.token`, mode `600`, inside a `700` folder. |
| Confirm choices like the server and the channel. | Run `moraebot setup discord` and press Enter to keep the saved token. |
| | Run `moraebot check-config`. |
| | Ask you to post a test message. |

Secret rules for the agent:

- Never print, paste or echo a token. Never pass it as a command argument or an environment variable. Never put it in chat or logs.
- Strip spaces and line breaks around the token before saving it.
- If you paste a token into chat, the agent writes it to the file at once and tells you to regenerate it, because chat isn't private.

Restart the gateway last:

```sh
launchctl kickstart -k gui/$(id -u)/dev.moraebot.gateway
```

A restart interrupts any task that's running, so the agent warns you first.

Every step needs evidence, such as command output or the test reply in Discord. A step that only "ran" isn't done.
