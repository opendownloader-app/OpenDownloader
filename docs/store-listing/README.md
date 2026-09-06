# Store listing copy

One `<locale>.txt` per language, each holding two blocks:

- **SEARCH TERMS** — the seven terms for the Edge Partner Center "Search terms"
  field, which is per-language.
- **OVERVIEW** — the long description for the listing's Description field.

These are **not** shipped inside the extension package. They are pasted into the
store dashboards by hand:

| Store | Where |
|---|---|
| Chrome Web Store | Developer Dashboard → item → Store listing → language selector → Description |
| Edge | Partner Center → Extension → Store listings → per-language → Description + Search terms |
| Firefox (AMO) | Developer Hub → Edit listing → Describe Add-on → translations |

The extension's own `name` and `description` are localized separately, from
`apps/extension/public/_locales/<locale>/messages.json`, and update only when a
new version is published. Locale codes follow Chrome's `_locales` convention
(underscore, e.g. `pt_BR`, `zh_CN`).

## What the copy has to do

The market research behind this release found two things that shape every line
of it.

**The headline is a plain benefit, not "open source".** The people this is for
are not shopping for a licence; they are trying to keep a recording. "Open
source" is a trust signal and belongs in the second paragraph, where someone
who has already decided they want the thing goes looking for a reason to trust
it. Every competitor's own listing leads with the task.

**The strongest differentiator is what is *not* there.** The three most common
complaints across this category are bundled installers, a companion app you
must also install, and features that were free until they weren't. So the copy
says, early and plainly: nothing else to install, nothing uploaded, nothing
metered, no subscription. That is not marketing framing — it is the product,
and it is checkable by reading the source.

**What the copy must never do** is imply the extension downloads from services
whose terms forbid it. A listing that hinted otherwise would be removed. Where a
limit exists it is stated as a deliberate choice, which is what it is.

## This copy describes the store build, which is not the only build

Since 2026-09-05 the source also supports the large platforms — YouTube,
Bilibili, TikTok, Douyin, Instagram, Facebook and others. **The copy in this
directory deliberately does not mention any of them**, and that is not an
oversight:

- The Chrome Web Store's developer policy prohibits extensions that download
  from YouTube, and Edge mirrors it. A listing that advertised it would be
  removed, and so would one that shipped it quietly.
- So there are two builds. The **store build** is what this copy describes: open
  media, refusals compiled in, nothing that breaches a store policy. The
  **self-distributed build** adds the platform extractors and is shipped from
  your own site, the way tools in this category have always been.

If you publish only the self-distributed build, this copy still describes it
accurately — it undersells it rather than misleading anyone. If you publish
both, keep this copy for the store and write the platform claims only where they
are safe to make. Do not add YouTube to these files.
