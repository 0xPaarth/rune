const DAEMON_URL = 'http://127.0.0.1:9191/api/cf/ingest';
const TOKEN_KEY = 'daemonToken';

const CF_PROBLEM_PATTERN =
  /^https?:\/\/codeforces\.com\/(problemset\/problem\/\d+\/[^/]+|contest\/\d+\/problem\/[^/]+)/;

const statusEl = document.getElementById('status');
const buttonEl = document.getElementById('ingest-btn');
const gearEl = document.getElementById('gear');

const DEFAULT_BTN_TEXT = 'Ingest Problem';

function setStatus(text, kind = '') {
  statusEl.textContent = text;
  statusEl.className = kind;
}

function setButton({ text, state = 'idle', spinner = false, disabled = false }) {
  buttonEl.disabled = disabled;
  buttonEl.classList.remove('success', 'error');
  if (state === 'success') buttonEl.classList.add('success');
  if (state === 'error') buttonEl.classList.add('error');
  buttonEl.innerHTML = spinner
    ? `<span class="spinner"></span><span>${text}</span>`
    : text;
}

function resetButton() {
  setButton({ text: DEFAULT_BTN_TEXT, state: 'idle', disabled: false });
}

gearEl.addEventListener('click', () => {
  if (chrome.runtime.openOptionsPage) {
    chrome.runtime.openOptionsPage();
  } else {
    window.open(chrome.runtime.getURL('options.html'));
  }
});

async function loadToken() {
  const { [TOKEN_KEY]: token } = await chrome.storage.local.get(TOKEN_KEY);
  return token || null;
}

async function init() {
  const token = await loadToken();
  if (!token) {
    setStatus('Token not set. Click gear icon to set it.', 'warn');
    buttonEl.disabled = true;
  } else {
    setStatus('Ready');
  }
}

buttonEl.addEventListener('click', async () => {
  const token = await loadToken();
  if (!token) {
    setStatus('Token not set. Click gear icon to set it.', 'warn');
    return;
  }

  // Disable immediately to prevent double-clicks.
  setButton({ text: 'Ingesting...', state: 'idle', spinner: true, disabled: true });
  setStatus('Scraping page...');

  try {
    const [tab] = await chrome.tabs.query({ active: true, currentWindow: true });

    if (!tab?.url || !CF_PROBLEM_PATTERN.test(tab.url)) {
      setStatus('Not a Codeforces problem page.', 'error');
      setButton({ text: '✗ Failed (Check Token)', state: 'error', disabled: true });
      setTimeout(resetButton, 2000);
      return;
    }

    // Inject the scraper into the page. Its return value (the payload)
    // comes back in the injection result — no message passing needed.
    const results = await chrome.scripting.executeScript({
      target: { tabId: tab.id },
      files: ['scraper.js'],
    });

    const payload = results?.[0]?.result;
    if (!payload) {
      setStatus('Scrape failed: no payload returned.', 'error');
      setButton({ text: '✗ Failed (Check Token)', state: 'error', disabled: true });
      setTimeout(resetButton, 2000);
      return;
    }
    if (payload.error) {
      setStatus(`Scrape failed: ${payload.error}`, 'error');
      setButton({ text: '✗ Failed (Check Token)', state: 'error', disabled: true });
      setTimeout(resetButton, 2000);
      return;
    }

    setStatus(`Sending "${payload.name}" to daemon...`);

    const response = await fetch(DAEMON_URL, {
      method: 'POST',
      headers: {
        'Content-Type': 'application/json',
        'Authorization': `Bearer ${token}`,
      },
      body: JSON.stringify(payload),
    });

    if (!response.ok) {
      const body = await response.text().catch(() => '');
      setStatus(`Daemon error (${response.status}): ${body}`, 'error');
      setButton({ text: '✗ Failed (Check Token)', state: 'error', disabled: true });
      setTimeout(resetButton, 2000);
      return;
    }

    const data = await response.json();
    setStatus(`Workspace created:\n${data.path}`, 'success');
    setButton({ text: '✓ Ingested!', state: 'success', disabled: true });
    // Auto-close so the user lands back in their editor.
    setTimeout(() => window.close(), 1500);
  } catch (err) {
    // fetch throws TypeError when the daemon isn't running
    setStatus(`Error: ${err.message}\nIs the daemon running on :9191?`, 'error');
    setButton({ text: '✗ Failed (Check Token)', state: 'error', disabled: true });
    setTimeout(resetButton, 2000);
  }
});

init();
