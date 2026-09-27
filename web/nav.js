// Injects a hamburger toggle into the shared top nav and wires the mobile dropdown.
(function () {
  var nav = document.querySelector('nav');
  if (!nav) return;
  var wrap = nav.querySelector('.wrap');
  var links = nav.querySelector('.navlinks');
  if (!wrap || !links) return;

  var btn = document.createElement('button');
  btn.className = 'navtoggle';
  btn.type = 'button';
  btn.setAttribute('aria-label', 'Menu');
  btn.setAttribute('aria-expanded', 'false');
  btn.innerHTML = '<span></span><span></span><span></span>';
  wrap.appendChild(btn);

  function close() {
    nav.classList.remove('open');
    btn.setAttribute('aria-expanded', 'false');
  }
  btn.addEventListener('click', function (e) {
    e.stopPropagation();
    var open = nav.classList.toggle('open');
    btn.setAttribute('aria-expanded', open ? 'true' : 'false');
  });
  links.addEventListener('click', function (e) { if (e.target.closest('a')) close(); });
  document.addEventListener('click', function (e) {
    if (nav.classList.contains('open') && !nav.contains(e.target)) close();
  });
  window.addEventListener('resize', function () { if (window.innerWidth > 860) close(); });
})();
