// Retrying fetches.
//
import { engineConfig } from "./config";

// A download that fails outright on one dropped connection is not a resumable
// downloader; it is a downloader with a manual retry button. Everything the
// engine fetches goes through here.
//
// What is *not* retried matters as much as what is: a 4xx other than 408/429 is
// a statement about the request, not about the network, and repeating it just
// burns the server's rate limit before failing anyway.

/** Attempts per request, including the first. */
const MAX_ATTEMPTS = 5;
/** First backoff step; doubles each attempt. */
const BASE_DELAY_MS = 500;
/** Ceiling per step, so a long stall does not become an unbounded wait. */
const MAX_DELAY_MS = 15_000;

export interface RetryOptions {
  headers?: Record<string, string>;
  signal?: AbortSignal;
  /** Defaults to GET. Site extractors need POST for JSON APIs. */
  method?: string;
  body?: string;
  /**
   * Treat `403` as a throttle rather than a refusal.
   *
   * Normally a 403 is a statement about the request that repeating will not change, so
   * retrying it just burns the server's patience. Platform media hosts are the exception:
   * Google's answer `403` once a URL has served a few large sequential reads, and the
   * same URL works again after a pause. Set for streams whose extractor said the host
   * rate-limits — see `Stream.max_chunk`.
   */
  retryForbidden?: boolean;
  /** Called before each wait, so the UI can say why it is stalled. */
  onRetry?: (attempt: number, delayMs: number, reason: string) => void;
}

/**
 * A fetch failure that looks like the browser blocking a cross-origin read.
 *
 * `fetch` deliberately refuses to say whether a request failed on DNS, on the
 * wire, or on CORS — so this can only ever be a guess, and it is worded as one
 * where it surfaces. It exists because "TypeError: Failed to fetch" is a
 * uselessly opaque thing to show a user of the web app, where CORS is by far
 * the most likely cause and a relay is the fix.
 */
export function looksLikeCorsFailure(e: unknown): boolean {
  return e instanceof TypeError;
}

function sleep(ms: number, signal?: AbortSignal): Promise<void> {
  return new Promise((resolve, reject) => {
    const timer = setTimeout(resolve, ms);
    signal?.addEventListener(
      "abort",
      () => {
        clearTimeout(timer);
        reject(new DOMException("aborted", "AbortError"));
      },
      { once: true },
    );
  });
}

/** Status codes worth trying again. */
/**
 * The host served the start of a file and then refused every later offset.
 *
 * Carried as its own type rather than recognised by its message, because the caller acts
 * on it: a download that ends this way cannot be resumed — the next attempt reaches the
 * same offset and is refused there too — so offering "Resume" on it is offering an action
 * that provably cannot succeed. Measured on Google's media addresses, which serve to
 * about 1.1 MB and refuse the rest.
 */
export class HostRefusedRemainder extends Error {
  constructor(message: string) {
    super(message);
    this.name = "HostRefusedRemainder";
  }
}

function isRetryableStatus(
  status: number,
  retryForbidden = false,
  headers?: Record<string, string>,
): boolean {
  // 408 request timeout, 429 too many requests, and the 5xx family are all
  // transient by definition. A 404 will not become a 200 by asking twice.
  if (status === 408 || status === 429 || (status >= 500 && status < 600))
    return true;
  // 403 is normally a statement about the request rather than about the moment — except
  // on a host that answers it when throttling, where waiting is exactly the right move.
  if (retryForbidden && status === 403) return true;
  // 412 means "your precondition did not hold", and the only preconditions this engine
  // sends are `If-Range` on a resume — where a 412 is real and repeating it is pointless.
  // Nothing here sends one on a page or manifest fetch, so a 412 on one of those is not
  // a precondition failure at all: it is a site using the code to turn away traffic it
  // dislikes. Bilibili answers roughly one watch-page request in three that way, and the
  // very next identical request succeeds.
  return status === 412 && !hasPrecondition(headers);
}

/** Whether the request carried a conditional header, making a 412 a real answer. */
function hasPrecondition(headers: Record<string, string> | undefined): boolean {
  if (!headers) return false;
  return Object.keys(headers).some((name) =>
    ["if-range", "if-match", "if-none-match", "if-unmodified-since"].includes(
      name.toLowerCase(),
    ),
  );
}

/**
 * Honour `Retry-After`, which may be either a delay in seconds or an HTTP date.
 *
 * A server that tells us exactly when to come back is more authoritative than
 * our backoff curve, so its answer wins — clamped, because a misconfigured
 * origin sending `Retry-After: 86400` should not hang the job for a day.
 */
function retryAfterMs(res: Response): number | null {
  const header = res.headers.get("retry-after");
  if (!header) return null;

  const seconds = Number(header);
  if (Number.isFinite(seconds)) return Math.min(seconds * 1000, MAX_DELAY_MS);

  const date = Date.parse(header);
  if (Number.isNaN(date)) return null;
  return Math.min(Math.max(date - Date.now(), 0), MAX_DELAY_MS);
}

function backoffMs(attempt: number): number {
  // Exponential with jitter. Without jitter, four parallel chunks that fail
  // together would retry in lockstep and hit the server as a thundering herd.
  const exponential = Math.min(BASE_DELAY_MS * 2 ** attempt, MAX_DELAY_MS);
  return Math.round(exponential * (0.5 + Math.random() * 0.5));
}

/**
 * `fetch` with bounded retries.
 *
 * Resolves with the response once it is non-retryable — which includes a plain
 * 404, so callers still check `res.ok` themselves. Rejects only when every
 * attempt failed or the request was aborted.
 */
export async function fetchWithRetry(
  url: string,
  options: RetryOptions = {},
): Promise<Response> {
  let lastError: unknown;
  /** Attempts actually made. Not the same as MAX_ATTEMPTS: the loop breaks early
   *  for a failure that repeating cannot fix, and saying "5 attempts" after one is a
   *  message that sends people looking for a flaky network. */
  let attempts = 0;
  // The host may route requests through a relay; it rewrites the URL rather
  // than wrapping fetch, so retry, range and abort behaviour stay identical.
  const target = engineConfig.rewriteUrl(url);

  for (let attempt = 0; attempt < MAX_ATTEMPTS; attempt++) {
    if (options.signal?.aborted) {
      throw new DOMException("aborted", "AbortError");
    }
    attempts = attempt + 1;
    try {
      const res = await fetch(target, {
        method: options.method ?? "GET",
        headers: options.headers,
        body: options.body,
        signal: options.signal,
      });
      if (
        !isRetryableStatus(res.status, options.retryForbidden, options.headers)
      ) {
        return res;
      }

      // Drain the body so the connection can be reused for the retry.
      await res.arrayBuffer().catch(() => undefined);
      lastError = new Error(`server returned ${res.status}`);

      if (attempt === MAX_ATTEMPTS - 1) break;
      const delay = retryAfterMs(res) ?? backoffMs(attempt);
      options.onRetry?.(attempt + 1, delay, `HTTP ${res.status}`);
      await sleep(delay, options.signal);
    } catch (e) {
      // An abort is the user's decision, not a failure to retry through.
      if ((e as { name?: string }).name === "AbortError") throw e;
      lastError = e;
      // A `TypeError` from `fetch` is the browser refusing to let this page read the
      // response — almost always CORS. Waiting changes nothing about that, and five
      // attempts with backoff turns an instant, explainable failure into fifteen seconds
      // of a spinner, which is how it looked to the first person who tried it.
      if (looksLikeCorsFailure(e)) break;
      if (attempt === MAX_ATTEMPTS - 1) break;
      const delay = backoffMs(attempt);
      options.onRetry?.(
        attempt + 1,
        delay,
        e instanceof Error ? e.message : "network error",
      );
      await sleep(delay, options.signal);
    }
  }

  const detail =
    lastError instanceof Error ? lastError.message : String(lastError);
  // A run of 403s from a host that throttles is worth naming, because the status alone
  // sends people looking for a permissions problem that is not there.
  if (options.retryForbidden && detail.includes("403")) {
    throw new HostRefusedRemainder(
      `${new URL(target).hostname} served the beginning of this file and refused the ` +
        "rest. YouTube now limits these addresses to about their first megabyte and " +
        "delivers the remainder over a different protocol, so the part that is missing " +
        "cannot be fetched this way. Nothing about the link, the permissions or the " +
        "connection changes it, and retrying will reach the same point again.",
    );
  }
  // A CORS-shaped failure is rethrown as the `TypeError` it was, not wrapped.
  //
  // Wrapping it in a plain `Error` is what made every downstream
  // `looksLikeCorsFailure()` check dead code: the web app has a specific, useful answer
  // for this case — run the relay, or use the extension — and it could never reach it,
  // so a YouTube link on a page with no relay reported "failed after 5 attempts: Failed
  // to fetch" and left the user with nothing to act on. The loop already breaks on the
  // first one, so there is no retry history worth preserving either.
  if (looksLikeCorsFailure(lastError)) throw lastError;

  throw new Error(
    `failed after ${attempts} attempt${attempts === 1 ? "" : "s"}: ${detail}`,
  );
}
