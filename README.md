# Claude Buddy: a desktop pet for Claude Code

[![Latest release](https://img.shields.io/github/v/release/taivt0109/claude-buddy)](https://github.com/taivt0109/claude-buddy/releases/latest)
[![Downloads](https://img.shields.io/github/downloads/taivt0109/claude-buddy/total)](https://github.com/taivt0109/claude-buddy/releases)
[![Platforms](https://img.shields.io/badge/platform-Windows%20%7C%20macOS-blue)](#install)
[![Built with Rust](https://img.shields.io/badge/built%20with-Rust-orange)](https://www.rust-lang.org/)
[![License: MIT](https://img.shields.io/badge/license-MIT-green)](LICENSE)

**English** | [Tiếng Việt](README.vi.md)

A small character that floats on your desktop and shows what your Claude Code sessions are doing: working, waiting for
you, done, or failed. For Windows 10/11 and macOS 11 or later.

Run several Claude Code sessions at once and stop checking each terminal to see if one is waiting for you. Claude Buddy
is a desktop companion and status notifier for [Claude Code](https://docs.anthropic.com/en/docs/claude-code), Anthropic's
AI coding agent: it listens to Claude Code hooks, tells you when a session needs your input or has finished, and takes you
to the right terminal or IDE window (VS Code, Cursor, Windows Terminal and others) in one click. It is a small native Rust
app, not Electron.

[Install](#install) · [Create a character](#creating-a-character-with-codex) · [Share characters](#sharing-characters) ·
[Local API](#local-api) · [Build from source](#build-from-source)

## Features

- **See your sessions at a glance:** the character shows whether Claude is working, needs you, is done, or hit an error.
  Click it to jump to that session's window.
- **Create a character from a description:** Codex draws it with all five states animated, including turning around and
  celebrating when the work is done. No Codex? Use ChatGPT and drop the ZIP on the app.
- **Change it with another prompt:** "add a red hat" redraws it, and Codex remembers what it made.
- **Share characters:** one `.buddy` file, double-click to add.
- **Add your own:** WebM, MP4, PNG, APNG, GIF, WebP, JPG or SVG.
- **Token usage:** today, this week, this month and the last 14 days.
- **Stays out of your way:** hides during full screen, and clicks on it don't take focus from what you're typing in.
- **English and Vietnamese.**

## Install

### Windows

Open PowerShell. No admin rights needed.

```powershell
irm https://raw.githubusercontent.com/taivt0109/claude-buddy/main/install.ps1 | iex
```

This downloads the app, adds a Start Menu shortcut, adds its hooks to Claude Code and starts it. `settings.json` is
backed up first and your other hooks stay as they are. Only Claude Code sessions opened after that show up.

Run the same command again to update. To skip the hooks, set `$env:CLAUDE_BUDDY_NO_HOOKS = "1"` first; the app then
asks when it starts.

The app isn't code-signed, so Windows SmartScreen or antivirus software may warn about it.

### macOS

Open Terminal. Works on Apple silicon and Intel.

```sh
curl -fsSL https://raw.githubusercontent.com/taivt0109/claude-buddy/main/install.sh | sh
```

This installs `Claude Buddy.app` in `~/Applications`, adds its hooks to Claude Code and starts it. `settings.json` is
backed up first and your other hooks stay as they are. Only Claude Code sessions opened after that show up. Run the same
command again to update.

The app isn't signed by Apple, so macOS may refuse to open a copy you downloaded with a browser. The command above
doesn't run into this.

## Updates

An installed app updates itself: it checks the latest release when it starts and every 30 minutes, downloads it, checks its
checksum, and restarts into it as soon as the main window is closed. Claude Code sessions keep running through this, and
each one shows up again at its next event. Turn this off under Settings > Update automatically. Running the install command again also updates. A copy built from source is left
alone.

## Uninstall

### Windows

```powershell
irm https://raw.githubusercontent.com/taivt0109/claude-buddy/main/uninstall.ps1 | iex
```

This removes the app, its shortcut, its autostart entry and its hooks. Settings, usage data and added characters stay
in `%APPDATA%\claude-buddy-rs` unless you set `$env:CLAUDE_BUDDY_PURGE = "1"` first.

### macOS

```sh
curl -fsSL https://raw.githubusercontent.com/taivt0109/claude-buddy/main/uninstall.sh | sh
```

This removes the app, its login item and its hooks. Settings, usage data and added characters stay in
`~/Library/Application Support/claude-buddy-rs`; delete that folder too if you don't want them.

## Using it

| Action | Result |
|---|---|
| Hover over the character | Shows the full status of your sessions |
| Click the character | Opens or closes the session list (closes by itself after the mouse leaves) |
| Click the bubble | Opens the window of the session that most needs attention |
| Click a session row | Opens that session's terminal or IDE window |
| Drag the character | Anywhere on the screen; the position is remembered |
| Scroll on the character (Windows) | Makes it bigger or smaller, up to 60% of the screen |
| Point at the character, then Ctrl+Q / Ctrl+W / Ctrl+R (Windows) | Skills: teleport to another corner / shadow clones in the corners / biggest size and back. Anywhere else, add Alt (Ctrl+Alt+Q…). After a skill, the Ctrl keys stay with the buddy for 4 seconds, so the next one needs no pointing. Other keys: `skillKeys` under Configuration |
| Right-click the character or tray icon | Quick menu |
| Click the tray icon (Windows) | Opens the main window (Characters, Usage, Settings) |
| Click the menu bar icon (macOS) | Quick menu, with the main window under **Manage characters**, **View usage** and **Settings** |
| Click **Share** on a character you added | Saves it as a `.buddy` file |
| Double-click a `.buddy` file, or drag it or a ZIP onto the main window | Adds that character |

Closing the main window keeps the app running in the tray or menu bar. Quit from the right-click menu.

The interface is in English and Vietnamese. It follows the system by default; change it under Settings > Language.

### Differences on macOS

- Opening a session brings its terminal or IDE to the front, but can't choose between several windows of that program.
- There is no see-through window background.
- There is no scrolling to resize, no skills and no `autoSkills` yet.
- Notifications are sent through the system's scripting tool, so macOS may ask for permission or show it as their sender.

## Stays out of your way

- Bubbles show for a few seconds when a status changes, just wide enough for their text. Only sessions that need you or
  failed stay up.
- The tray or menu bar icon has a dot colored by status, so you can check at a glance with nothing on the screen.
- Transparent parts of the buddy let the mouse through, and clicking the buddy doesn't take focus from what you're
  typing in.
- Hides during full-screen presentations, movies and games, then comes back.
- No sound or notification during full screen or Focus Assist on Windows, or when you're already in that session's window
  (on macOS, in its program).
- The character comes in three sizes. All of this can be changed in Settings.

## Usage

Today, this week (from Monday), this month and the last 14 days, in local time. Numbers are counted from the transcripts
in `.claude/projects` in your home folder (subagents included), reading only the `usage` numbers and never the conversation.
The big number is input + output + cache writes; cache reads are shown separately. Claude used on other machines or on
the web isn't included.

## Creating a character with Codex

If [Codex](https://openai.com/codex/) is installed and signed in on this computer, click **Create with Codex** on the Characters
page and describe the character you want. It takes about 6 to 10 minutes and uses your Codex quota. You get all five states
as animations, and you can keep working in the meantime as long as the app's window stays open. Codex only draws: the app
checks and cuts the images itself.

To change it later, select it and click **Edit with Codex…**, then say what should change, such as "add a red hat". Codex
continues the conversation that drew it and redraws all five states. The character only changes once they're all done.

No Codex? Type your description and click **Copy prompt for ChatGPT**, paste it into ChatGPT, then drag the ZIP it gives you
onto the app window.

## Sharing characters

Select a character you added and click **Share** to save it as one `.buddy` file, then send that file to anyone. To add
a character someone sent you, double-click the file or drag it onto the app window, with no restart. If a character with
that name already exists, both are kept.

A `.buddy` file only holds images, video, sound and a description. Nothing in it is ever run, and files with other
content are refused.

## Adding your own characters

Add a character from files, or import a folder (`character.json` and one media
file per state):

| Format | Notes |
|---|---|
| WebM VP9 with alpha | Transparent background |
| MP4 | No transparent background |
| PNG, APNG, GIF, WebP, JPG | White backgrounds can be removed when adding |
| SVG | Drawn as a still image; effects in the file don't play |

Custom sounds: `sounds/done` and `sounds/waiting` as `.wav` or `.mp3`. Very long clips are cut short (limit 50 MB per
file).

On Windows, images larger than the usual size (1024 pixels, say) stay sharp when the character is made bigger.

On Windows, with `"autoSkills": true` in `character.json`, the character's skills go off by themselves:

| When | Skill |
|---|---|
| Sessions have been working for 20 seconds | Shadow clones come out, and vanish when the work stops |
| A session needs you | Teleports to the corner nearest the mouse pointer |
| Sessions are done | Teleports to another corner |

Not while Ctrl+R has it at its biggest, while the screen is locked, or while you drag the character. Turn it on or off for any character
from the right-click menu (**Skills follow the sessions**).

## Configuration

`config.json` in `%APPDATA%\claude-buddy-rs` on Windows or `~/Library/Application Support/claude-buddy-rs` on macOS, key
`port` (default 47329). After changing the port, reconnect to Claude Code.
On Windows, `autoCloneSeconds` (default 20) sets how long sessions work before the shadow clones come out.
`skillKeys` sets the skills' keys, one letter or digit each, `""` for none:
`{"blink": "Q", "clones": "W", "ultimate": "R"}`. Ctrl and Ctrl+Alt stay as they are; restart the app after a change.
The skills switch is also under **Settings**.

## Local API

Listens on `127.0.0.1` only. Requests with an unknown `Origin` or `Host` header are refused, so web pages can't control
the app.

- `POST /hook`: raw Claude Code hook data
- `POST /event`: events from other tools, for example `{"session_id":"a","state":"working","step":"test","label":"Backend"}`
- `GET /sessions`, `GET /usage`, `GET /health`

## Build from source

```
cargo build --release
target\release\claude-buddy-rs.exe     # Windows
target/release/claude-buddy-rs         # macOS
```

Needs Rust, plus the Windows SDK on Windows (to embed the icon) or the Xcode Command Line Tools on macOS. On first run the
app asks whether to connect to Claude Code. If you agree, it adds its own hooks to `.claude/settings.json` in your home
folder, keeping a backup as `settings.json.buddy-rs-backup-*`.

`scripts/package-macos.sh` builds the macOS app from release builds for both kinds of chip.

The hooks store the full path to the program. If you move it, reconnect in Settings. If you delete it by hand, turn off
"Connect to Claude Code" first (or run `--remove-hooks`), otherwise Claude Code reports hook errors.

## Development

```
cargo test
set PREVIEW_DIR=C:\some\folder && cargo test preview -- --ignored
cargo test export_app_icon -- --ignored
```

The second command exports PNGs of the characters and windows. The third regenerates `assets/app.ico` from the logo drawn
in code.

Environment variables for testing without touching real data: `CLAUDE_BUDDY_RS_HOME` (separate data folder, can run side
by side), `CLAUDE_CONFIG_DIR` (Claude folder), `CLAUDE_BUDDY_RS_THEME=dark|light`, `CLAUDE_BUDDY_RS_BACKDROP=blur`,
`CLAUDE_BUDDY_RS_KEEP_ANIMATING=1` (Windows: keeps animating while locked, for measuring CPU),
`CLAUDE_BUDDY_RS_OPEN_PAGE=0|1|2` (macOS: opens the main window at start), `CLAUDE_BUDDY_RS_UPDATE_URL` with
`CLAUDE_BUDDY_RS_UPDATE_FROM` (updates from a local test server, pretending to be that version).

## License

MIT, see [LICENSE](LICENSE).
