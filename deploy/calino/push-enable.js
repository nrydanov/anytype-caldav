// Turns on server reminders inside Calino.
//
// nginx adds this script to Calino's index.html, so Calino itself is not
// modified. On iOS it uses Declarative Web Push (Safari, 18.4+): the page
// subscribes through window.pushManager and Safari shows the exporter's
// notifications without a service worker; that subscription belongs to the
// installed app, so there the button appears only when running as that app.
// Everywhere else (Chrome and Firefox, on Android and on a desktop) it
// subscribes through the service worker push-sw.js, which shows them.
(() => {
  const USERNAME = 'anytype';
  const DONE_KEY = 'anytype-push-registered';
  const DISMISSED_KEY = 'anytype-push-dismissed';

  const standalone =
    navigator.standalone === true || matchMedia('(display-mode: standalone)').matches;
  const declarative = 'pushManager' in window;
  const throughWorker = 'serviceWorker' in navigator && 'PushManager' in window;
  if (!('Notification' in window)) return;
  // `navigator.standalone` exists only on iOS, where a push reaches nothing
  // but the installed app.
  if (declarative || 'standalone' in navigator) {
    if (!declarative || !standalone) return;
  } else if (!throughWorker) {
    return;
  }

  // The offer is for a phone and for the installed app. In a tab on a desktop
  // it would hang over the calendar of everyone who only came to look; there
  // it appears when asked for, by opening the page with #reminders.
  const asked = location.hash === '#reminders';
  const handheld = matchMedia('(pointer: coarse)').matches;
  if (!asked && !standalone && !handheld) return;

  const dismissed = () => {
    try {
      return localStorage.getItem(DISMISSED_KEY) === '1';
    } catch {
      return false;
    }
  };

  // Where subscriptions are made: the window on iOS, a worker elsewhere.
  const pushManager = async () => {
    if (declarative) return window.pushManager;
    const registration = await navigator.serviceWorker.register('/push-sw.js', {
      scope: '/push-sw/',
    });
    return registration.pushManager;
  };

  const keyBytes = (base64url) => {
    const padded = base64url.replace(/-/g, '+').replace(/_/g, '/') +
      '='.repeat((4 - (base64url.length % 4)) % 4);
    return Uint8Array.from(atob(padded), (c) => c.charCodeAt(0));
  };

  const basic = (username, password) => {
    const bytes = new TextEncoder().encode(`${username}:${password}`);
    return 'Basic ' + btoa(String.fromCharCode(...bytes));
  };

  const registered = () => {
    try {
      return localStorage.getItem(DONE_KEY) === '1';
    } catch {
      return false;
    }
  };

  // The key is fetched before the tap: Safari may refuse to subscribe once
  // the user gesture has been spent on network waits.
  async function enable(button, publicKey) {
    button.disabled = true;
    try {
      if ((await Notification.requestPermission()) !== 'granted') {
        throw new Error('уведомления запрещены в настройках');
      }
      const manager = await pushManager();
      const subscription =
        (await manager.getSubscription()) ||
        (await manager.subscribe({
          userVisibleOnly: true,
          applicationServerKey: keyBytes(publicKey),
        }));
      // The same login Calino was given: a person's own one makes the
      // subscription theirs, and it then receives only their reminders.
      const username = prompt('Логин CalDAV', USERNAME);
      if (!username) throw new Error('логин не введён');
      const password = prompt('Пароль CalDAV, чтобы включить напоминания');
      if (!password) throw new Error('пароль не введён');
      const authorization = basic(username.trim(), password);
      const saved = await fetch('/push/subscribe', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json', Authorization: authorization },
        body: JSON.stringify(subscription),
      });
      if (saved.status === 401) throw new Error('неверный пароль');
      if (!saved.ok) throw new Error(`подписка: HTTP ${saved.status}`);
      try {
        localStorage.setItem(DONE_KEY, '1');
      } catch {}
      await fetch('/push/test', { method: 'POST', headers: { Authorization: authorization } });
      button.remove();
      alert('Напоминания включены. Отправлено тестовое уведомление.');
    } catch (error) {
      button.disabled = false;
      alert(`Не удалось включить напоминания: ${error.message}`);
    }
  }

  async function offer() {
    // Looking for an existing subscription must not register a worker in a
    // browser whose owner never asked for reminders.
    let subscription = null;
    if (declarative) {
      subscription = await window.pushManager.getSubscription();
    } else {
      const registration = await navigator.serviceWorker.getRegistration('/push-sw/');
      subscription = registration ? await registration.pushManager.getSubscription() : null;
    }
    if (subscription && Notification.permission === 'granted' && registered()) return;
    // "Not now" is remembered; #reminders brings the offer back.
    if (dismissed() && !asked) return;
    const keyResponse = await fetch('/push/key');
    if (!keyResponse.ok) return;
    const { publicKey } = await keyResponse.json();
    const button = document.createElement('button');
    button.type = 'button';
    button.textContent = '🔔 Напоминания';
    button.setAttribute('aria-label', 'Включить напоминания Anytype');
    Object.assign(button.style, {
      position: 'fixed',
      right: '16px',
      bottom: 'calc(env(safe-area-inset-bottom, 0px) + 88px)',
      zIndex: '2147483647',
      padding: '10px 14px',
      borderRadius: '999px',
      border: 'none',
      background: '#2563eb',
      color: '#fff',
      font: '600 14px -apple-system, system-ui, sans-serif',
      boxShadow: '0 4px 14px rgba(0,0,0,.25)',
    });
    button.addEventListener('click', () => enable(button, publicKey));

    const close = document.createElement('span');
    close.textContent = '×';
    close.setAttribute('role', 'button');
    close.setAttribute('aria-label', 'Не сейчас');
    Object.assign(close.style, {
      marginLeft: '10px',
      padding: '0 2px',
      fontSize: '18px',
      lineHeight: '1',
      opacity: '0.8',
      cursor: 'pointer',
    });
    close.addEventListener('click', (event) => {
      event.stopPropagation();
      try {
        localStorage.setItem(DISMISSED_KEY, '1');
      } catch {}
      button.remove();
    });
    button.appendChild(close);
    document.body.appendChild(button);
  }

  if (document.readyState === 'complete') offer();
  else window.addEventListener('load', offer);
})();
