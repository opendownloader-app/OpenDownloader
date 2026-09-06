// Small DOM helpers.
//
// Deliberately not a framework and deliberately not `innerHTML`. Everything
// these pages render includes filenames and URLs that came from a web page the
// extension was pointed at, and `textContent` is what makes that safe by
// construction rather than by remembering to escape.

type Child = Node | string | null | undefined | false;

interface Attrs {
  class?: string;
  title?: string;
  text?: string;
  type?: string;
  value?: string;
  placeholder?: string;
  disabled?: boolean;
  checked?: boolean;
  hidden?: boolean;
  href?: string;
  target?: string;
  rel?: string;
  min?: string;
  max?: string;
  step?: string;
  multiple?: boolean;
  accept?: string;
  spellcheck?: boolean;
  onClick?: (e: MouseEvent) => void;
  onChange?: (e: Event) => void;
  onInput?: (e: Event) => void;
}

export function el<K extends keyof HTMLElementTagNameMap>(
  tag: K,
  attrs: Attrs = {},
  ...children: Child[]
): HTMLElementTagNameMap[K] {
  const node = document.createElement(tag);
  const { onClick, onChange, onInput, text, ...rest } = attrs;
  for (const [key, value] of Object.entries(rest)) {
    if (value === undefined || value === false) continue;
    if (key === "class") node.className = String(value);
    else if (typeof value === "boolean") (node as unknown as Record<string, unknown>)[key] = value;
    else node.setAttribute(key, String(value));
  }
  if (text !== undefined) node.textContent = text;
  if (onClick) node.addEventListener("click", onClick as EventListener);
  if (onChange) node.addEventListener("change", onChange as EventListener);
  if (onInput) node.addEventListener("input", onInput as EventListener);
  for (const child of children) {
    if (child === null || child === undefined || child === false) continue;
    node.append(child);
  }
  return node;
}

export function text(value: string): Text {
  return document.createTextNode(value);
}

/** A labelled control, laid out the same way everywhere. */
export function field(label: string, control: HTMLElement): HTMLElement {
  return el("label", { class: "field" }, el("span", { text: label }), control);
}

/** A checkbox with its label, returning both so the caller can read it. */
export function checkbox(
  label: string,
  checked: boolean,
  onChange: (checked: boolean) => void,
): { row: HTMLElement; input: HTMLInputElement } {
  const input = el("input", { type: "checkbox" });
  input.checked = checked;
  input.addEventListener("change", () => onChange(input.checked));
  return { row: el("label", {}, input, text(label)), input };
}

/** Ask the user for files without keeping a hidden `<input>` in the document. */
export function pickFiles(accept: string, multiple: boolean): Promise<File[]> {
  return new Promise((resolve) => {
    const input = el("input", { type: "file", accept, multiple });
    input.style.display = "none";
    document.body.append(input);
    input.addEventListener(
      "change",
      () => {
        resolve(Array.from(input.files ?? []));
        input.remove();
      },
      { once: true },
    );
    // A cancelled picker fires no event in most browsers, so the element is
    // removed on the next interaction rather than leaked forever.
    input.addEventListener("cancel", () => {
      resolve([]);
      input.remove();
    });
    input.click();
  });
}

/**
 * Sort filenames the way a person expects: `seg2` before `seg10`.
 *
 * HLS segments are named with an unpadded counter, so a plain lexicographic
 * sort interleaves them wrongly and the remuxed output plays out of order.
 */
export function naturalSort(files: File[]): File[] {
  const collator = new Intl.Collator(undefined, { numeric: true, sensitivity: "base" });
  return files.slice().sort((a, b) => collator.compare(a.name, b.name));
}
