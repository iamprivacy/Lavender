// Lavender start-up splash. A classic script loaded straight from index.html,
// right after the empty #splash element, so the splash is on screen with the
// window's first frame while the app bundle loads and the library opens.
// The app calls window.lavenderSplash.hide() once its first screen is
// mounted; the splash then plays its exit and removes itself.
;(function () {
  var root = document.getElementById('splash')
  if (!root) return

  // Counted from now, when the splash is first drawn, not from when the
  // window opened: a slow start-up before the page loads must not eat into
  // the time the intro is on screen.
  var shownAt = performance.now()

  var WORD = 'Lavender'
  var TAGLINE = 'Words for your sounds'
  var STATUS = 'Opening your library…'
  var BUDS = 26
  // A fast start still shows the name rising in, instead of a flash.
  var MIN_SHOWN_MS = 2500
  // The exit's longest piece (the buds flying off) in splash.css.
  var EXIT_MS = 970
  // If the app never says it is ready (a start-up error), the splash steps
  // aside so whatever the app did draw is visible.
  var GIVE_UP_MS = 30000

  var letters = ''
  for (var l = 0; l < WORD.length; l++) {
    letters += '<span style="--i:' + l + '">' + WORD[l] + '</span>'
  }

  // Fixed pseudo-random spread, so every start looks the same.
  var buds = ''
  for (var i = 0; i < BUDS; i++) {
    var x = (i * 37) % 100
    var dur = 7 + ((i * 13) % 6)
    var delay = -((i * 1.7) % dur)
    var size = 0.9 + ((i * 7) % 5) * 0.25
    var colour = i % 3 ? 'var(--splash-bud)' : 'var(--splash-bud-light)'
    // Where this bud flies on exit: outward from the centre line, with spread.
    var dx = ((x - 50) * 0.9 + (((i * 29) % 21) - 10) * 1.5).toFixed(1)
    var dy = ((((i * 17) % 23) - 11) * 2.2).toFixed(1)
    var rot = ((i * 47) % 140) - 70
    buds +=
      '<span class="splash-bud" style="--x:' +
      x +
      '%;--dur:' +
      dur +
      's;--delay:' +
      delay +
      's">' +
      '<b style="--dx:' +
      dx +
      'vw;--dy:' +
      dy +
      'vh;--rot:' +
      rot +
      'deg">' +
      '<i style="--s:' +
      size +
      'vmin;--c:' +
      colour +
      '"></i></b></span>'
  }

  root.innerHTML =
    '<div class="splash-buds">' +
    buds +
    '</div>' +
    '<div class="splash-word">' +
    letters +
    '</div>' +
    '<div class="splash-tag">' +
    TAGLINE +
    '</div>' +
    '<div class="splash-status">' +
    STATUS +
    '</div>'

  var hiding = false
  function hide() {
    if (hiding) return
    hiding = true
    var wait = Math.max(0, MIN_SHOWN_MS - (performance.now() - shownAt))
    setTimeout(function () {
      requestAnimationFrame(function () {
        root.classList.add('exiting')
        setTimeout(function () {
          root.remove()
        }, EXIT_MS)
      })
    }, wait)
  }

  setTimeout(hide, GIVE_UP_MS)
  window.lavenderSplash = { hide: hide }
})()
