const TOKEN_KEY = 'daemonToken';

const tokenEl = document.getElementById('token');
const saveEl = document.getElementById('save');
const toggleEl = document.getElementById('toggle');
const statusEl = document.getElementById('status');

function setStatus(text, kind = '') {
  statusEl.textContent = text;
  statusEl.className = kind;
}

async function loadToken() {
  const { [TOKEN_KEY]: token } = await chrome.storage.local.get(TOKEN_KEY);
  if (token) {
    tokenEl.value = token;
    setStatus('Loaded');
  } else {
    setStatus('Not set');
  }
}

saveEl.addEventListener('click', async () => {
  const token = tokenEl.value.trim();
  if (!token) {
    await chrome.storage.local.remove(TOKEN_KEY);
    setStatus('Cleared', 'success');
    return;
  }
  await chrome.storage.local.set({ [TOKEN_KEY]: token });
  setStatus('Saved ✓', 'success');
});

toggleEl.addEventListener('click', () => {
  const showing = tokenEl.type === 'text';
  tokenEl.type = showing ? 'password' : 'text';
  toggleEl.textContent = showing ? 'Show' : 'Hide';
});

tokenEl.addEventListener('keydown', (e) => {
  if (e.key === 'Enter') saveEl.click();
});

loadToken();
