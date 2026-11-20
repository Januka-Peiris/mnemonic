# Android Redesign Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Restyle every screen of the Android app to the agreed dark, dense, minimal look, with icons, chat bubbles, tool pills, a permission bottom sheet and a New task sheet.

**Architecture:** Keep `api/`, `push/`, the view models' polling and data logic, and navigation routes. Replace the theme and Compose UI, adding a small shared component set in `ui/`, plus a few pure functions (tool-run summary, chat status, permission button order, markdown lists) with unit tests.

**Tech Stack:** Kotlin 2.4.21, AGP 8.10.1, Jetpack Compose (BOM 2025.05.01), Material 3, compile SDK 35.

**Spec:** `docs/superpowers/specs/2026-11-20-android-redesign-design.md`. Reference mockup: `docs/superpowers/specs/2026-11-20-android-redesign-mockup.html` (open it in a browser and match it).

## Global Constraints

- Never use em dashes anywhere: code, comments, strings, docs, commit messages.
- Commit messages end with a blank line then `Written with Claude Code`.
- Keep AGP, Kotlin, Compose BOM and compile SDK versions as they are; the only new dependency allowed is `androidx.compose.material:material-icons-extended` (BOM-managed) if you use it.
- Dark only. Colour tokens, type sizes, spacing and radii are exactly the spec's Theme section; nothing larger than the spec's sizes.
- Do not change `api/`, `push/` crypto and registration, or the polling, retry and data logic in the view models, except where a task says so.
- Every action that exists today stays reachable.
- Kotlin tests: `script/android test`. Build: `script/android build`.
- Emulator checks: AVD `pixel35` (`$HOME/Library/Android/sdk/emulator/emulator -avd pixel35 -no-snapshot-save -no-audio &`), scratch mock `/tmp/phone-mock/mock.py` (not committed; read it for arguments and port; extend it freely; stop it when done), connect to `http://10.0.2.2:<port>`. Jay's real phone may be plugged in: always set `ANDROID_SERIAL=emulator-5554` (or the emulator's serial from `adb devices`) for `./gradlew installDebug` and `adb`, so nothing is installed on the phone. Screenshots go to `/tmp/android-redesign-shots/`.
- Errors from the Mac are still shown verbatim.

## Review Focus

1. Long workspace, chat and repo names: names ellipsize; status labels, PR labels and top-bar actions never leave the screen. Pinned by the Task 2 and Task 3 emulator checks using the mock's long workspace name.
2. A permission with more than three options, or no allow-once option: buttons wrap, and with no allow-once option every button is outlined. Pinned in Task 4 (`PermissionButtonsTest`).
3. Several pending permissions, or a new one arriving after the sheet was swiped away: the oldest undismissed prompt shows; a dismissed prompt doesn't reopen by itself, but a different new one does. Pinned in Task 4 (`PermissionSheetStateTest`).
4. The keyboard open on Home's New task sheet and the chat composer: the field and send button stay above the keyboard. Pinned by the Task 2 and Task 3 emulator checks (screenshot with keyboard up).
5. A running tool inside a run of finished tools: it must not be folded away, so the "Running" pill shows. Pinned in Task 1 (`ChatStateTest.runningToolIsNeverFolded`).

---

### Task 1: Theme, shared components and the new pure logic

**Files:**
- Modify: `android/app/src/main/java/dev/mnemonic/phone/ui/Theme.kt`
- Create: `android/app/src/main/java/dev/mnemonic/phone/ui/Components.kt`
- Create: `android/app/src/main/java/dev/mnemonic/phone/chat/ToolSummary.kt`
- Create: `android/app/src/main/java/dev/mnemonic/phone/chat/ChatStatus.kt`
- Modify: `android/app/src/main/java/dev/mnemonic/phone/chat/ChatState.kt` (folding only)
- Modify: `android/app/src/main/java/dev/mnemonic/phone/chat/Markdown.kt` (list blocks)
- Modify: `android/app/src/main/java/dev/mnemonic/phone/MainActivity.kt` (dark system bars)
- Test: `chat/ToolSummaryTest.kt`, `chat/ChatStatusTest.kt`, `chat/MarkdownTest.kt` (extend), `chat/ChatStateTest.kt` (extend)

**Interfaces:**
- Produces: `object MnColors` with the spec's tokens as `Color` vals named `background, surface, surfaceRaised, border, divider, text, textSecondary, textMuted, accent, accentText, accentSoft, userBubble, userBubbleText, success, successSoft, warning, warningSoft, warningCard, error, errorSoft`; `MnemonicTheme(content)` applying a dark `ColorScheme` built from them and a `Typography` with the spec's sizes; `val MonoFamily: FontFamily`.
- Produces in `ui/Components.kt` (all `@Composable`):
  - `enum class StatusKind { NeedsYou, Working, Finished, Idle, Error }`
  - `StatusSquare(kind: StatusKind)`: 22dp square, 6dp radius, icon per spec Home section.
  - `StatusLabel(text: String, kind: StatusKind)`: 10sp label on the kind's soft colour.
  - `TopBar(title: String, subtitle: String?, onBack: (() -> Unit)?, actions: @Composable RowScope.() -> Unit = {})`
  - `Pill(text: AnnotatedString, accent: Boolean = false, leading: @Composable (() -> Unit)? = null, onClick: (() -> Unit)? = null)`
  - `ReplyChip(text: String, onClick: () -> Unit)`
  - `Composer(value: String, onValueChange: (String) -> Unit, placeholder: String, onSend: () -> Unit, onMic: (() -> Unit)?, sendEnabled: Boolean, onFocused: (() -> Unit)? = null)`: round field, mic icon (hidden when `onMic` is null), accent round send button.
  - `PrimaryButton(text, onClick, enabled = true, modifier)` and `SecondaryButton(text, onClick, enabled = true, modifier)`: 7dp radius, 11sp, accent and outlined.
  - `ErrorBar(text: String)` and `SectionLabel(text: String)`.
- Produces: `fun toolRunSummary(tools: List<ChatItem.Tool>): String` and `fun toolRunFailed(tools: List<ChatItem.Tool>): Int`.
- Produces: `enum class ChatStatus { Working, NeedsYou, Error, Idle }` and `fun chatStatus(generating: Boolean, hasPendingPermission: Boolean, agentState: TeamAgentState?): ChatStatus` (needs-you beats working, working beats error, error beats idle).
- Produces: `Block.ListBlock(ordered: Boolean, items: List<List<Inline>>)` from `parseBlocks`.
- Changes: `ChatState` folding never folds a tool whose status is `in_progress` (it flushes the run like a waiting tool), and `FoldedTools.label` comes from `toolRunSummary`.

- [ ] **Step 1: Write failing tests.**
  - `ToolSummaryTest`:
    - `mixedKindsAreCountedByKind`: tools of kinds read, read, read, read, execute, execute give `"read 4 files · ran 2 commands"`.
    - `editSearchFetchAndOther`: edit, search, search, fetch, think give `"edited 1 file · searched 2 times · fetched 1 page · 1 other step"` (singular forms for 1).
    - `singleToolUsesItsTitle`: one tool titled `cargo test` gives `"cargo test"`.
    - `failuresAreCounted`: two failed of three gives `toolRunFailed == 2`.
  - `ChatStatusTest`: `(true, true, Working)` gives `NeedsYou`; `(true, false, Error)` gives `Working`; `(false, false, Error)` gives `Error`; `(false, false, null)` gives `Idle`.
  - `MarkdownTest`:
    - `bulletListBecomesListBlock`: `"- a\n- **b**"` gives one `ListBlock(ordered = false)` with 2 items, the second containing `Bold`.
    - `numberedList`: `"1. x\n2. y"` gives `ordered = true`.
    - `dashWithoutSpaceIsParagraph`: `"-not a list"` gives `Paragraph`.
  - `ChatStateTest.runningToolIsNeverFolded`: three tools (completed, in_progress, completed) give items `[FoldedTools(1), Tool(in_progress), FoldedTools(1)]`.
- [ ] **Step 2: Run** `script/android test`; the new tests fail.
- [ ] **Step 3: Implement** the logic, then the theme and components. `MainActivity` uses `enableEdgeToEdge` with dark system bar styles and wraps content in `MnemonicTheme`. Leave the screens' existing composables in place for now; they pick up the new theme.
- [ ] **Step 4: Run** `script/android test` (all pass) and `script/android build`.
- [ ] **Step 5: Commit** `Add the Android app's dark theme, shared components and tool summaries`.

### Task 2: Home and the New task sheet

**Files:**
- Modify: `android/app/src/main/java/dev/mnemonic/phone/home/HomeScreen.kt`
- Modify: `android/app/src/main/java/dev/mnemonic/phone/home/HomeViewModel.kt` (only to expose what the sheet needs: repositories, agents, start task, last repo)
- Create: `android/app/src/main/java/dev/mnemonic/phone/task/NewTaskSheet.kt`
- Delete: `android/app/src/main/java/dev/mnemonic/phone/task/NewTaskScreen.kt` (move its logic into the sheet or a small view model; remove the `newTask` route from `ui/Navigation.kt`)
- Modify: `android/app/src/main/java/dev/mnemonic/phone/AppState.kt` (last repository id)

**Interfaces:**
- Consumes: Task 1's `MnColors`, `StatusSquare`, `StatusLabel`, `SectionLabel`, `ErrorBar`, `Composer`, `PrimaryButton`, `SecondaryButton`.
- Produces: `AppState.lastRepositoryId: Flow<String?>` and `suspend fun setLastRepositoryId(id: String)`.
- Produces: `fun preselectRepository(repositories: List<PhoneRepository>, lastId: String?): PhoneRepository?` (the last one if still present, else the first, else null).

Behaviour: the spec's Home and New task sections, matching the mockup's Home phone. A workspace row with exactly one chat opens it; with several it lists them inline (current behaviour). Pull to refresh stays. The unreachable banner and answer errors use `ErrorBar`.

- [ ] **Step 1: Write failing test** `PreselectRepositoryTest`: last id present returns it; last id missing returns the first; empty list returns null.
- [ ] **Step 2: Run** `script/android test`; it fails.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `script/android test`, then the emulator check: Home with the mock (Needs you card, finished and errored rows, repo headings, the long workspace name row), the New task sheet closed and open with the keyboard up, and a started task opening its chat. Compare with the mockup's Home phone and fix differences in spacing and size. Screenshots: `home.png`, `newtask.png`, `newtask-keyboard.png`.
- [ ] **Step 5: Commit** `Restyle the Android app's home and turn new task into a sheet`.

### Task 3: Chat screen

**Files:**
- Modify: `android/app/src/main/java/dev/mnemonic/phone/chat/ChatScreen.kt`
- Modify: `android/app/src/main/java/dev/mnemonic/phone/chat/ChatViewModel.kt` (only to expose `chatStatus` inputs and the repo and agent names for the subtitle)
- Modify: `android/app/src/main/java/dev/mnemonic/phone/chat/Markdown.kt` (render `ListBlock`)

**Interfaces:**
- Consumes: Task 1's components, `toolRunSummary`, `toolRunFailed`, `chatStatus`, `ListBlock`.
- Produces: a slot in `ChatScreen` where Task 4's permission sheet and bar go: render nothing for permissions in this task except keeping today's inline permission cards working, so Task 4 can replace them.

Behaviour: the spec's Chat section, matching the mockup's Chat and Chat menu phones.
- Top bar: back, title, `<repo> · <agent>`, `StatusLabel` from `chatStatus`, and the `⋯` menu (Stop agent only while generating, View diff, the PR action, New chat with the agent submenu). The Merge confirm dialog is restyled; it still posts the confirmed head.
- Transcript: user bubbles; plain agent text with lists; Thinking pill; folded tool pills that expand to compact rows; a running pill for an unfolded `in_progress` tool; the plan card with icons.
- Bottom: the reply chips in one scrolling row, then `Composer`. Bottom pinning and keyboard behaviour stay as today.

- [ ] **Step 1: Run** `script/android test` to confirm a green start (this task adds no pure logic beyond Task 1's).
- [ ] **Step 2: Implement.**
- [ ] **Step 3: Run** `script/android test`, then the emulator check with the mock (extend the mock so a chat has a user message, a markdown list reply, a fold of mixed kinds, an in_progress tool and a plan): `chat.png`, `chat-menu.png`, `chat-fold-open.png`, `chat-keyboard.png`, `merge-confirm.png`. Compare with the mockup and fix differences.
- [ ] **Step 4: Commit** `Restyle the Android app's chat`.

### Task 4: Permission sheet

**Files:**
- Create: `android/app/src/main/java/dev/mnemonic/phone/chat/PermissionSheet.kt`
- Modify: `android/app/src/main/java/dev/mnemonic/phone/chat/ChatScreen.kt`, `ChatViewModel.kt` (dismissed set, answering)
- Test: `chat/PermissionButtonsTest.kt`, `chat/PermissionSheetStateTest.kt`

**Interfaces:**
- Consumes: Task 1's components; `ChatState.permissions` and tool kinds; `ChatViewModel`'s existing answer call (409 handling unchanged).
- Produces:
  - `fun permissionTitle(kind: String?): String`: `"Run this command?"` for `execute`, `"Edit this file?"` for `edit`, `"Allow this?"` otherwise.
  - `fun permissionButtons(options: List<PermissionOption>): Pair<PermissionOption?, List<PermissionOption>>`: the first option whose kind is `allow_once` is the primary, the rest in the Mac's order; no allow_once means primary null and all options in the list.
  - `fun sheetPermission(pending: List<TeamPermissionRequest>, dismissed: Set<String>): TeamPermissionRequest?`: the first pending request (Mac's order, oldest first) whose `toolCallId` isn't dismissed.

Behaviour: the spec's Permission section, matching the mockup's Permission phone. Swiping the sheet away adds the tool call id to `dismissed` and shows a slim warning bar above the composer, "Needs your OK · Review", which clears that id and reopens the sheet. The inline permission cards from Task 3 go away; the tool row for a waiting tool stays visible in the transcript.

- [ ] **Step 1: Write failing tests.**
  - `PermissionButtonsTest`:
    - `allowOnceIsPrimary`: options [reject_once, allow_once, allow_always] give primary `allow_once` and rest `[reject_once, allow_always]`.
    - `noAllowOnceMeansAllSecondary`: [allow_always, reject_once] give primary null and both in the list.
    - `fiveOptionsKeepOrder`.
    - `titles`: execute, edit, null.
  - `PermissionSheetStateTest`:
    - `oldestFirst`: pending [a, b] with nothing dismissed gives `a`.
    - `dismissedSkipsToNext`: pending [a, b] with dismissed {a} gives `b`.
    - `allDismissedGivesNull`.
    - `newRequestAfterDismissShows`: pending [a, c] with dismissed {a} gives `c`.
- [ ] **Step 2: Run** `script/android test`; they fail.
- [ ] **Step 3: Implement** with `ModalBottomSheet` (light scrim), the code box, and the button row using `FlowRow` so it wraps.
- [ ] **Step 4: Run** `script/android test`, then the emulator check: `permission.png`, `permission-dismissed-bar.png`, plus answering from the sheet reaching the mock.
- [ ] **Step 5: Commit** `Show permission prompts as a bottom sheet in the Android app`.

### Task 5: Diff, Connect, Settings and notifications

**Files:**
- Modify: `android/app/src/main/java/dev/mnemonic/phone/diff/DiffScreen.kt`
- Modify: `android/app/src/main/java/dev/mnemonic/phone/connect/ConnectScreen.kt`
- Modify: `android/app/src/main/java/dev/mnemonic/phone/settings/SettingsScreen.kt`
- Modify: `android/app/src/main/java/dev/mnemonic/phone/push/Notifications.kt` (colour and small icon only)
- Create: `android/app/src/main/res/drawable/ic_notification.xml` (monochrome vector of the app mark)
- Test: `diff/DiffLinesTest.kt`

**Interfaces:**
- Consumes: Task 1's components.
- Produces: `enum class DiffLineKind { File, Hunk, Added, Removed, Context }` and `fun diffLineKind(line: String): DiffLineKind` (`diff --git`, `+++`, `---` give File; `@@` gives Hunk; `+` gives Added; `-` gives Removed; else Context).

Behaviour: the spec's Diff, Connect and Settings, and Notifications sections. Notifications call `setColor(MnColors.accent.toArgb())` and use `ic_notification`.

- [ ] **Step 1: Write failing test** `DiffLinesTest` covering each kind, including that `+++ b/x` is File, not Added.
- [ ] **Step 2: Run** `script/android test`; it fails.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `script/android test`, `script/android build`, then the emulator check: `diff.png`, `connect.png`, `connect-error.png`, `settings.png`, and one posted notification (`notification.png`, posted with a temporary debug hook you remove before committing).
- [ ] **Step 5: Commit** `Restyle the Android app's diff, connect, settings and notifications`.
