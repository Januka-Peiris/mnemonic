# Android app redesign

## Intent

The Android app (PR #40) works, and Jay uses it with real alerts, but he finds it looks bad. Its problems:
- Text buttons ("Back", "Mic", "Send", "Menu") where icons belong.
- Three rows of identical outlined pills that eat a quarter of the chat screen.
- Truncated titles.
- Flat "you" / "agent" labels, a muddy permission card and an ASCII plan.
- No sign that the agent is working.

He chose a look from mockups: the colours and density of a developer tool (Linear or terminal feel) with the calm, minimal layout of a messaging app (Claude or ChatGPT feel), a size smaller than first shown. The reference mockup is `docs/superpowers/specs/2026-11-20-android-redesign-mockup.html` (four phones: Home, Chat, Permission, Chat menu). Builders open it in a browser and match it.

## Goals

- **Every screen in the agreed look:** dark, one purple accent, coloured status labels, small dense type, icons instead of words, no bottom tab bar.
- **Chat reads like a messaging app:**
  - your messages are purple bubbles on the right;
  - the agent's replies are plain text;
  - tool work is one small pill;
  - a running step shows as a "Running ..." pill;
  - permissions slide up from the bottom;
  - secondary actions sit behind a `⋯` menu.
- **Home leads with what needs Jay,** then workspaces under repo headings, with a "Start a new task..." box at the bottom.

## Non-goals

- No change to what the app can do, the Mac API, alerts, or the view models' polling and data logic, beyond the behaviour listed below.
- A light theme, tablets, or a home-screen widget.

## Anti-goals

- Bigger or airier than the mockup. Jay asked twice for it smaller and denser.
- Losing anything the current screens can do. Every action that exists today must still be reachable.

## Approaches

1. **Restyle the existing screens in place (chosen).** Keep the view models, client and push code. Replace the Compose UI and theme. Working, tested logic stays untouched.
2. **Rebuild the UI layer from scratch.** Rejected: it redoes working code for no gain.

Look options shown to Jay:
- A. Material You. He rejected it.
- B. Developer tool. He liked its colours, density and status labels, but not its bottom tab bar.
- C. Messaging app. He liked its chat shape, its composer, and settings hidden behind a gear.

He chose B's colours with C's layout, then a smaller version of that.

## Design

### Theme (dark only)

The app ignores the system light or dark setting, and the status and navigation bars are dark.

Colour tokens, from the mockup:

| Token | Value |
|---|---|
| background | `#0B0D10` |
| surface | `#14171C` |
| surfaceRaised | `#171A20` |
| border | `#262B33` |
| divider | `#161A20` |
| text | `#D7DAE0` |
| textSecondary | `#8B93A1` |
| textMuted | `#6B7280` |
| accent | `#8B5CF6` |
| accentText | `#A78BFA` |
| accentSoft | `#1E1638` |
| userBubble | `#2A2147` (text `#EBE6FF`) |
| success | `#22C55E` on `#0F2A18` |
| warning | `#F5A524` on `#3A2A08` (card fill `#15130C`) |
| error | `#F87171` on `#2A1212` (my call; the mockup had no error colour) |

Type, smaller than Material's defaults:

| Use | Size |
|---|---|
| screen title | 17sp semibold |
| top-bar title | 13sp semibold |
| body | 13sp, line height 1.45 |
| secondary | 11sp |
| section labels | 10sp, upper case, letter-spaced |
| status labels | 10sp |
| code | 11.5sp monospace |

Spacing and radii:
- List rows: 6 to 8dp vertical padding, 14dp horizontal.
- Radii: 6 to 10dp on cards and buttons, 16dp on bubbles, fully round on chips and the composer.

Icons come from Material Symbols:
- back arrow, settings gear, mic, send (arrow up), more (horizontal dots);
- stop, diff, pull request, add, check;
- a spinner for running work.

### Home

- **Top bar:** "Mnemonic" on the left, gear on the right, which opens Settings.
- **Needs you cards** (warning fill and border):
  - A small square status icon, the workspace name, and "<repo> · wants to run a command" (the existing permission line) as the secondary line.
  - The command in code font.
  - **Allow once** (accent) and **Deny** (outlined), shown only when both once-only answers exist, as today.
  - Tapping the card opens the chat. The separate Open button goes.
  - Finished and errored chats show as compact rows in this section, with a green "new" or red "error" label.
- **Repo headings** (section label style), then one row per workspace:
  - a status square: amber "!" needs you, purple half-circle working, green check finished, grey circle idle, red cross error;
  - the name, and a secondary line with the state and time;
  - a small code-font PR label (`#212 ready`, `#812`) on the right, coloured by PR state.
- **Tapping a row** opens its chat directly when it has one, and lists its chats inline when it has several (current behaviour). My call.
- **Composer box at the bottom** reading "Start a new task...", with mic and send icons. Tapping it opens the New task sheet. Pull to refresh stays.
- **The unreachable banner and answer errors** are compact error-coloured bars under the top bar.

### New task (bottom sheet, replaces the screen)

- The sheet rises from Home's composer.
- Its first row has two small dropdown chips: the repo (preselected with the last repo used, stored locally; first in the Mac's order otherwise) and the agent (the Mac's default).
- Below them: the multi-line text box, a mic icon, and an accent send button. Send is disabled until there is text.
- Starting opens the new chat, as today. Errors show in the sheet verbatim.

### Chat

**Top bar:**
- back arrow;
- title (the chat title, or the workspace name);
- secondary line `<repo> · <agent>`;
- a status label: working (accent), needs you (warning), idle (muted) or error;
- a `⋯` button.

**The `⋯` menu** holds:
- Stop agent, shown only while generating;
- View diff;
- the PR action: "Create PR", "Waiting for PR..." (disabled) or "Merge #n", which keeps today's confirm dialog restyled;
- New chat, which keeps the agent picker as a small menu.

**Transcript:**
- Your messages are right-aligned bubbles in userBubble colour, with 16dp radii and a 4dp bottom-right corner.
- The agent's text is plain, full width, rendered from the existing markdown subset plus bullet lists and numbered lists. Today a line starting with "- " shows the dash.
- Thoughts are a muted "Thinking" pill that expands.
- **Folded tool runs** are one muted pill summarising the run by kind:
  - read → "read N files"
  - edit → "edited N files"
  - execute → "ran N commands"
  - search → "searched N times"
  - fetch → "fetched N pages"
  - anything else → "N other steps"

  The parts are joined with " · ", and failures are added as "N failed" in error colour. A single tool shows its own title. Tapping expands the run into compact rows (status icon, title in code font). My call on the wording.
- **A running tool** shows as an accent pill: spinner, "Running", then the tool title in code font.
- **The plan** is a compact surface card titled "Plan" or "Plan done", with check-circle, half-circle and empty-circle icons instead of `[x]`.

**Bottom:**
- the three quick replies as small chips in one horizontal row that scrolls;
- then the round composer: text field "Reply...", mic icon, accent send button.

There is no other bottom row.

### Permission (bottom sheet)

- **When a permission is pending,** a modal bottom sheet opens over a lightly dimmed chat. It holds:
  - a small warning label, "needs you";
  - a title naming the action: "Run this command?" for execute, "Edit this file?" for edit, and "Allow this?" for anything else;
  - the tool's title or command in a code box;
  - one row of buttons: the first allow-once option as the accent button, and the remaining options as outlined buttons in the order the Mac sent them. The row wraps if there are more than three options.
- **Answering** closes the sheet. A 409 closes it and the prompt is gone.
- **Swiping the sheet away** leaves a slim warning bar above the composer ("Needs your OK · Review") that reopens it. My call: the chat stays readable.
- **Several pending prompts** are shown one at a time, oldest first.

### Diff

The same top bar, titled "Diff" with the workspace as the secondary line. It shows:
- file headers as surface bars in code font;
- added lines in green on a faint green background, removed lines in red on a faint red background, hunk headers muted;
- text in code font at 11sp, scrolling both ways.

### Connect and Settings

- **Connect:** "Mnemonic" with a one-line explanation, one outlined text field for the address, one accent Connect button, and errors in error colour beneath.
- **Settings** is a short list of compact rows:
  - Mac address (with Change);
  - Alerts, with the current status and Turn on alerts when off;
  - Send test alert;
  - Version.

### Notifications

The accent colour is set on notifications, and the small icon is a monochrome version of the app icon.

## Tests

Only for new logic:
- the tool-run summary (mixed kinds, a single tool, failures);
- markdown bullet and numbered lists becoming list blocks.

Existing tests keep passing. The screens are checked on the emulator against the mockup, with screenshots of Home, New task, Chat, Permission, the menu, Diff, Connect and Settings.

## Left to the builder

- Exact dp values within the ranges above.
- Compose structure (shared components for status label, pill, chip and composer).
- How the app icon becomes a monochrome notification icon.
- The icon dependency (`material-icons-extended`, or the few icons as vector drawables).
