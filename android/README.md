# Mnemonic for Android

The Android app is a phone companion for Mnemonic on your Mac. It connects to the Mac over Tailscale, lists your workspaces and chats, lets you read and reply, start new tasks, review diffs, open and merge pull requests, and shows alerts when an agent needs you.

## Firebase setup (one-off)

Alerts are delivered through Firebase Cloud Messaging.

1. Create a free Firebase project.
2. Add an Android app with the id `dev.mnemonic.phone`.
3. Put the downloaded `google-services.json` in `android/app/`. It is gitignored.
4. In the Firebase project settings, create a service account key and save it as `~/.config/mnemonic/firebase-service-account.json`.
5. Point Mnemonic at it in `settings.json`:

```json
"phone": { "fcm_service_account": "~/.config/mnemonic/firebase-service-account.json" }
```

The app builds and runs without `google-services.json`, but alerts stay off.

## Connecting

Open the Team window on the Mac and use the address it shows, for example `https://your-mac.tailnet.ts.net:47232`. Enter that full address on the phone's Connect screen.

## Installing

With the phone connected over USB or wireless debugging:

```
script/android install
```

Other commands:

```
script/android test
script/android build
```
