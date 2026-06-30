// HOLDFAST Klaxon — minimal Web Push display service worker.
// Registered by the inbox page so the browser will accept a PushManager subscription. On a push
// message it shows a notification; clicking it focuses/open the inbox. Klaxon itself stores
// subscriptions and records delivery intent; the encrypted server-side send is a deferred
// refinement, but this worker makes the browser-side registration a real, end-to-end flow.

self.addEventListener('install', function (event) {
  self.skipWaiting();
});

self.addEventListener('activate', function (event) {
  event.waitUntil(self.clients.claim());
});

self.addEventListener('push', function (event) {
  var payload = { title: 'New notification', body: '', url: '/' };
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
      data: { url: payload.url || '/' },
      icon: undefined,
      badge: undefined
    })
  );
});

self.addEventListener('notificationclick', function (event) {
  event.notification.close();
  var url = (event.notification.data && event.notification.data.url) || '/';
  event.waitUntil(
    self.clients.matchAll({ type: 'window', includeUncontrolled: true }).then(function (list) {
      for (var i = 0; i < list.length; i++) {
        if ('focus' in list[i]) {
          list[i].focus();
          return;
        }
      }
      if (self.clients.openWindow) {
        return self.clients.openWindow(url);
      }
    })
  );
});
