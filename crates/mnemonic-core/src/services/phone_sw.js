"use strict";

self.addEventListener("install", () => self.skipWaiting());
self.addEventListener("activate", (event) => event.waitUntil(self.clients.claim()));

self.addEventListener("push", (event) => {
  let payload;
  try {
    payload = event.data ? event.data.json() : {};
  } catch (error) {
    payload = { title: "Mnemonic", body: event.data ? event.data.text() : "" };
  }
  const options = {
    body: payload.body || "",
    tag: payload.chat_id,
    data: payload,
  };
  if (payload.permission) {
    options.actions = [
      { action: "allow", title: "Allow once" },
      { action: "deny", title: "Deny" },
    ];
  }
  event.waitUntil(self.registration.showNotification(payload.title || "Mnemonic", options));
});

async function answerPermission(notification, allow) {
  const payload = notification.data;
  const permission = payload.permission;
  let result;
  try {
    const response = await fetch("/v1/chats/" + encodeURIComponent(payload.chat_id) + "/permission", {
      method: "POST",
      headers: { "Content-Type": "application/json", "X-Mnemonic-Phone": "1" },
      body: JSON.stringify({
        tool_call_id: permission.tool_call_id,
        option_id: allow ? permission.allow_option_id : permission.deny_option_id,
      }),
    });
    if (response.ok) result = allow ? "Allowed" : "Denied";
    else if (response.status === 409) result = "Already answered";
    else result = "Couldn't answer (" + response.status + ")";
  } catch (error) {
    result = "Couldn't reach the Mac";
  }
  await self.registration.showNotification(payload.title || "Mnemonic", {
    body: result,
    tag: payload.chat_id,
  });
}

async function openPage(url) {
  const clients = await self.clients.matchAll({ type: "window", includeUncontrolled: true });
  for (const client of clients) {
    if (client.url.startsWith(self.location.origin) && "focus" in client) {
      await client.focus();
      if ("navigate" in client && url) await client.navigate(url);
      return;
    }
  }
  await self.clients.openWindow(url || "/");
}

self.addEventListener("notificationclick", (event) => {
  const notification = event.notification;
  notification.close();
  const payload = notification.data || {};
  if ((event.action === "allow" || event.action === "deny") && payload.permission) {
    event.waitUntil(answerPermission(notification, event.action === "allow"));
    return;
  }
  event.waitUntil(openPage(payload.url));
});
