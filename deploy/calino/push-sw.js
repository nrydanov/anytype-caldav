// Shows the exporter's reminders in browsers that have no Declarative Web Push:
// Chrome and Firefox, on Android and on a desktop. Safari on iOS shows the same
// payload by itself and never loads this file.
//
// Registered by push-enable.js under the scope /push-sw/, which holds no page:
// a push reaches a worker whatever its scope, and Calino's own service worker,
// if it is ever turned on, keeps the scope / to itself.
self.addEventListener('install', () => self.skipWaiting());
self.addEventListener('activate', (event) => event.waitUntil(self.clients.claim()));

self.addEventListener('push', (event) => {
  let payload = {};
  try {
    payload = event.data ? event.data.json() : {};
  } catch {
    payload = { title: 'Anytype', body: event.data ? event.data.text() : '' };
  }
  // The declarative envelope carries the page to open; the flat fields are the
  // same title and body.
  const shown = payload.notification || payload;
  event.waitUntil(
    self.registration.showNotification(shown.title || 'Anytype', {
      body: shown.body || '',
      tag: shown.tag || payload.tag,
      icon: '/apple-touch-icon.png',
      badge: '/favicon-96x96.png',
      data: { url: shown.navigate || payload.url || '/' },
    }),
  );
});

self.addEventListener('notificationclick', (event) => {
  event.notification.close();
  const url = (event.notification.data && event.notification.data.url) || '/';
  event.waitUntil(
    self.clients.matchAll({ type: 'window', includeUncontrolled: true }).then((windows) => {
      const open = windows.find((client) => new URL(client.url).origin === self.location.origin);
      if (open) return open.focus().then((client) => client.navigate(url)).catch(() => undefined);
      return self.clients.openWindow(url);
    }),
  );
});
