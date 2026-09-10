// The language control.
//
// Every language is listed in its own script — 日本語, not "Japanese".
// A picker that names languages in English is unusable by the one person
// who needs it, which is the whole point of having one.
import { el } from "./dom";
import { LOCALES, getLocale, setLocale, t } from "./i18n";

/**
 * Builds the picker. The caller decides where it goes, because the popup,
 * the manager and the web page each have their own idea of a toolbar.
 *
 * A `<select>` rather than a custom menu: it is one element, it is
 * keyboard- and screen-reader-correct without any work, and on a phone
 * the platform gives it a native picker. Nothing here is worth a
 * bespoke dropdown.
 */
export function languagePicker(): HTMLElement {
  // `aria-label` is set directly rather than through `el`: the helper's
  // Attrs type lists the attributes this codebase actually uses, and
  // widening a shared type for one call site is the wrong trade.
  const select = el("select", {
    class: "language-picker",
    title: t("Language"),
  }) as HTMLSelectElement;
  select.setAttribute("aria-label", t("Language"));

  for (const locale of LOCALES) {
    const option = el("option", { value: locale.code }, locale.name) as HTMLOptionElement;
    // `lang` on each option so the browser picks the right glyphs: the
    // same Han codepoint is drawn differently in Chinese and Japanese,
    // and this list shows both at once.
    option.lang = locale.code;
    select.append(option);
  }
  select.value = getLocale();

  select.addEventListener("change", () => setLocale(select.value));

  // The label is itself translated, so it has to be refreshed when the
  // language changes — localizeDom cannot reach an attribute it set.
  const refresh = () => {
    select.value = getLocale();
    select.setAttribute("aria-label", t("Language"));
    select.setAttribute("title", t("Language"));
  };
  select.addEventListener("focus", refresh);
  return select;
}
