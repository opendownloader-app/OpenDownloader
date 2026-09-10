// The eight languages, on screen.
//
// `check.mjs` already proves every catalogue is complete. What it cannot
// see is whether any of it reaches a user: a translation can be present
// in a file and never rendered, which is exactly what happens when a
// page forgets to call `initPageLocale` or a string is built somewhere
// the tree walker never visits.
import { expect, test } from "./fixtures";

/** The popup's own subtitle — a whole sentence, and different enough in
 *  every language that no two can be confused. "Download" would not do:
 *  it is the same word in several. */
const SUBTITLE: Record<string, string> = {
  English: "Every feature is free.",
  简体中文: "所有功能均免费。",
  繁體中文: "所有功能皆免費。",
  日本語: "すべての機能が無料です。",
  한국어: "모든 기능이 무료입니다.",
  Deutsch: "Alle Funktionen sind kostenlos.",
  Español: "Todas las funciones son gratuitas.",
  Português: "Todos os recursos são gratuitos.",
};

test("every offered language actually renders", async ({ context, extensionId }) => {
  const page = await context.newPage();
  await page.goto(`chrome-extension://${extensionId}/popup.html`);
  const picker = page.locator("select.language-picker");
  await expect(picker, "the popup should offer a language picker").toBeVisible();

  const header = page.locator("header p");
  for (const [language, sentence] of Object.entries(SUBTITLE)) {
    await picker.selectOption({ label: language });
    await expect(header, `${language} should render its own subtitle`).toContainText(sentence);
  }
  await page.close();
});

test("the choice survives, and <html lang> follows it", async ({ context, extensionId }) => {
  const page = await context.newPage();
  await page.goto(`chrome-extension://${extensionId}/popup.html`);
  await page.locator("select.language-picker").selectOption({ label: "日本語" });

  // Not decoration: `lang` selects the glyphs for Han characters, which
  // are drawn differently in Japanese and Chinese, and it is what a
  // screen reader switches voice on.
  await expect(page.locator("html")).toHaveAttribute("lang", "ja");

  // A picker that forgets is not a picker. The popup is torn down on
  // every close, so this is the ordinary case rather than an edge one.
  await page.reload();
  await expect(page.locator("header p")).toContainText(SUBTITLE["日本語"]);
  await expect(page.locator("select.language-picker")).toHaveValue("ja");
  await page.close();
});

test("the wordmark is never translated", async ({ context, extensionId }) => {
  // It is a name, not a word. Excluded by `data-i18n-skip` rather than by
  // hoping no catalogue ever contains it.
  const page = await context.newPage();
  await page.goto(`chrome-extension://${extensionId}/popup.html`);
  await page.locator("select.language-picker").selectOption({ label: "简体中文" });
  await expect(page.locator("header h1")).toHaveText("OpenDownloader");
  await page.close();
});
