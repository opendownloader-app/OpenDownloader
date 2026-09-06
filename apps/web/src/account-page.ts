// The account page: sign in, balance, buy credits, and where they went.
//
// It is a page of its own rather than a panel on the app, and the reason is mechanical.
// `<openapps-login>`'s Google button navigates the whole window to
// accounts.google.com and back. On the app page that would take the download queue with
// it: transfers run in the tab, so a full-page navigation aborts every one in flight.
// Resume state survives in IndexedDB, but the bytes in the air do not, and a sign-in
// that silently interrupts your downloads is not a sign-in anyone would choose.
//
// So it lives here, on a page holding nothing, reached from the account control in the
// app's header. openpdfedit reaches the same conclusion from the same constraint — it
// gives sign-in its own window rather than putting it in the editor.
//
// One page serves both states: `<openapps-login>` renders a signed-in view of its own,
// so there is no separate "logged in" route and nothing to redirect between.

import { OPENAPPS_BASE_URL } from "./lib/openapps";

async function main(): Promise<void> {
  const slot = document.getElementById("account-slot");
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
  // No redirect. The element re-renders itself as the signed-in view, and this page is
  // where someone signing in wanted to end up — bouncing them elsewhere would take away
  // the balance and history they just came to see.

  // Deliberately no "already signed in, bounce them away" check. This page is also the
  // account view — the element renders a signed-in state of its own — and redirecting on
  // load would make that view unreachable for exactly the people it is for. The Google
  // return is covered by the event above, which the element emits once it has exchanged
  // the code.
  // Balance, buying and history are mounted only once there is a session.
  //
  // Each of these renders its own signed-out placeholder — "Sign in to buy credits.",
  // "Sign in to see where your credits went." — so mounting all three unconditionally
  // stacks three restatements of the sentence already on the card above them. Signed
  // out, the card is the whole page; signed in, these are.
  const details = document.createElement("div");
  details.className = "stack";

  function showDetails(): void {
    if (details.childElementCount > 0) return;
    const credits = document.createElement("openapps-credits");
    credits.setAttribute("poll-seconds", "30");
    // Buying belongs here rather than on the app page: it is the one thing an account is
    // actually for, since nothing in OpenDownloader spends credits. The element renders
    // one button per rail the *server* reports as enabled, so it stays correct without
    // this app knowing anything about payments.
    const buy = document.createElement("openapps-buy");
    const history = document.createElement("openapps-history");
    history.setAttribute("page-size", "5");
    details.append(credits, buy, history);
  }

  if (mod.getClient?.()?.isLoggedIn) showDetails();
  slot.addEventListener("openapps-login", showDetails);
  slot.append(details);
}

void main().catch((e) => {
  const slot = document.getElementById("account-slot");
  if (slot) {
    slot.textContent =
      "The sign-in panel could not load. Downloads are unaffected — nothing needs an " +
      "account. " +
      (e instanceof Error ? e.message : String(e));
    slot.className = "status-error";
  }
});
