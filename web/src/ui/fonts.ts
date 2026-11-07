// Inter, self-hosted with its optical sizes. Registered from script rather than CSS because
// the bundler inlines small CSS assets as data: URLs, which the page's CSP (font-src 'self')
// refuses; a file import always ships as a same-origin asset.

import latinExt from "@fontsource-variable/inter/files/inter-latin-ext-opsz-normal.woff2" with { type: "file" };
import latinItalic from "@fontsource-variable/inter/files/inter-latin-opsz-italic.woff2" with { type: "file" };
import latin from "@fontsource-variable/inter/files/inter-latin-opsz-normal.woff2" with { type: "file" };

const LATIN =
  "U+0000-00FF, U+0131, U+0152-0153, U+02BB-02BC, U+02C6, U+02DA, U+02DC, U+0304, U+0308, U+0329, " +
  "U+2000-206F, U+20AC, U+2122, U+2191, U+2193, U+2212, U+2215, U+FEFF, U+FFFD";
const LATIN_EXT =
  "U+0100-02BA, U+02BD-02C5, U+02C7-02CC, U+02CE-02D7, U+02DD-02FF, U+0304, U+0308, U+0329, " +
  "U+1D00-1DBF, U+1E00-1E9F, U+1EF2-1EFF, U+2020, U+20A0-20AB, U+20AD-20C0, U+2113, U+2C60-2C7F, U+A720-A7FF";

/** Add the Inter faces to the document; the browser fetches each only when text needs it. */
export function registerFonts(): void {
  const faces: [string, FontFaceDescriptors][] = [
    [latin, { style: "normal", weight: "100 900", display: "swap", unicodeRange: LATIN }],
    [latinExt, { style: "normal", weight: "100 900", display: "swap", unicodeRange: LATIN_EXT }],
    [latinItalic, { style: "italic", weight: "100 900", display: "swap", unicodeRange: LATIN }],
  ];
  for (const [url, descriptors] of faces) {
    document.fonts.add(new FontFace("Inter", `url("${url}") format("woff2")`, descriptors));
  }
}
