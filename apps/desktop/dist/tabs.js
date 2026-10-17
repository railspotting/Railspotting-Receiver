"use strict";
// The app calls setTab() when it switches views itself (from the menu, or a dashboard link).
function setTab(name) {
  for (const a of document.querySelectorAll(".tabs a")) {
    if (a.dataset.tab === name) a.setAttribute("aria-current", "page");
    else a.removeAttribute("aria-current");
  }
}
document.querySelector(".tabs").addEventListener("click", (e) => {
  const a = e.target.closest("a[data-tab]");
  if (a) setTab(a.dataset.tab);
});
