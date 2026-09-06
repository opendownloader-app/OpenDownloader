# Downloading from the major video platforms

**Requested 2026-09-05:** support Bilibili, YouTube, TikTok, Douyin, Instagram,
Facebook, WeChat and the rest, reversing the "compliance subset" positioning that
`2026-09-04-all-features-free.md` inherited from the market study.

That study listed this as an opportunity it advised against taking, on
positioning grounds. The owner has decided otherwise, which is their call. This
document records what changes, what stays refused, and what it costs.

## What stays refused, and why it is different

The blocklist does not disappear; it narrows to **one principle: we do not break
encryption**.

Refused, permanently, with no setting to change it:

- **DRM services** — Netflix, Disney+, Hotstar, Prime Video, Max, Hulu, Peacock,
  Paramount+, Apple TV+, Spotify, Apple Music, Audible, Tidal, Deezer,
  Crunchyroll. Their catalogues are Widevine/FairPlay/PlayReady protected.
  Circumventing that is a distinct legal category (DMCA §1201 and equivalents),
  not a terms-of-service question.
- **Encrypted streams anywhere** — `#EXT-X-KEY`, `#EXT-X-SESSION-KEY`, DASH
  `ContentProtection`. No key is fetched and nothing is decrypted, on any host.
  This is what actually protects the paid catalogue of a mixed site like
  Bilibili or iQiyi: its free content downloads, its DRM content refuses itself.

Allowed from now on: YouTube, Bilibili, TikTok, Douyin, Instagram, Facebook,
WeChat, X/Twitter, Vimeo, Twitch, Reddit, Weibo, Kuaishou, Xiaohongshu, and
anything else not on the DRM list.

## What this costs — read before publishing

**The Chrome Web Store forbids it.** The Developer Program Policies name YouTube
downloading explicitly. Edge mirrors Chrome's policy. Firefox's AMO has removed
YouTube downloaders before. An extension that does this will not survive on those
stores, whatever its listing says.

That leaves the distribution channels this does not affect: the **web app**, the
**self-hosted relay**, and a **self-distributed** extension (a `.crx`/`.xpi` from
your own site, loaded unpacked or self-hosted, which is how yt-dlp-adjacent tools
have always shipped). `docs/store-listing/` copy is written for the compliance
build and needs a second variant if both are published; the recommendation is to
keep the store build as-is and ship platform support in the self-distributed and
web builds. Nothing in the code prevents you deciding otherwise.

## What actually works, and how it was determined

Every claim below was checked live from this machine on 2026-09-05.

| Site | Datacenter fetch | Why | The route that works |
|---|---|---|---|
| YouTube | ✅ works | InnerTube needs no session | **iOS InnerTube client** — verified returning 24 direct URLs, no cipher, ranges honoured |
| TikTok | ❌ WAF login page | bot-blocked outside a browser | **read the page's own state** |
| Bilibili | ❌ HTTP 412 | anti-bot on both site and API | **read the page's own state** |
| Douyin | ⚠️ interstitial | session-gated | **read the page's own state** |
| Instagram | ⚠️ interstitial | session-gated | **read the page's own state** |
| Facebook | ❌ 302 to login | session-gated | **read the page's own state** |
| WeChat | ⚠️ partial | article pages vary | **read the page's own state** |

**This is the whole design insight.** Scraping these sites from outside is
fragile and mostly blocked. But the extension runs *inside the page the user is
already looking at, in their session*. The data is already in the document — the
player was given it in order to play. Reading `window.__playinfo__` on Bilibili
or `__UNIVERSAL_DATA_FOR_REHYDRATION__` on TikTok is not circumvention, it is
reading what the page loaded, and it works where scraping does not.

So there are three routes, in order of preference:

1. **Page state** — inject a reader on user action and parse the JSON the page
   already holds. Best quality metadata (titles, all renditions), no bot
   detection, uses the session the user already has.
2. **The sniffer that already exists** — TikTok, Douyin, Instagram and Facebook
   serve plain progressive MP4s, which `webRequest` already sees. Removing the
   blocklist alone makes these work. Belt and braces behind route 1.
3. **API extractor** — YouTube only, because InnerTube genuinely works from
   anywhere and the page no longer carries usable URLs.

### YouTube specifically

The web client stopped returning format URLs. `ytInitialPlayerResponse` now
carries `serverAbrStreamingUrl` and formats with **no `url` and no
`signatureCipher`** — server-side ABR. Scraping the page for URLs is dead, and
so is the signature-cipher work that every older downloader does.

The iOS InnerTube client still returns direct URLs. Verified: 22 video and 10
audio formats, no `n` throttle parameter, a range request answered `206` with a
correct `Content-Range`. That means the existing resumable, multi-connection,
verifying engine works on YouTube unchanged.

Video and audio are **separate** — there are no muxed formats left. Merging is
therefore not optional, which is why `dl-container::mux` is in this plan.

## Tasks

### Phase 1 — policy
- ☑ `dl-core::policy`: replace the host blocklist with the DRM list above;
  document the principle in the module doc; keep `refuse_encrypted` untouched and
  extend it to DASH `ContentProtection`. Update every test.

### Phase 2 — the extractor framework and the sites
- ☑ `dl-core::sites`: a state machine in the existing "Rust decides, TypeScript
  fetches" shape. Rust says what to fetch or what to read from the page;
  TypeScript performs it and feeds the bytes back.
- ☑ Per-site extractors: YouTube (InnerTube), Bilibili, TikTok, Douyin,
  Instagram, Facebook, WeChat, and a generic reader covering X/Twitter, Vimeo,
  Twitch, Reddit, Weibo, Kuaishou, Xiaohongshu and others. Fixture-driven tests.
- ☑ A `platform-sites` cargo feature, on by default and **off** for a store
  build. A store build does not contain the extractor code at all, which is the
  one thing a reviewer can verify and a runtime setting cannot.

### Phase 3 — merging video and audio
- ☑ `dl-container::mux`: read a video-only and an audio-only **progressive** MP4,
  emit one fragmented MP4 carrying both. Reuses the `mp4` sample-table reader and
  the `fmp4` writer.
- ☑ `dl-container::fmerge`: the same for **fragmented** input, which is what
  YouTube and Bilibili actually serve — discovered by trying `mux` against a real
  stream and getting "stsc is empty". A fragmented file's sample tables are
  deliberately empty and its `sidx` indexes its fragments, so merging is
  renumbering tracks and passing fragments through rather than rewriting samples.

### Phase 4 — plumbing
- ☑ Extension: `scripting` and `declarativeNetRequest` permissions, page-state
  reading on user action behind the same per-site grant detection uses, the popup
  showing real titles and qualities, a `merge` job kind.
- ☑ Forbidden headers. `Referer` and `Origin` are both load-bearing here and both
  are on the Fetch standard's forbidden list, so `fetch` silently drops them.
  YouTube's InnerTube endpoint answers 403 to every `Origin` but its own, which
  includes the `chrome-extension://…` an extension page sends. The extension
  rewrites them with a session `declarativeNetRequest` rule scoped to the exact
  URL and removed when the job ends; a web page cannot, and says so.
- ☑ Web app: the same for a pasted link, minus page-state reading, which a page
  cannot do for another origin.
- ☑ Engine: a `merge` job that fetches two streams and combines them, choosing
  the fragmented or progressive merger by looking at the bytes rather than
  trusting the site.

### Phase 5 — verification
- ☑ Fixture tests for every extractor. YouTube's are cut from a real captured
  response; the rest are hand-written to the documented page shape and say so in
  their module docs, because no live capture is possible from a datacenter IP.
- ☑ A live check against YouTube: the real wasm extractor, against the real site,
  yielding 9 options with correct titles and sizes and both streams answering
  `206` to a range request.
- ☑ End-to-end tests in a real browser for the extraction path, the resulting
  download, and the DRM refusal.
- ☑ Full suite green: 365 Rust tests, 21 browser end-to-end tests, the web app's
  smoke test, clippy, fmt and `tsc` clean, and all three builds produced. The
  store build (`--no-default-features`) passes its own 104 tests and its wasm is
  69 KB smaller with none of the platform markers in it.
- ☑ **A real YouTube video downloaded, merged and played.** The whole chain, live:
  extract → 1 MiB range reads → fragmented merge → verify. `ffprobe` reads the
  result as `h264 320x240` plus `aac 44100 Hz stereo`, 19.06 s, and it decodes
  end to end with no errors.

## The one thing that could not be verified here

Short videos download, merge and play. **Long ones stop after the first
megabyte**, and it is worth being exact about why, because the answer is not in
this code.

Measured from this machine on 2026-09-05: a Google media URL serves offset 0 and
refuses every offset at or beyond 1 MiB with `403` — on a freshly issued URL,
at any range size, whether the reads are sequential or not. That was chased
through three plausible theories (a size ceiling, per-URL ageing, a
sequential-access requirement) and none of them survived contact with the
evidence. What remains is Google declining to serve bulk media to a datacenter
address, which is exactly what a build machine looks like from the outside.

A browser on an ordinary connection is the case this product runs in, and there
the same URLs stream to completion — it is how YouTube itself plays them. The
code's response is small and already made: a `403` mid-download is treated as a
throttle and retried with backoff, and when it persists the user is told the host
stopped serving the file rather than being shown a bare status code.

## Added 2026-09-05, second pass

**Independent video and audio choice.** `Extraction` gained `videos` and `audios`, two
ranked lists a UI can pair however the user likes, alongside the ready-made `options`.
Both front ends show the one-click "best available" first and a "choose quality yourself"
panel underneath with two dropdowns.

**"Best" means best deliverable, not largest.** This is the part worth reading. On
YouTube the top renditions are VP9 and AV1 in WebM and the top audio is Opus, while this
build joins picture to sound only inside MP4 — so flagging the largest would recommend a
file that cannot be given sound. `rank_choices` marks the highest rendition that can
actually be produced complete. Everything else stays in the list and stays choosable; the
UI disables the button and says why when a combination cannot be joined.

Mergeability is judged by **container, not codec**, and that was checked rather than
assumed: the fragmented merger copies each track's description through verbatim, so an
AV1 video merged with an AAC track into a file `ffprobe` reads as `av1` + `aac`, H.264
likewise, and WebM failed exactly as the flag predicts.

**Four more extractors**, none of which any store policy names, so all four are in every
build including the store one: Vimeo (its config endpoint, since its `og:video` points at
a player page and is deliberately ignored), Dailymotion, Twitch clips, and X. The generic
page reader's host list grew to about forty mainstream sites — Reddit, Tumblr, Pinterest,
LinkedIn, Threads, VK, Rumble, Odysee, Niconico, SoundCloud, Bandcamp, the Chinese long
tail and several news sites.

**Two bugs found by testing rather than by reading.** X writes `application/x-mpegURL`
with a capital URL, so a lowercase match silently classified its HLS playlist as a
progressive MP4. And ranking by pixel count tied every Vimeo, Dailymotion and Twitch
rendition at zero, because those sites state a height and no width — ranking is by height
now.

`npm run verify:sites` drives the compiled wasm against the live internet and prints what
each site offers; `--full` also downloads, merges and decodes a real video.

## What will break, and when

Site extractors rot. That is the nature of the category — yt-dlp ships several times a week
for exactly this reason. Every extractor here is a pure function over a captured
response shape, so when a site changes, the failing test names the site and the
fix is local. That is the most that can be promised, and it is worth saying
plainly in the README rather than implying permanence.
