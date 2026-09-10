// The shared UI: everything both front ends render.

export { Manager, candidateForUrl, type ManagerOptions } from "./manager";
export { mountTools, type ToolsOptions } from "./tools";
export { checkbox, el, field, naturalSort, pickFiles, text } from "./dom";
export { Busy, TORRENT_STAGES, type Stage } from "./busy";
export {
  LOCALES,
  DEFAULT_LOCALE,
  getLocale,
  setLocale,
  initLocale,
  initPageLocale,
  localizeDom,
  onLocaleChange,
  resolveLocale,
  t,
  missing,
  CATALOGUES,
  type Catalogue,
  type LocaleDef,
} from "./i18n";
export { languagePicker } from "./language-picker";
