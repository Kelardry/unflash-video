// What the page keeps in this browser between visits (localStorage): the
// settings, what's new seen, the tours done. Storage can be blocked (a
// private window, the site's data turned off) or full, and the page goes on
// without it: a read finds nothing, a write lasts the visit. A module of its
// own, so that the modules app.js imports can use it (they cannot import
// app.js, which imports them).

/** What this browser keeps under `key`, or null (nothing kept, or storage blocked). */
export function stored(key) {
  try {
    return localStorage.getItem(key);
  } catch (e) {
    return null;
  }
}

/** Keep `value` under `key` in this browser (with storage blocked, it lasts the visit). */
export function store(key, value) {
  try {
    localStorage.setItem(key, value);
  } catch (e) {
    /* storage blocked, or full */
  }
}
