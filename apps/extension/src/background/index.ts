// The service worker.
//
// Its entire job is detection. It does not download anything: an MV3 worker is
// terminated after ~30s idle, after 5 minutes on any single request, and if a
// `fetch()` response takes more than 30s to arrive — all three of which a real
// download violates routinely. The engine lives in the manager tab instead,
// which is an ordinary document with none of those limits and which behaves
// identically on Chrome, Edge and Firefox.

import { ext } from "../platform/webext";
import type { PopupRequest, PopupResponse } from "../shared/messages";
import { attachSniffer, clearCandidates, listCandidates } from "./sniffer";

attachSniffer();

ext.runtime.onMessage.addListener(
  (message: PopupRequest, _sender, sendResponse: (r: PopupResponse) => void) => {
    handle(message)
      .then(sendResponse)
      .catch((e: unknown) => sendResponse({ ok: false, error: String(e) }));
    // Returning true keeps the message channel open for the async reply above.
    return true;
  },
);

async function handle(message: PopupRequest): Promise<PopupResponse> {
  switch (message.action) {
    case "listCandidates":
      return { ok: true, candidates: await listCandidates(message.tabId) };
    case "clearCandidates":
      await clearCandidates(message.tabId);
      return { ok: true };
  }
}
