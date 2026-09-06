// What the app page shows about an account: one control, top right, and nothing else.
//
// No sign-in element mounts here. Its Google button navigates the whole window, and this
// page holds the download queue — a full-page navigation aborts every transfer in
// flight. So the control is a link to /login, which holds nothing, and that page is both
// the sign-in surface and the account view.
//
// This module exists only to reflect session state onto that link, so a signed-in
// visitor can tell at a glance without the page fetching anything it does not need.

import { OPENAPPS_BASE_URL } from "./lib/openapps";

/**
 * Mark the header's account control if there is a session.
 *
 * Reads the SDK's own storage rather than calling the server: this runs on every page
 * load, the answer only decides a tooltip and a colour, and a network round trip for
 * that would be spent on every visitor including the signed-out majority.
 *
 * `isLoggedIn` means "a session exists here", not "the server still honours it". That is
 * the right strength for a hint. /login asks the server properly, because there the
 * answer decides what to render.
 */
export async function reflectAccountState(
  link: HTMLElement | null,
): Promise<void> {
  if (!link) return;
  try {
    const mod = (await import(
      /* @vite-ignore */ new URL("../openapps/openapps-ui.js", import.meta.url)
        .href
    )) as {
      configure: (o: { baseUrl: string }) => void;
      getClient?: () => { isLoggedIn?: boolean } | null;
    };
    mod.configure({ baseUrl: OPENAPPS_BASE_URL });
    if (mod.getClient?.()?.isLoggedIn) {
      link.classList.add("signed-in");
      link.setAttribute("title", "Your account");
      link.setAttribute("aria-label", "Your account");
    }
  } catch {
    // The control still works — it is a plain link. Nothing here is worth a message:
    // the page's actual job is downloading, and none of it needs an account.
  }
}
