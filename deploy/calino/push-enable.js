// Turns on server reminders inside the Calino Home Screen app.
//
// nginx adds this script to Calino's index.html, so Calino itself is not
// modified. It uses Declarative Web Push (Safari, iOS 18.4+): the page
// subscribes through window.pushManager and Safari shows the exporter's
// notifications without a service worker. The subscription belongs to the
// installed app, so the button appears only when running as that app.
(() => {
  const USERNAME = 'anytype';
  const DONE_KEY = 'anytype-push-registered';

  const standalone =
    navigator.standalone === true || matchMedia('(display-mode: standalone)').matches;
  if (!standalone || !('pushManager' in window) || !('Notification' in window)) return;

  const keyBytes = (base64url) => {
    const padded = base64url.replace(/-/g, '+').replace(/_/g, '/') +
      '='.repeat((4 - (base64url.length % 4)) % 4);
    return Uint8Array.from(atob(padded), (c) => c.charCodeAt(0));
  };

  const basic = (password) => {
    const bytes = new TextEncoder().encode(`${USERNAME}:${password}`);
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
      const subscription =
        (await window.pushManager.getSubscription()) ||
        (await window.pushManager.subscribe({
          userVisibleOnly: true,
          applicationServerKey: keyBytes(publicKey),
        }));
      const password = prompt('Пароль CalDAV, чтобы включить напоминания');
      if (!password) throw new Error('пароль не введён');
      const authorization = basic(password);
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
    const subscription = await window.pushManager.getSubscription();
    if (subscription && Notification.permission === 'granted' && registered()) return;
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
    document.body.appendChild(button);
  }

  if (document.readyState === 'complete') offer();
  else window.addEventListener('load', offer);
})();
