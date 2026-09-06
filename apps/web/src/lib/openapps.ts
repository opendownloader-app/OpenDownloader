/**
 * Every OpenApps hostname this product talks to, in one file.
 *
 * Defined once because the alternative has already cost this suite real time:
 * OpenCapture kept its backend URL as a literal at two call sites, so moving to a custom
 * domain fixed one and silently left the other pointing at the old host. A grep for
 * `openapps.network` anywhere outside this file must return nothing.
 *
 * # Why none of these say `openapps.network`
 *
 * The platform runs at `accounts.openapps.network`, and every product reaches it through
 * its own hostname instead. That is not decoration: a browser names the host it is about
 * to allow in its permission prompt, and "OpenDownloader wants to communicate with
 * gateway.openapps.network" reads like the app is phoning someone else's server — which,
 * to anyone who has never heard of OpenApps, is exactly what it looks like.
 *
 * What the masking does **not** hide, said plainly rather than overclaimed: Google
 * sign-in visibly bounces through `accounts.openapps.network` on the OAuth callback hop,
 * and a wallet signature prompt names that host too. The server builds both from its own
 * `public_url` once at startup, so no hostname added here changes them.
 */

/** The product site. */
export const SITE_ORIGIN = "https://opendownloader.app";

/** Where the web app is served. */
export const APP_ORIGIN = "https://app.opendownloader.app";

/** OpenApps accounts, credits and sign-in — the platform, under our own name. */
export const OPENAPPS_BASE_URL = "https://auth.opendownloader.app";

/**
 * Paid features, under our own name.
 *
 * OpenDownloader has none: every feature runs on the user's own machine, which costs
 * nothing to serve and so has nothing to meter. The constant exists anyway because the
 * host is provisioned alongside `auth.` — a product that later grows a paid route must
 * not have to add a hostname, a certificate and a permission prompt to ship it.
 */
export const OPENAPPS_GATEWAY_URL = "https://gateway.opendownloader.app";

/**
 * The platform's own sign-in page, for hosts that cannot run the flow themselves.
 *
 * The extension is one. Chrome refuses to land a cross-origin OAuth redirect on a
 * `chrome-extension://` page, and no wallet or Nostr signer is ever injected into one —
 * extensions inject those globals from content scripts, and content scripts do not run on
 * another extension's pages. So the buttons cannot work there however long you wait, and
 * the answer is to open this page in a tab instead of hosting sign-in ourselves.
 */
export const OPENAPPS_SIGNIN_URL = `${OPENAPPS_BASE_URL}/signin`;

/**
 * Whether signing in is required for anything here. It is not, and this constant exists
 * to keep that answer in one place rather than implied by the absence of checks.
 *
 * Every feature of OpenDownloader runs in the browser on the user's own machine. There is
 * no server doing work on their behalf, so there is nothing to charge for and nothing to
 * gate. An account carries credits across the *other* OpenApps tools; here it buys
 * nothing, and the UI must never imply otherwise.
 */
export const ACCOUNT_REQUIRED = false;
