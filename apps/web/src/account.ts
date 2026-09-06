// The account surface: sign in, see your balance, see where credits went.
//
// Deliberately not a gate. Every feature of OpenDownloader runs in this tab, on the
// user's own machine, so there is nothing here to meter and nothing to withhold — an
// account exists so that one OpenApps identity and its credits work across the *other*
// tools in the suite. The elements below are therefore additive: signed out, the page
// works exactly as it always did.

import { OPENAPPS_BASE_URL } from "./lib/openapps";

/**
 * Load the OpenApps elements and point them at our own hostname.
 *
 * The bundle is vendored under `public/openapps/` rather than imported as a package:
 * `@openapps/ui` is not published to npm, and this app is its own repository, so it
 * cannot reach up into the monorepo for a workspace dependency. Same approach OpenTabs
 * takes.
 *
 * Loaded on demand rather than at startup. It is ~360 KB of Lit and crypto helpers for
 * a panel most people never open, and the download queue should not wait on it.
 */
let loaded: Promise<void> | null = null;

function loadElements(): Promise<void> {
  loaded ??= (async () => {
    // The elements read their configuration from a module-level client, so `configure`
    // has to run before the first element upgrades — hence the import, then configure,
    // then render order below.
    const mod = (await import(
      /* @vite-ignore */ new URL("./openapps/openapps-ui.js", document.baseURI)
        .href
    )) as { configure: (o: { baseUrl: string }) => void };
    mod.configure({ baseUrl: OPENAPPS_BASE_URL });
  })();
  return loaded;
}

/**
 * Mount the account panel.
 *
 * Returns without doing anything if the host page has no slot for it, so the extension's
 * manager — which shares this codebase but has no account surface — is unaffected.
 */
export async function mountAccountPanel(
  root: HTMLElement | null,
): Promise<void> {
  if (!root) return;

  const body = document.createElement("div");
  body.className = "stack";
  body.innerHTML = `
    <p class="muted" style="margin:0">
      Signing in is optional and unlocks nothing here — every feature works signed out.
      An OpenApps account carries your credits across the other tools in the suite.
    </p>
    <openapps-login></openapps-login>
    <openapps-credits poll-seconds="30"></openapps-credits>
    <openapps-history page-size="5"></openapps-history>
  `;

  const status = document.createElement("p");
  status.className = "muted";
  status.textContent = "Loading the account panel…";
  root.replaceChildren(status);

  try {
    await loadElements();
    root.replaceChildren(body);
  } catch (e) {
    // A failure here must not read as "you are signed out" — that is a different fact,
    // and the difference matters to someone deciding whether their credits are safe.
    status.className = "status-error";
    status.textContent =
      "The account panel could not load. Downloads are unaffected — nothing here needs " +
      "an account. " +
      (e instanceof Error ? e.message : String(e));
  }
}
