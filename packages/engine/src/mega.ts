// Fetching a file from a Mega link.
//
// Three steps, and the middle one is the reason this can live in a page at all:
//
//   1. the link is parsed by the core — key, nonce and MAC come out of the URL fragment,
//      which the browser never sends, so Mega has never seen the key and neither have we;
//   2. `g.api.mega.co.nz` is asked for a temporary download URL. Its API answers
//      `Access-Control-Allow-Origin: *`, so this needs no relay;
//   3. the bytes are AES-128-CTR, decrypted chunk by chunk on the way to disk.
//
// The filename is not in the link either. It is in an encrypted attribute blob the same
// key opens — AES-128-CBC with a zero IV, plaintext beginning `MEGA` — so even the name
// is something the reader derives rather than the server volunteering.

import { loadCore } from "./wasm";

/** Where Mega's client API lives. The `.co.nz` host is the one its own web client uses. */
const MEGA_API = "https://g.api.mega.co.nz/cs";

export interface MegaFile {
  /** Temporary, single-use-ish URL for the ciphertext. */
  url: string;
  /** Plaintext length in bytes. */
  size: number;
  filename: string;
  /** Base64, for `Job.decrypt`. */
  key: string;
  nonce: string;
}

/** Mega's error codes are negative numbers in place of an object. */
function megaError(code: number): string {
  switch (code) {
    case -2:
      return "Mega rejected that request as malformed — the link may be truncated.";
    case -9:
      return "Mega has no such file. The link is wrong, or the file was deleted.";
    case -11:
      return "Mega refused access to that file.";
    case -16:
      return "That Mega account is suspended.";
    case -17:
      return "This file is over Mega's transfer quota for now. Free transfers are capped per IP, and it clears after a few hours.";
    default:
      return `Mega refused this download (code ${code}).`;
  }
}

function base64UrlToBytes(value: string): Uint8Array<ArrayBuffer> {
  const padded = value.replace(/-/g, "+").replace(/_/g, "/");
  const binary = atob(padded + "=".repeat((4 - (padded.length % 4)) % 4));
  return Uint8Array.from(binary, (c) => c.charCodeAt(0));
}

function bytesToBase64(bytes: Uint8Array): string {
  let s = "";
  for (const b of bytes) s += String.fromCharCode(b);
  return btoa(s);
}

/**
 * Decrypt Mega's attribute blob to recover the filename.
 *
 * AES-128-CBC with an all-zero IV. The awkward part is that Mega's blob carries no
 * PKCS#7 padding, and WebCrypto's CBC always expects some — it will reject the whole
 * thing rather than hand back the plaintext.
 *
 * The fix is to give it padding it will accept. Appending one block that decrypts to
 * sixteen 0x10 bytes makes the padding valid, and WebCrypto then strips exactly that
 * block and returns the real plaintext. Building it needs a raw ECB encryption, which
 * WebCrypto does not expose — but the first block of a CBC encryption with a zero IV
 * *is* the ECB encryption of that block, which is the whole trick.
 *
 * The plaintext must begin with the literal `MEGA`. That prefix is the check that the
 * key was right: without it a wrong key yields plausible bytes and a garbage filename,
 * which is a worse failure than saying the name could not be read.
 */
async function filenameFrom(
  at: string,
  keyBytes: Uint8Array<ArrayBuffer>,
): Promise<string | null> {
  const cipher = base64UrlToBytes(at);
  if (cipher.length < 16 || cipher.length % 16 !== 0) return null;

  const decryptKey = await crypto.subtle.importKey(
    "raw",
    keyBytes,
    "AES-CBC",
    false,
    ["decrypt"],
  );
  const encryptKey = await crypto.subtle.importKey(
    "raw",
    keyBytes,
    "AES-CBC",
    false,
    ["encrypt"],
  );

  // A block that will decrypt, in CBC, to sixteen 0x10 bytes — i.e. valid padding.
  const last = cipher.subarray(cipher.length - 16);
  const wanted = new Uint8Array(16);
  for (let i = 0; i < 16; i++) wanted[i] = 0x10 ^ last[i]!;
  const encrypted = new Uint8Array(
    await crypto.subtle.encrypt(
      { name: "AES-CBC", iv: new Uint8Array(16) },
      encryptKey,
      wanted,
    ),
  );

  const padded = new Uint8Array(cipher.length + 16);
  padded.set(cipher, 0);
  padded.set(encrypted.subarray(0, 16), cipher.length);

  let plain: Uint8Array;
  try {
    plain = new Uint8Array(
      await crypto.subtle.decrypt(
        { name: "AES-CBC", iv: new Uint8Array(16) },
        decryptKey,
        padded,
      ),
    );
  } catch {
    return null;
  }

  const text = new TextDecoder().decode(plain).replace(/\0+$/, "");
  if (!text.startsWith("MEGA")) return null;
  try {
    return (JSON.parse(text.slice(4)) as { n?: string }).n ?? null;
  } catch {
    return null;
  }
}

/**
 * Resolve a Mega file link into something downloadable.
 *
 * Throws with Mega's own reason where it gives one — quota in particular, which is a
 * wait rather than a fault and should not read like a broken link.
 */
export async function resolveMegaFile(url: string): Promise<MegaFile> {
  const core = await loadCore();
  if (core.is_mega_folder_link(url)) {
    throw new Error(
      "That is a Mega folder link. This reads single files — open the folder and copy " +
        "the link for the file you want.",
    );
  }
  const parsed = core.parse_mega_link(url);
  if (!parsed) {
    throw new Error("That does not look like a Mega file link.");
  }
  const link = JSON.parse(parsed) as {
    handle: string;
    key: string;
    nonce: string;
    metaMac: string;
  };

  const response = await fetch(`${MEGA_API}?id=${Date.now() % 1e6}`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify([{ a: "g", g: 1, p: link.handle }]),
  });
  if (!response.ok) {
    // Mega has begun demanding a proof-of-work token on some requests, and announces
    // it with this header. Computing one is not implemented here, so say that plainly
    // rather than reporting a bare status that reads like an outage.
    if (response.headers.get("x-hashcash")) {
      throw new Error(
        "Mega asked this download to solve a proof-of-work challenge, which " +
          "OpenDownloader does not implement yet.",
      );
    }
    throw new Error(`Mega's API answered ${response.status}.`);
  }
  const body = (await response.json()) as unknown;
  // The API answers a bare negative number, or an array whose entries may be one.
  if (typeof body === "number") throw new Error(megaError(body));
  const first = Array.isArray(body) ? body[0] : body;
  if (typeof first === "number") throw new Error(megaError(first));

  const node = first as { g?: string; s?: number; at?: string };
  if (!node.g || typeof node.s !== "number") {
    throw new Error("Mega did not return a download URL for that file.");
  }

  const keyBytes = Uint8Array.from(atob(link.key), (c) => c.charCodeAt(0));
  const name = node.at ? await filenameFrom(node.at, keyBytes) : null;

  return {
    url: node.g,
    size: node.s,
    // A file whose name could not be recovered still downloads; naming it after the
    // handle is better than refusing, and better than a name that is silently wrong.
    filename: name ?? `mega-${link.handle}.bin`,
    key: link.key,
    nonce: link.nonce,
  };
}

/** Whether this URL is a Mega link of any kind, file or folder. */
export async function isMegaLink(url: string): Promise<boolean> {
  const core = await loadCore();
  return (
    core.parse_mega_link(url) !== undefined || core.is_mega_folder_link(url)
  );
}

export { bytesToBase64 };
