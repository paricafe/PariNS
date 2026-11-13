// Applies browser-local appearance preferences before the first paint.
(function () {
  var root = document.documentElement;
  var theme = null; var background = null;
  try { theme = localStorage.getItem('parins-theme'); background = localStorage.getItem('parins-background'); } catch (error) { }
  var dark = theme === 'dark' || (theme !== 'light' && matchMedia('(prefers-color-scheme: dark)').matches);
  root.classList.toggle('dark', dark);
  if (background && /^[a-z]+$/.test(background)) root.dataset.bg = background;
})();
