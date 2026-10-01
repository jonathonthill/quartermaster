// Light or dark: the choice in Settings > Appearance (saved on this computer),
// or the system's setting. Loaded before the stylesheet so the window never
// shows the wrong colors first.
function appearanceChoice() {
  try {
    return localStorage.getItem('appearance') || 'system';
  } catch {
    return 'system';
  }
}

function applyAppearance() {
  const choice = appearanceChoice();
  const dark = choice === 'dark' || (choice === 'system' && matchMedia('(prefers-color-scheme: dark)').matches);
  document.documentElement.dataset.theme = dark ? 'dark' : 'light';
}

applyAppearance();
matchMedia('(prefers-color-scheme: dark)').addEventListener('change', applyAppearance);
