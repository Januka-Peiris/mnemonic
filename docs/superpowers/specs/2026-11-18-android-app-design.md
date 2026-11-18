# Android app design

## Intent

The phone page (PRs #33 and #36) lets Jay start, drive and ship work on his Mac from his Android phone: a list of workspaces grouped by repo, a "Needs you" section, New task, chat with quick replies and voice, Create PR and Merge, and web push alerts with Allow once and Deny on them. It works, but it is a web page. Jay wants a proper Android app. There is no deadline; he wants it done properly, and without costs wherever possible.

Wrapping the page as a Trusted Web Activity was ruled out: Chrome checks the site's Digital Asset Links through a Google server on the public internet, which can't reach a Mac that is only on the tailnet, so the app would always show an address bar.

The first version of the app does what the page does, natively. Jay will use it and then decide what else it should do; extras wait for that.

## Goals

- A native Android app, installed from an APK, with the page's features: Needs you, workspaces grouped by repo, New task, chat driving, permissions, diff, Create PR, Merge.
- Alerts that arrive with the app closed, with Allow once and Deny on permission alerts, without ntfy.
- Native feel: Material 3 with dynamic colour, back gesture, system voice input, the keyboard pushing the composer up, notification channels.
- Free to build and run: no Play Store account, no paid services.

## Non-goals

- Features the page doesn't have (home-screen widget, share sheet, quick settings tile, lock-screen live updates). Decided after Jay uses version one.
- iOS, the Play Store, or reaching the Mac from outside the tailnet.
- Retiring the web page. It stays, and both work at once.
- Building the app in CI.

## Anti-goals

- Widening who can drive the Mac. The phone server's caller rules stay exactly as they are: Jay's own Tailscale login only, never the Mac's own addresses.
- Secrets in alerts. Alert content stays what web push carries today (repo and workspace names, the generic line, ids), and Google must not be able to read it.
- A second copy of the Mac's logic in the app. The app is a client of the existing `/v1` API; anything the Mac decides stays on the Mac.

## Constraints

- The phone reaches the Mac only through Tailscale, at the address the Team window shows (`https://<MagicDNS name>:47232/`, or `http://` when the certificate couldn't be had).
- Android cannot receive browser web push. Push to a closed app on Android goes through Firebase Cloud Messaging (FCM). FCM is free with no message limit; it needs a Firebase project on Jay's Google account.
- Build tools on the Mac: Android SDK at `~/Library/Android/sdk` (platform 35, build-tools 35.0.0), OpenJDK 17 from Homebrew at `/opt/homebrew/opt/openjdk@17`. Builds set `JAVA_HOME` themselves; nothing is added to Jay's shell profile.
- The repo's Rust rules apply to every Mac-side change.

## Approaches

**What kind of app.**
1. Trusted Web Activity around the page. Rejected: the asset link check needs a public server (above).
2. Kotlin with Jetpack Compose (chosen). Google's own toolkit, the most native feel, nothing new to install beyond the JDK.
3. React Native with Expo, or Flutter. Would also give iOS later, at the cost of a JavaScript or Dart toolchain and a less native feel. Rejected; Jay asked for Android.

**How alerts reach the app.**
1. FCM (chosen). Free; Android delivers it even when the app is closed and the phone dozes. The Mac encrypts each message the same way it encrypts web push (RFC 8291) with a key the app generated, so Google carries only ciphertext.
2. A connection the app keeps open to the Mac. Needs a permanent "running" notification, drains battery, and Android still cuts it in deep doze. Rejected.
3. UnifiedPush through a distributor app such as ntfy. Rejected; Jay doesn't want ntfy.

## Design

### Where it lives

- `android/` at the repo root: a single-module Gradle project (`android/app`), Kotlin, Jetpack Compose, Material 3, with the Gradle wrapper committed. Package and application id `dev.mnemonic.phone`. App name "Mnemonic". Min SDK 29 (Android 10), target and compile SDK 35.
- `script/android` builds and installs: `script/android build` produces a release APK; `script/android install` builds and runs `adb install -r` to a phone connected over USB or wireless debugging. It sets `JAVA_HOME` and `ANDROID_HOME` for itself.
- Release signing uses a keystore Jay generates once with `keytool` (the script does it on first run), stored outside the repo at `~/.config/mnemonic/android-release.jks`, password in the macOS keychain. Updates must be signed with the same key, so the script never regenerates an existing one.
- `android/app/google-services.json` (Firebase's app config) is gitignored. The Firebase Gradle plugin is applied only when the file exists, so the app and its unit tests build without it; such a build has everything except alerts, and Settings says alerts need the Firebase setup. (My call: lets the work proceed before Jay's Firebase setup.)

### Connecting

- First launch shows one screen: the Mac's address, pasted or typed, prefilled with `https://` and `:47232`. Connect calls `GET /v1/workspaces`; on success the address is saved and the app opens Home. On failure it shows the server's own reason (for example "Only this Mac's owner may connect") or "Can't reach the Mac. Is Tailscale on?".
- Settings (from Home's menu) can change the address, show alert status, and send a test alert.
- Any address the phone server accepts works (MagicDNS name, short machine name, or Tailscale IP); the app derives `Origin` from the address it connects to, which matches the `Host` it sends, so the server needs no change.
- Cleartext is allowed only for hosts under `ts.net`, through the network security config, so the HTTP fallback works over Tailscale (which encrypts it anyway) and nowhere else.

### Talking to the Mac

- The app uses the existing `/v1` routes unchanged. On every POST it sends `Content-Type: application/json`, `X-Mnemonic-Phone: app` and `Origin` set to the saved address's origin. A native app can set these, and they satisfy the server's existing check, which exists to stop other web pages in a browser, not other programs on Jay's tailnet devices (those are already refused unless they come from Jay's login).
- Live chat uses the same long poll the page uses (`GET /v1/chats/<id>?cursor=...&wait=...`). Home refreshes the workspace list while it is on screen, at the page's interval, and stops when the app is in the background.
- Errors from the Mac (409 "Open Mnemonic on the Mac first", `gh` output on merge, and so on) are shown verbatim, as the page does.

### Screens

Each mirrors the page's behaviour in the v2 design (`2026-11-17-phone-page-v2-design.md`), drawn with Material components.

- **Home:** "Needs you" first (pending permissions with inline Allow once / Deny / Open, finished and errored chats since last opened), then workspaces grouped by repo with status and PR chip. A floating "New task" button. Pull to refresh. The "last opened" marks live in the app's local storage, with the same first-run baseline as the page.
- **New task:** repo dropdown (Mac's order), multi-line task box, agent dropdown (Mac's default selected), Start. Voice comes from the keyboard's own mic and from a mic button that starts Android's speech recognizer (`RecognizerIntent`) and fills the box. Start opens the new chat.
- **Chat:** messages with tool calls folded as on the page, permission cards with their buttons, opens at the newest message. Composer pinned to the bottom with the mic button, the three quick replies above it, and Stop, New chat, Diff and Create PR or Merge in a bottom bar. Merge asks for confirmation with the title, method and short head commit, and sends the head the app saw.
- **Diff:** monospace text, scrollable both ways, as the page shows it.

### Alerts

- **Registration:** on first launch after connecting, and whenever FCM gives a new token, the app generates (once) a P-256 key pair and a 16-byte auth secret, keeps the private key in the Android Keystore or encrypted app storage, and POSTs `/v1/push/devices` `{ "install_id", "token", "p256dh", "auth", "name" }`. `install_id` is a random id the app makes once per install, so a new token from the same install replaces the old row instead of adding one. (My call.) Android 13+ asks for notification permission from an explicit "Turn on alerts" button, as the page does.
- **Mac side:** a new table `phone_push_devices` (install id primary key, token, keys, name, created). The existing alert fan-out in `phone.rs` sends to FCM devices next to web push subscriptions, same payload, same idle rule, same de-duplication. The payload is encrypted with the existing `web_push_service` RFC 8291 code using the device's keys, base64url-encoded, and sent as an FCM data message with high priority through the FCM HTTP v1 API.
- **FCM credentials on the Mac:** Jay downloads a Firebase service account key once, saves it as `~/.config/mnemonic/firebase-service-account.json`, and points the new settings key `phone.fcm_service_account` at that path by editing settings.json (no picker in the UI). The Mac reads the file when it needs a new access token, so a moved or revoked key shows up within the hour; the Team window's "This computer" card shows whether app alerts are on and, if not, the reason (file missing, unreadable, or Google refusing it). (My call.) The Mac exchanges a signed JWT (RS256, `jsonwebtoken`) for an OAuth access token and caches it until shortly before expiry. FCM answering `UNREGISTERED` (404) deletes that device.
- **On the phone:** the FCM service decrypts the payload and posts a notification on one of two channels, "Needs permission" (high importance) and "Finished" (default). Tapping opens the chat. Permission alerts carry Allow once and Deny actions handled by a broadcast receiver that POSTs `/v1/chats/<id>/permission` without opening the app, then replaces the notification with "Allowed", "Denied" or "Already answered" (any 409). If the Mac can't be reached or answers anything else within 10 seconds, the notification is replaced with "Couldn't reach the Mac" and keeps an Open action, so a tap is never lost silently. (My call.)
- Settings' test alert POSTs `/v1/push/test`, which sends one generic alert to every device and subscription, ignoring the idle rule.

### New Mac routes

Under the existing caller rules.

- `POST /v1/push/devices`: register or refresh a device.
- `POST /v1/push/test`: send a test alert.

### Tests

Only for behaviour that could break silently.

- Kotlin: the app's RFC 8291 decryption reproduces the RFC's worked example, and decrypts what the Mac's `web_push_service::encrypt` produces (a fixture generated by a Rust test and checked in).
- Kotlin: the API client parses JSON fixtures captured from the real server for workspaces, chat updates and PR status.
- Rust: the FCM JWT has the service account's email as `iss`, the FCM scope, and the Google token URL as `aud`; an `UNREGISTERED` answer deletes the device; a device registration with malformed keys gets 400.
- Rust: a POST with `X-Mnemonic-Phone: app` and a matching `Origin` is accepted (extend the existing caller tests).

## Left to the builder

- Gradle and library versions, the app's internal architecture (view models, navigation), and the HTTP and JSON libraries.
- How the app stores its private key.
- The icon (start from `crates/zed/resources/app-icon@2x.png`) and visual details within Material 3.
- How `script/android` keeps the keystore password in the keychain.

### Verifying

- The builder checks screens on the Android emulator (system images are installed) against a real Mnemonic phone server where it can. Push delivery and notification actions end to end are checked by Jay on his phone after the Firebase setup.

## What Jay does once

- Create a free Firebase project, add an Android app with id `dev.mnemonic.phone`, download `google-services.json` into `android/app/`, and download a service account key for the Mac.
- Turn on wireless debugging (or plug in USB) the first time the app is installed.
