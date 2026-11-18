# Android App Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A native Android app (Kotlin, Jetpack Compose) that does what the phone page does, plus Firebase Cloud Messaging alerts the Mac encrypts per RFC 8291.

**Architecture:** The app is a client of the existing `/v1` phone API; the Mac gains a device table, two routes and an FCM sender next to its web push fan-out. Wire shapes are pinned by JSON fixtures that a Rust test generates and a Kotlin test parses, so the two sides can't drift silently.

**Tech Stack:** Rust (mnemonic-core, mnemonic-panels, settings_content), Kotlin 2.x, Jetpack Compose with Material 3, Gradle with the Android Gradle Plugin, OkHttp, kotlinx.serialization, Firebase Messaging.

**Spec:** `docs/superpowers/specs/2026-11-18-android-app-design.md`. The page's behaviour it mirrors is in `docs/superpowers/specs/2026-11-17-phone-page-v2-design.md` and the page itself, `crates/mnemonic-core/src/services/phone_page.html`.

## Global Constraints

- Never use em dashes anywhere: code, comments, strings, docs, commit messages.
- Rust: the repo's `CLAUDE.md` rules. No `unwrap()`, no `let _ =` on fallible calls (use `?`, `.log_err()` or a `match`), full-word names, comments only for non-obvious why.
- Never run `cargo fmt` on the whole workspace. Format only the Rust files you touched: `rustfmt --edition 2024 <files>`.
- Commit messages end with a blank line then `Written with Claude Code`.
- Package and application id `dev.mnemonic.phone`; app name "Mnemonic"; min SDK 29, target and compile SDK 35; `buildToolsVersion = "35.0.0"` (both are already installed; don't make Gradle download other platforms).
- Build environment: `JAVA_HOME=/opt/homebrew/opt/openjdk@17`, `ANDROID_HOME=$HOME/Library/Android/sdk`. Set them per command; never edit shell profiles.
- `android/app/google-services.json` is gitignored and may be absent; the app and its unit tests must build without it.
- The phone server's caller rules don't change: owner login only, Mac's own addresses refused, POSTs need a JSON body, `X-Mnemonic-Phone` and a same-origin `Origin`.
- Every POST from the app sends `Content-Type: application/json`, `X-Mnemonic-Phone: app`, and `Origin` equal to the saved address's origin (scheme, host, and port exactly as typed when not default).
- Alert payloads carry only what web push carries today (title, generic body, chat id, url, permission ids). Never message text, commands or diffs.
- Errors from the Mac are shown to the user verbatim.
- Constants copied from the page: workspace poll 5000 ms, chat long-poll `wait=25`, retry backoff 2000 ms doubling to 15000 ms, PR refresh 15000 ms, quick replies exactly `"Yes, go ahead"`, `"Continue"`, `"Open a PR"`.

## Review Focus

1. The address typed with a trailing slash, a path, upper case, or no scheme: the app must normalise it to an origin, and the `Origin` it sends must equal the `Host` OkHttp sends, or every POST fails with 403. Pinned in Task 4 (`MacAddressTest`).
2. A tool call waiting on permission inside a run of tool calls: it must never be folded away. Pinned in Task 4 (`ChatStateTest.waitingToolIsNeverFolded`).
3. A notification Allow tap while Tailscale is off: the notification must change to "Couldn't reach the Mac", never stay as if nothing happened. Pinned in Task 7 (`PermissionReplyTest`).
4. The FCM token rotating on the same install: the Mac must replace that install's row, not add one. Pinned in Task 1 (`push_device_upsert_replaces_same_install`).
5. The Firebase key file missing or unreadable: web push keeps working and the Team window says why app alerts are off. Pinned in Task 2 (`fcm_status_reports_missing_file`).

---

### Task 1: Mac accepts app devices

**Files:**
- Modify: `crates/mnemonic-core/src/db/migrations.rs` (new table after `phone_push_subscriptions`)
- Create: `crates/mnemonic-core/src/repositories/push_device_repository.rs` (register in the repositories module list)
- Modify: `crates/mnemonic-core/src/services/web_push_service.rs` (add `PushDevice`)
- Modify: `crates/mnemonic-core/src/services/phone_server.rs` (routes, `PhoneSources`, tests)
- Modify: `crates/mnemonic-panels/src/phone.rs` (fill the two new sources; the test-alert source may return `Ok(())` after logging until Task 2 wires sending)

**Interfaces:**
- Produces: `pub struct PushDevice { pub install_id: String, pub token: String, pub p256dh: String, pub auth: String, pub name: String }` in `web_push_service`, with `pub fn subscription(&self) -> PushSubscription` (endpoint empty, same keys) and `pub fn check(&self) -> Result<(), String>` (install id passes `is_plain_id`, token non-empty and at most 4096 bytes, name at most 100 chars, keys pass `PushSubscription::check_keys`).
- Produces: `push_device_repository::{upsert(db, &PushDevice) -> Result<(), String>, list(db) -> Result<Vec<PushDevice>, String>, delete(db, install_id: &str) -> Result<(), String>}`.
- Produces: `PhoneSources` gains `save_push_device: Arc<dyn Fn(PushDevice) -> Result<(), String> + Send + Sync>` and `send_test_alert: Arc<dyn Fn() -> Result<(), String> + Send + Sync>`.
- Routes: `POST /v1/push/devices` body `{ "install_id", "token", "p256dh", "auth", "name" }` returns `200 {}` or `400 <reason>`; `POST /v1/push/test` body `{}` returns `200 {}` or `500 <reason>`.

- [ ] **Step 1: Write failing tests**
  - In `push_device_repository` tests (in-memory DB as `push_subscription_repository` tests do): `push_device_upsert_replaces_same_install`: upsert install `a` with token `t1`, then install `a` with token `t2`; `list` returns exactly one device with token `t2`. `push_device_delete_removes_only_that_install`.
  - In `phone_server` tests, following the existing raw-HTTP style: `push_device_route_saves_device` (valid body reaches `save_push_device` once with the same fields), `push_device_route_refuses_bad_keys` (p256dh `"abc"` gets 400, source not called), `app_post_with_matching_origin_is_accepted` (POST to `/v1/push/test` with `X-Mnemonic-Phone: app` and `Origin: http://<Host value>` gets 200 and calls `send_test_alert`).
- [ ] **Step 2: Run them and see them fail.** `cargo test -p mnemonic-core push_device app_post_with_matching_origin` fails to compile or fails.
- [ ] **Step 3: Implement.** Table `phone_push_devices (install_id TEXT PRIMARY KEY, token TEXT NOT NULL, p256dh TEXT NOT NULL, auth TEXT NOT NULL, name TEXT NOT NULL, created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP)`; upsert is `INSERT ... ON CONFLICT(install_id) DO UPDATE`. Route handling sits next to the existing `/v1/push/subscriptions` branch in `handle_action`.
- [ ] **Step 4: Run tests.** `cargo test -p mnemonic-core phone_server push_device` passes, and `cargo check -p mnemonic-panels` is clean.
- [ ] **Step 5: Commit** `Let the phone server register app devices`.

### Task 2: Mac sends alerts through FCM

**Files:**
- Create: `crates/mnemonic-core/src/services/fcm_service.rs` (register in services module)
- Modify: `crates/settings_content/src/settings_content.rs` (`PhoneSettingsContent.fcm_service_account: Option<String>`, doc comment, default `""`)
- Modify: `assets/settings/default.json` (`"fcm_service_account": ""` with a one-line comment in the `phone` block)
- Modify: `crates/mnemonic-panels/src/phone.rs` (`PhoneSettings.fcm_service_account: Option<PathBuf>`, `~` expanded; app fan-out; test alert; status)
- Modify: `crates/mnemonic-panels/src/team_settings.rs` (status line)

**Interfaces:**
- Consumes: Task 1's `PushDevice`, `push_device_repository`, `send_test_alert` source; existing `web_push_service::encrypt`.
- Produces in `fcm_service`:
  - `pub struct ServiceAccount { pub project_id: String, pub client_email: String, pub private_key: String, pub token_uri: String }` with `pub fn load(path: &Path) -> Result<ServiceAccount, String>`; errors read `"The Firebase key file is missing: <path>"`, `"The Firebase key file couldn't be read: <reason>"`.
  - `pub fn jwt_assertion(account: &ServiceAccount, now_seconds: u64) -> Result<String, String>`: RS256, claims `iss = client_email`, `scope = "https://www.googleapis.com/auth/firebase.messaging"`, `aud = token_uri` (Google's is `https://oauth2.googleapis.com/token`), `iat = now`, `exp = now + 3600`.
  - `pub struct AccessTokenCache` that exchanges the assertion (`grant_type=urn:ietf:params:oauth:grant-type:jwt-bearer`) and reuses the token until 5 minutes before its `expires_in`. One process-wide instance.
  - `pub enum FcmOutcome { Delivered, Unregistered }` and `pub fn outcome_for_status(status: u16, body: &str) -> Result<FcmOutcome, String>`: 2xx is Delivered, 404 or a body containing `"UNREGISTERED"` is Unregistered, anything else is `Err` with the status and body.
  - `pub fn send(account: &ServiceAccount, access_token: &str, device: &PushDevice, payload: &[u8]) -> Result<FcmOutcome, String>`: encrypts with `web_push_service::encrypt(payload, &device.subscription())`, POSTs `https://fcm.googleapis.com/v1/projects/<project_id>/messages:send` with `{"message":{"token":<token>,"data":{"payload":<base64url no padding of ciphertext>},"android":{"priority":"HIGH"}}}`.
  - `pub fn fcm_status(path: Option<&Path>) -> Result<(), String>`: `Err("No Firebase key file is set")` when `None`, the `load` error when the file is bad, `Ok` otherwise.
- In `phone.rs`: alongside `send_web_push(&payload)`, call `send_app_push(&payload)` that lists devices, sends to each, deletes on `Unregistered`, logs other errors and continues. A failure here never stops web push or ntfy. The test-alert source sends payload `{"title":"Mnemonic","body":"Test alert from your Mac","chat_id":"","url":"<page url>","permission":null}` to every web push subscription and every device, ignoring the idle rule.
- Team window: under the phone card, when at least one device is registered show `App alerts are off: <reason>` (warning colour) if `fcm_status` errs, and nothing when it is Ok.

- [ ] **Step 1: Write failing tests** in `fcm_service`: `jwt_claims_are_what_google_expects` (generate an RSA key in the test with the workspace `rsa` crate or a checked-in test PEM, decode the JWT with `jsonwebtoken` and assert `iss`, `scope`, `aud`, `exp - iat == 3600`); `unregistered_answer_means_forget_device` (`outcome_for_status(404, "")` and `outcome_for_status(400, "{\"error\":{\"details\":[{\"errorCode\":\"UNREGISTERED\"}]}}")` are `Unregistered`, `(500, "x")` is `Err`); `fcm_status_reports_missing_file` (a path in a temp dir that doesn't exist gives the "missing" error; `None` gives "No Firebase key file is set").
- [ ] **Step 2: Run** `cargo test -p mnemonic-core fcm` and see them fail.
- [ ] **Step 3: Implement** `fcm_service`, the setting, the fan-out, the test alert and the status line. Use `reqwest` blocking as `web_push_service::send` does, and `jsonwebtoken` (already a dependency) for RS256.
- [ ] **Step 4: Run** `cargo test -p mnemonic-core fcm web_push`, `cargo test -p mnemonic-panels phone`, `cargo check -p mnemonic-panels -p settings_content`. All pass.
- [ ] **Step 5: Commit** `Send phone alerts to the Android app through Firebase`.

### Task 3: Android project, build script and push decryption

**Files:**
- Create: `android/settings.gradle.kts`, `android/build.gradle.kts`, `android/gradle.properties`, `android/gradle/wrapper/*`, `android/gradlew`, `android/gradlew.bat`, `android/gradle/libs.versions.toml`
- Create: `android/app/build.gradle.kts`, `android/app/src/main/AndroidManifest.xml`, `android/app/src/main/res/xml/network_security_config.xml`, `android/app/src/debug/res/xml/network_security_config.xml`, launcher icon resources, `android/app/src/main/java/dev/mnemonic/phone/MainActivity.kt` (a placeholder Compose screen showing "Mnemonic")
- Create: `android/app/src/main/java/dev/mnemonic/phone/push/PushCrypto.kt`, `android/app/src/test/java/dev/mnemonic/phone/push/PushCryptoTest.kt`, `android/app/src/test/resources/mac_push_fixture.json`
- Create: `android/.gitignore` (build outputs, `local.properties`, `app/google-services.json`)
- Create: `script/android`
- Modify: `crates/mnemonic-core/src/services/web_push_service.rs` (fixture test)

**Interfaces:**
- Produces: `object PushCrypto { fun generateKeys(): PushKeys; fun decrypt(body: ByteArray, keys: PushKeys): ByteArray }` with `data class PushKeys(val privateKey: ByteArray /* 32-byte P-256 scalar */, val publicKey: ByteArray /* 65-byte uncompressed */, val auth: ByteArray /* 16 bytes */)` and `fun PushKeys.p256dhBase64Url(): String`, `fun PushKeys.authBase64Url(): String` (no padding). `decrypt` parses the single aes128gcm record (salt 16, rs 4, idlen 1, keyid, ciphertext), derives keys exactly as RFC 8291, checks and strips the `0x02` delimiter, and throws `IllegalArgumentException` on anything malformed.
- Produces: `script/android build` (release APK path printed), `script/android install` (build then `adb install -r`), `script/android test` (`./gradlew testDebugUnitTest`).
- Produces: Gradle applies `com.google.gms.google-services` only when `app/google-services.json` exists; `BuildConfig.HAS_FIREBASE` tells the app which.

- [ ] **Step 1: Scaffold** the Gradle project. Gradle isn't installed; `brew install gradle` once to run `gradle wrapper`, then always use `./gradlew`. Pick current stable AGP and Kotlin (with the Compose compiler plugin) that support compile SDK 35. The release network security config permits cleartext only for domain `ts.net` with subdomains; the debug one also permits `10.0.2.2` (the emulator's view of the Mac).
- [ ] **Step 2: Write the failing tests** `PushCryptoTest.decryptsRfc8291Example` (RFC 8291 section 5 values: receiver private `q1dXpw3UpT5VOmu_cf_v6ih07Aems3njxI-JWgLcM94`, public `BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4`, auth `BTBZMqHH6r4Tts7J_aSIgg`, body `DGv6ra1nlYgDCS1FRnbzlwAAEABBBP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27mlmlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A_yl95bQpu6cVPTpK4Mqgkf1CXztLVBSt2Ks3oZwbuwXPXLWyouBWLVWGNWQexSgSxsj_Qulcy4a-fN`, plaintext `When I grow up, I want to be a watermelon`) (the body and public key match the Rust test `encrypts_the_rfc_8291_example` in `web_push_service.rs`; check the receiver private key against RFC 8291 Appendix A if decryption fails) and `PushCryptoTest.decryptsWhatTheMacEncrypts` (reads `mac_push_fixture.json`: receiver private, public, auth, body, plaintext, all base64url).
- [ ] **Step 3: Add the Rust side of the fixture:** test `android_fixture_matches_mac_encryption` in `web_push_service` that runs `encrypt_with` with fixed server private key and salt over plaintext `{"title":"mnemonic / fixture","body":"needs permission to run a command"}` for a fixed receiver, and asserts the result equals `android/app/src/test/resources/mac_push_fixture.json` (read with `include_str!` relative path). When `UPDATE_ANDROID_FIXTURES=1` is set it writes the file instead. Generate it once that way, then run without the variable.
- [ ] **Step 4: Implement `PushCrypto`** with `java.security` / `javax.crypto` only (EC P-256 via `KeyPairGenerator("EC")`, `KeyAgreement("ECDH")`, HMAC-SHA256 HKDF written out, `AES/GCM/NoPadding`).
- [ ] **Step 5: Write `script/android`** (bash, `set -euo pipefail`). It exports `JAVA_HOME` and `ANDROID_HOME` as in Global Constraints. On `build`/`install`, if `~/.config/mnemonic/android-release.jks` doesn't exist it generates a random password, stores it with `security add-generic-password -a "$USER" -s mnemonic-android-release -w <password>`, and runs `keytool -genkeypair` (RSA 4096, alias `mnemonic`, validity 10000 days, dname `CN=Mnemonic`). It never overwrites an existing keystore. It reads the password with `security find-generic-password -s mnemonic-android-release -w` and passes keystore path and password to Gradle as environment variables `MNEMONIC_KEYSTORE` and `MNEMONIC_KEYSTORE_PASSWORD`, which `app/build.gradle.kts` uses for the release signing config (release builds fail with a clear message when they're absent).
- [ ] **Step 6: Run** `script/android test` (both crypto tests pass), `cargo test -p mnemonic-core android_fixture` (passes), `script/android build` (prints an APK path under `android/app/build/outputs/apk/release/`).
- [ ] **Step 7: Commit** `Add the Android app project and push decryption`. Do not commit the keystore or `google-services.json`.

### Task 4: Android API client, models and chat state

**Files:**
- Create under `android/app/src/main/java/dev/mnemonic/phone/`: `api/MacAddress.kt`, `api/MacClient.kt`, `api/Models.kt`, `chat/ChatState.kt`, `home/NeedsYou.kt`
- Create tests: `api/MacAddressTest.kt`, `api/ModelsTest.kt`, `chat/ChatStateTest.kt`, `home/NeedsYouTest.kt`
- Create: `android/app/src/test/resources/wire/{workspaces,chat_update,pr_status,agents,repositories,outcome}.json`
- Modify: `crates/mnemonic-core/src/services/phone_server.rs` (wire fixture test)

**Interfaces:**
- Produces: `data class MacAddress(val baseUrl: String, val origin: String) { companion object { fun parse(text: String): MacAddress? } }`. Rules: trim; add `https://` when there's no scheme; only `http` and `https`; lower-case the host; drop any path, query and fragment; keep a non-default port; `origin` is `scheme://host[:port]`; `baseUrl` is `origin + "/"`; return null for empty or unparseable input.
- Produces: `class MacClient(address: MacAddress, http: OkHttpClient)` with suspend functions returning `MacResult<T>` (`sealed interface MacResult<out T> { data class Ok<T>(val value: T); data class Refused(val status: Int, val reason: String); data class Unreachable(val cause: String) }`): `workspaces()`, `chatUpdate(chatId, cursor: String?, waitSeconds: Int)`, `diff(workspaceId)`, `prStatus(workspaceId)`, `agents()`, `repositories()`, `sendMessage(chatId, text)`, `stop(chatId)`, `answerPermission(chatId, toolCallId, optionId)`, `newChat(workspaceId, agent)`, `startTask(repositoryId, agent, text)`, `createPr(workspaceId)`, `merge(workspaceId, head)`, `registerDevice(installId, token, p256dh, auth, name)`, `sendTestAlert()`. Paths and bodies exactly as `phone_server.rs` routes them. `Refused.reason` is the response body text verbatim. Long-poll calls use a read timeout of `waitSeconds + 10` seconds.
- Produces: `@Serializable` models mirroring `TeamWorkspaceList`, `TeamWorkspace`, `TeamChat`, `TeamAgentState` (snake_case strings, unknown values decode as `idle`), `TeamChatUpdate`, `TeamEntryChange`, `TeamEntry` (kinds `updates`, `completed_plan`, `compaction`, anything else as unknown), `TeamPermissionRequest`, `PhonePrStatus`, `PhoneAgentList`, `PhoneRepositoryList`, `PhoneActionOutcome`. Session updates stay as `JsonObject` and are interpreted by `ChatState`. JSON is configured with `ignoreUnknownKeys = true`.
- Produces: `class ChatState` ported from the page's `applyUpdate`, `rebuildEntry`, `contentText` and `applyFolds` (`phone_page.html`): `fun apply(update: TeamChatUpdate)`, `val items: List<ChatItem>` where `sealed interface ChatItem { UserMessage(text); AgentMessage(text); Thought(text); Tool(id, title, status, waiting: Boolean); FoldedTools(tools: List<Tool>, label: String); Plan(entries); Compaction(summary) }`, `val generating`, `val permissions`, `val cursor`. Folding matches the page: consecutive tool calls collapse into one `FoldedTools` labelled with the single title or `"<n> tool calls"`, and a tool with a pending permission is never folded.
- Produces: `fun needsYou(workspaces: List<TeamWorkspace>, openedAt: (chatId: String) -> Long, installedAt: Long): List<NeedsYouItem>` ported from the page's `needsYou()`, with `sealed interface NeedsYouItem { Waiting(workspace, chat); Finished(workspace, chat, errored: Boolean) }` in the page's order.

- [ ] **Step 1: Rust fixture test** `android_wire_fixtures_match` in `phone_server.rs` tests: builds one realistic value of each wire type (a workspace with two chats, one `waiting`, one with `last_turn_ended_at`; a chat update with a user message chunk, an agent message chunk, three tool calls where the middle one has a pending permission, a plan; a PR status with `can_merge: true`; agents; repositories; an outcome with `chat_id` and `workspace_id`), serialises each with `serde_json::to_string_pretty`, and compares to the files under `android/app/src/test/resources/wire/`, writing them instead when `UPDATE_ANDROID_FIXTURES=1`. Generate, then rerun without the variable.
- [ ] **Step 2: Write failing Kotlin tests.** `MacAddressTest`: `"Jays-MacBook-Pro.tail6cfd16.ts.net:47232/"` gives origin `https://jays-macbook-pro.tail6cfd16.ts.net:47232`; `"http://100.79.128.81:47232/x?y"` gives `http://100.79.128.81:47232`; `"https://host.ts.net:443"` gives `https://host.ts.net`; `"ftp://x"` and `""` give null. `MacClientTest` with OkHttp `MockWebServer`: a POST carries `X-Mnemonic-Phone: app`, `Content-Type: application/json` and `Origin` equal to `MacAddress.origin`; a 409 with body `Open Mnemonic on the Mac first` becomes `Refused(409, "Open Mnemonic on the Mac first")`; a closed port becomes `Unreachable`. `ModelsTest`: each wire fixture decodes. `ChatStateTest`: applying `chat_update.json` yields the user message, agent text, a `FoldedTools` holding the first tool, the waiting tool unfolded, then the last tool, and the plan (`waitingToolIsNeverFolded`); a second update with `reset: true` replaces everything. `NeedsYouTest`: waiting chats come first; a chat whose turn ended before `openedAt` isn't listed; one that ended before `installedAt` isn't listed on first run.
- [ ] **Step 3: Run** `script/android test`; they fail.
- [ ] **Step 4: Implement** the files above.
- [ ] **Step 5: Run** `script/android test` and `cargo test -p mnemonic-core android_wire`; all pass.
- [ ] **Step 6: Commit** `Add the Android app's Mac client and chat state`.

### Task 5: Connect, Home and Settings screens

**Files:**
- Create under `android/app/src/main/java/dev/mnemonic/phone/`: `AppState.kt` (saved address, opened marks, installed-at, install id in `DataStore` preferences), `ui/Theme.kt` (Material 3, dynamic colour on Android 12+), `ui/Navigation.kt`, `connect/ConnectScreen.kt`, `home/HomeScreen.kt`, `home/HomeViewModel.kt`, `settings/SettingsScreen.kt`
- Modify: `MainActivity.kt`

**Interfaces:**
- Consumes: Task 4's `MacAddress`, `MacClient`, `needsYou`, models.
- Produces: navigation routes `connect`, `home`, `chat/{chatId}`, `newTask`, `diff/{workspaceId}`, `settings`; a deep link `mnemonic://chat/<chatId>` that opens `chat/{chatId}` (Task 7's notifications use it). Tasks 6 and 7 add their screens to these routes.

Behaviour, from the spec's Connecting and Screens sections:
- Connect: one text field prefilled `https://` with hint `your-mac.tailnet.ts.net:47232`, Connect button. Calls `workspaces()`. `Ok` saves the address and goes Home; `Refused` shows its reason; `Unreachable` shows `Can't reach the Mac. Is Tailscale on?`.
- Home: "Needs you" cards (Waiting: workspace title `<project> / <name>`, the generic permission line, Allow once / Deny / Open, Allow and Deny shown only when the prompt has both once-only options, loaded through `chatUpdate(chatId, null, 0)` as the page's `loadPrompt` does; Finished: title, `finished` or `errored`, Open). Then workspaces grouped by `project` alphabetically, within a group attention order `waiting_for_approval, error, working, idle` then most recent activity (`idle_seconds` ascending), each row with name, state label as the page's `stateLabel`, and a PR chip (`#<n> <pr_state>`). Rows truncate the name with an ellipsis; state and chip never leave the screen. Pull to refresh, a floating "New task" button, a top-bar menu with Settings. Polls every 5000 ms only while Home is resumed (lifecycle-aware); an `Unreachable` result shows a banner and keeps polling with the 2000 to 15000 ms backoff.
- Opening a chat records its opened mark (now, ms). `installedAt` is written on first launch.
- Settings: the saved address with Change (back to Connect), alert status text (filled in by Task 7; for now `Alerts aren't set up yet`), app version.

- [ ] **Step 1: Write a failing unit test** `HomeOrderTest` for the grouping and ordering function `fun groupWorkspaces(workspaces: List<TeamWorkspace>): List<Pair<String, List<TeamWorkspace>>>` (alphabetical groups, attention order, then `idle_seconds` ascending with null last).
- [ ] **Step 2: Run** `script/android test`; it fails.
- [ ] **Step 3: Implement** the screens.
- [ ] **Step 4: Run** `script/android test` (passes) and check on the emulator: start AVD `pixel35` (`$ANDROID_HOME/emulator/emulator -avd pixel35 -no-snapshot-save &`), run the scratch mock server from `/tmp/phone-mock/mock.py` (not committed; it serves the page's API with fake data; read its header for the port) on the Mac, install the debug APK with `./gradlew installDebug`, connect to `http://10.0.2.2:<port>`, and take screenshots with `adb exec-out screencap -p > /tmp/android-shots/home.png`. Home shows Needs you and grouped workspaces with nothing cut off at the emulator's width. If the mock lacks a route the app needs, extend the mock (it is scratch).
- [ ] **Step 5: Commit** `Add the Android app's connect, home and settings screens`.

### Task 6: Chat, New task, Diff and shipping

**Files:**
- Create under `android/app/src/main/java/dev/mnemonic/phone/`: `chat/ChatScreen.kt`, `chat/ChatViewModel.kt`, `chat/Markdown.kt`, `task/NewTaskScreen.kt`, `diff/DiffScreen.kt`, `voice/Voice.kt`
- Test: `chat/PrActionTest.kt`, `chat/MarkdownTest.kt`

**Interfaces:**
- Consumes: Task 4's `ChatState`, `MacClient`; Task 5's routes.
- Produces: `fun prAction(pr: PhonePrStatus?, agentWorking: Boolean, requestHeld: Boolean): PrAction` with `sealed interface PrAction { None; CreatePr; Waiting /* held after a Create PR tap until a PR exists or the agent stops */; Merge(number, title, method, shortHead) }`, ported from the page's `prAction` and `prRequestHeld`. A merged or closed PR gives `CreatePr`.

Behaviour (spec Screens; page functions named for reference):
- Chat: long-polls `chatUpdate(chatId, cursor, 25)` in a loop while resumed (`followChat`), feeding `ChatState`; backoff 2000 to 15000 ms on `Unreachable`; stops when paused and resumes from the saved cursor. A 404 `Not open` shows the reason. Opens scrolled to the newest item and stays pinned to the bottom when already near it. Messages render the page's inline markdown subset (`renderMarkdown`: code spans, bold, links, bare URLs). Folded tools expand on tap. Permission cards show every option as a button and call `answerPermission`; any 409 means already answered and the card goes. Top bar: back, title `<project> / <name>`, Stop (only while `generating`). Bottom: the three quick replies, a composer with mic and Send (disabled when empty; text over 16 KiB refused with the server's reason), and a bar with New chat (agent picker from `agents()`, default first), Diff, and the PR action. The composer moves above the keyboard (`imePadding`).
- PR: refreshed every 15000 ms while the chat is open. Create PR calls `createPr`; Merge shows a confirm dialog with `Merge #<n>: <title>`, the method and the 7-character head, then calls `merge(workspaceId, head)` and shows the outcome's `message` or the refusal verbatim.
- New task: repository dropdown (`repositories()` order), task text box with mic, agent dropdown (default = first agent), Start disabled until repo and text are set. `startTask` result opens `chat/<chat_id>`; a timeout-style outcome with only `message` shows it and returns Home.
- Diff: monospace, scrolls both ways, the text as served.
- Voice: a mic button that launches `RecognizerIntent.ACTION_RECOGNIZE_SPEECH` (free-form, device language) and appends the best result to the field; hidden when no activity handles the intent. Declare the `<queries>` entry for that intent in the manifest.

- [ ] **Step 1: Write failing tests.** `PrActionTest`: no PR gives `CreatePr`; open PR with `can_merge` gives `Merge` with `shortHead` of 7 chars; `can_merge` false gives `None`; merged gives `CreatePr`; `requestHeld` with no PR gives `Waiting`. `MarkdownTest`: `` "run `ls` now" `` gives a code span for `ls`; `"see https://x.dev/a."` links `https://x.dev/a` without the full stop.
- [ ] **Step 2: Run** `script/android test`; they fail.
- [ ] **Step 3: Implement** the screens.
- [ ] **Step 4: Run** `script/android test` (passes) and check on the emulator against the mock as in Task 5: open a chat, send a quick reply, expand a fold, answer a permission, open New task and Diff, open the merge confirm. Screenshots to `/tmp/android-shots/`.
- [ ] **Step 5: Commit** `Add the Android app's chat, new task, diff and PR screens`.

### Task 7: Alerts on the phone

**Files:**
- Create under `android/app/src/main/java/dev/mnemonic/phone/push/`: `PushKeyStore.kt`, `PushRegistration.kt`, `MnemonicMessagingService.kt`, `Notifications.kt`, `PermissionReplyReceiver.kt`, `PermissionReply.kt`
- Test: `push/PermissionReplyTest.kt`, `push/AlertPayloadTest.kt`
- Modify: `AndroidManifest.xml`, `settings/SettingsScreen.kt`, `home/HomeScreen.kt` (Turn on alerts banner), `app/build.gradle.kts` (Firebase Messaging dependency, compiled in always; only initialised when `BuildConfig.HAS_FIREBASE`)

**Interfaces:**
- Consumes: Task 3's `PushCrypto`; Task 4's `MacClient.registerDevice`, `answerPermission`, `sendTestAlert`; Task 5's deep link and `AppState` install id.
- Produces: `@Serializable data class AlertPayload(val title: String, val body: String, @SerialName("chat_id") val chatId: String, val url: String, val permission: AlertPermission?)` with `AlertPermission(toolCallId, allowOptionId, denyOptionId)` in the Mac's snake_case. `fun replyResultText(result: MacResult<*>, allowed: Boolean): String` returns `"Allowed"` / `"Denied"` on Ok, `"Already answered"` on any 409, `"Couldn't reach the Mac"` otherwise.

Behaviour (spec Alerts):
- Keys: generated once with `PushCrypto.generateKeys()`; the private key and auth are stored encrypted with an AES key held in the Android Keystore (alias `mnemonic_push`), in app-private storage. Builder may choose the exact storage format.
- Registration: when `HAS_FIREBASE` and notification permission is granted, fetch the FCM token and call `registerDevice(installId, token, p256dh, auth, "<Build.MANUFACTURER> <Build.MODEL>")`, on Connect, on each app start, and in `onNewToken`. A failure is shown in Settings' alert status.
- Permission: Android 13+ asks only from a "Turn on alerts" button (Home banner while alerts are off, and Settings). Without Firebase, Settings says `Alerts need the Firebase setup (google-services.json)` and the banner is hidden.
- `onMessageReceived`: base64url-decode `data["payload"]`, decrypt, decode `AlertPayload`, post a notification. Channels: `permission` ("Needs permission", high) when `permission != null`, else `finished` ("Finished", default). Notification id is a stable hash of `chatId`, so a newer alert for the same chat replaces the older one. Tap opens `mnemonic://chat/<chatId>` (Home when `chatId` is empty). Permission alerts with both option ids get actions "Allow once" and "Deny". A payload that fails to decrypt or decode is dropped with a log line.
- `PermissionReplyReceiver` uses `goAsync()`, calls `answerPermission` with a 10 second overall timeout, and replaces the notification (same id, no actions, Open still works) with `replyResultText`.
- Settings: alert status (`Alerts are on`, the registration error, or the Firebase or permission message), Turn on alerts, and Send test alert (`sendTestAlert`, result shown verbatim).

- [ ] **Step 1: Write failing tests.** `PermissionReplyTest`: `Ok` gives `Allowed`/`Denied`; `Refused(409, anything)` gives `Already answered`; `Unreachable` and `Refused(500, ...)` give `Couldn't reach the Mac`. `AlertPayloadTest`: decrypting `mac_push_fixture.json` then decoding gives title `mnemonic / fixture`; a payload JSON with `"permission":{"tool_call_id":"t","allow_option_id":"a","deny_option_id":"d"}` decodes.
- [ ] **Step 2: Run** `script/android test`; they fail.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `script/android test` (passes), `script/android build` with and without a `google-services.json` present (use a dummy one with project id `mnemonic-dummy` for the "with" build, then delete it; neither is committed). On the emulator, Settings shows the Firebase message and the app doesn't crash without Firebase.
- [ ] **Step 5: Commit** `Add alerts to the Android app`.

### After the last task

Add `android/README.md`: what the app is, Jay's one-off Firebase steps from the spec ("What Jay does once": create the project, add Android app `dev.mnemonic.phone`, put `google-services.json` in `android/app/`, save the service account key as `~/.config/mnemonic/firebase-service-account.json` and set `"phone": { "fcm_service_account": "~/.config/mnemonic/firebase-service-account.json" }` in settings.json), and how to install with `script/android install`. Commit with the final review fixes.
