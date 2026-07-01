// HOLDFAST Klaxon — minimal Web Push display service worker.
// Registered from the inbox page with scope '/'. On a push message it decodes the (RFC 8291
// encrypted, JSON) payload sent by Klaxon's fan-out and shows a notification; clicking it focuses
// an already-open tab for the target URL, or opens one. Everything is defensive: a payload that
// is missing, non-JSON, or partial still produces a usable notification.

self.addEventListener('install', function (event) {
  self.skipWaiting();
});

self.addEventListener('activate', function (event) {
  event.waitUntil(self.clients.claim());
});

self.addEventListener('push', function (event) {
  var payload = { title: 'New notification', body: '', url: '/', icon: undefined };
  try {
    if (event.data) {
      payload = Object.assign(payload, event.data.json());
    }
  } catch (e) {
    if (event.data) {
      payload.body = event.data.text();
    }
  }
  event.waitUntil(
    self.registration.showNotification(payload.title || 'New notification', {
      body: payload.body || '',
      icon: payload.icon || undefined,
      data: { url: payload.url || '/' }
    })
  );
});

self.addEventListener('notificationclick', function (event) {
  event.notification.close();
  var url = (event.notification.data && event.notification.data.url) || '/';
  event.waitUntil(
    self.clients.matchAll({ type: 'window', includeUncontrolled: true }).then(function (list) {
      var target;
      try { target = new URL(url, self.location.origin).href; } catch (e) { target = url; }
      for (var i = 0; i < list.length; i++) {
        if (list[i].url === target && 'focus' in list[i]) {
          return list[i].focus();
        }
      }
      if (self.clients.openWindow) {
        return self.clients.openWindow(target);
      }
    })
  );
});
