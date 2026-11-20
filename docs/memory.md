# Memory

moraebot keeps a small memory of what you, the owner, asked it to remember. This page covers what the first part of it (issue #39, P1a) does: the store, the commands you type in plain words, and how the lead sees your core rules.

## Where it lives

Everything is under `~/.moraebot/memory/` (the moraebot home's `memory/` folder, mode 0700). It is a git repository, so every change is one commit you can read with `git log`.

```
core/owner.md          rules, preferences and facts you said to always keep
core/boundaries.md     things you said never to do
projects/<repo>/notes.md
projects/_general.md
threads/<conversation>.md
archive/...            forgotten or replaced entries, same paths as above
```

Each entry is one line with an id such as `m-1a2b3c`, the day it was saved, your words as you wrote them, and where they came from (the inbox row). Line breaks are folded to spaces, and keys or passwords moraebot recognizes (including its own messenger tokens) are saved as `[redacted]` markers.

Only one moraebot can use the store at a time. If moraebot stopped in the middle of a write, the next start commits the files that still read correctly and puts back the ones that don't.

## Commands

Only messages from you (the owner) are checked, and only ones without attachments. A message that is a memory command is answered by moraebot itself and never reaches the lead, even while the lead is busy in that conversation. Lane results, chat senders and anything else are never treated as commands.

| You write | What happens |
| --- | --- |
| `기억해: X`, `X 기억해줘`, `remember: X`, `remember that X` | Saves X for this conversation. |
| `기억해(<repo>): X` | Saves X for that project. |
| `항상 기억해: X`, `always remember: X` | Saves X as a core rule. If X says not to do something ("하지 마", "never", "don't"), it goes to the boundaries. |
| `뭐 기억해?`, `what do you remember?` | Lists the core rules, the boundaries and this conversation's entries. |
| `m-1a2b3c 보여줘` | Shows one entry. |
| `m-1a2b3c 고쳐: new text` | Saves your new text and archives the old entry as replaced. |
| `m-1a2b3c 지워`, `forget m-1a2b3c` | Asks once; your next message there answers it (`응`/`yes` forgets, `아니`/`no` keeps). |
| `m-1a2b3c 되살려`, `restore m-1a2b3c` | Brings a forgotten entry back. |

A sentence that only mentions an id, or a question such as `기억해?`, goes to the lead as usual. moraebot sends no notice when it saves something on its own (nothing does yet in P1a).

The lead has no tool that changes memory.

## What the lead sees

Before every lead turn moraebot reads `core/owner.md` and `core/boundaries.md` and appends them to the lead's system prompt, after the persona, between `<<<moraebot-memory core>>>` and `<<<end>>>`. The block stays within 6 KiB; when it doesn't fit, the newest entries are kept and a line says how many older ones were left out. Expired entries are skipped.

When the core changes (always-remember, correct, forget, restore), the lead restarts before its next turn, the same way it does when `persona.md` changes. That is why a plain `기억해` goes to the conversation, not the core.

Conversation and project entries are saved but not shown to the lead yet; that comes in the next part of #39.

## Coming from 0.2.0

The 0.2.0 memory used the same folder with a different layout. moraebot does not delete or convert it. If it finds `memory/system`, `memory/reference`, `memory/notes`, `memory/people`, `memory/skills` or `omp/config-overlay.yml` in the home, it refuses to start and prints one line ending in the `rm -rf ...` command for exactly those paths. Check them, run it, and start again. The 0.2.0 memory tables in `inbox.sqlite` are not used.

## Removing something from history for good

Forget only moves an entry to `archive/`; its text is still in the git history (and your original message is still in `inbox.sqlite`). To erase it completely, by hand:

1. Stop moraebot (`systemctl --user stop moraebot`, or however you run it).
2. Install [git-filter-repo](https://github.com/newren/git-filter-repo) if you don't have it.
3. Write the exact text to remove to a file, one line per text, for example `/tmp/purge.txt` containing `the secret sentence==>[removed]`.
4. Run, in the memory folder:

   ```sh
   cd ~/.moraebot/memory
   git filter-repo --force --replace-text /tmp/purge.txt
   git reflog expire --expire=now --all
   git gc --prune=now --aggressive
   ```

5. Check with `git log -p | grep -F 'the secret sentence'` (no output), then delete `/tmp/purge.txt`.
6. Start moraebot again.

This can't be undone. If a commit message held the text, `--replace-text` rewrites it too.
