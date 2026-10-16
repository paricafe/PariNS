// Appearance preferences are presentation-only (see src/appearance/preferences.ts).
// Never store credentials or console data here. Applied before first paint to avoid a flash.
(function () {
  var root = document.documentElement;
  var theme = null; var background = null;
  try { theme = localStorage.getItem('parins-theme'); background = localStorage.getItem('parins-background'); } catch (error) { /* storage unavailable */ }
  var dark = theme === 'dark' || (theme !== 'light' && matchMedia('(prefers-color-scheme: dark)').matches);
  root.classList.toggle('dark', dark);
  if (background && /^[a-z]+$/.test(background)) root.dataset.bg = background;
})();
