// The sign-in page, and why it is a page.
//
// `<openapps-login>`'s Google button navigates the whole window to
// accounts.google.com and back. On the app page that would take the download queue with
// it: transfers run in the tab, so a full-page navigation aborts every one in flight.
// Resume state survives in IndexedDB, but the bytes in the air do not, and a sign-in
// that silently interrupts your downloads is not a sign-in anyone would choose.
//
// So sign-in lives here, on a page holding nothing, and the app links to it. openpdfedit
// reaches the same conclusion from the same constraint — it gives sign-in its own
// window rather than putting it in the editor.

import { OPENAPPS_BASE_URL } from "./lib/openapps";

/** Where to go once there is a session. Same-origin only — this is attacker-supplied. */
function returnTarget(): string {
  const asked = new URLSearchParams(location.search).get("next");
  if (!asked) return "./";
  try {
    const url = new URL(asked, location.origin);
    // An open redirect on a sign-in page hands someone's fresh session to whoever
    // crafted the link. Only this origin, and only a path.
    if (url.origin !== location.origin) return "./";
    return url.pathname + url.search + url.hash;
  } catch {
    return "./";
  }
}

async function main(): Promise<void> {
  const slot = document.getElementById("login-slot");
  if (!slot) return;

  const status = document.createElement("p");
  status.className = "muted";
  status.textContent = "Loading…";
  slot.replaceChildren(status);

  const mod = (await import(
    /* @vite-ignore */ new URL("../openapps/openapps-ui.js", import.meta.url)
      .href
  )) as {
    configure: (o: { baseUrl: string }) => void;
    getClient?: () => { isLoggedIn?: boolean } | null;
  };
  mod.configure({ baseUrl: OPENAPPS_BASE_URL });

  const panel = document.createElement("openapps-login");
  panel.setAttribute("variant", "panel");
  panel.setAttribute("mark", "D");
  panel.setAttribute("heading", "Sign in to OpenDownloader");
  panel.setAttribute(
    "description",
    "One account across our apps. You do not need it here — every feature works without it.",
  );
  slot.replaceChildren(panel);

  // The element fires this once a session exists, whichever method produced it. Google
  // arrives back as a page load rather than an event, which the check below catches.
  slot.addEventListener("openapps-login", () => {
    location.replace(returnTarget());
  });

  // Returning from the Google redirect: the element completes it during its own load, so
  // give it a moment and then look, rather than racing it.
  const client = mod.getClient?.();
  if (client?.isLoggedIn) location.replace(returnTarget());
}

void main().catch((e) => {
  const slot = document.getElementById("login-slot");
  if (slot) {
    slot.textContent =
      "The sign-in panel could not load. Downloads are unaffected — nothing needs an " +
      "account. " +
      (e instanceof Error ? e.message : String(e));
    slot.className = "status-error";
  }
});
