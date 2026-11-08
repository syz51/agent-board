import { type ClassValue, clsx } from "clsx";
import { extendTailwindMerge } from "tailwind-merge";

// The theme's own size and shadow tokens (src/styles.css), so `cn` knows `text-meta` is a
// font size beside a text colour, not a second colour that replaces it.
const twMerge = extendTailwindMerge({
  extend: {
    theme: {
      text: ["2xs", "meta", "title", "display", "display-phone", "headline"],
      shadow: ["raise", "overlay", "panel"],
    },
  },
});

export function cn(...inputs: ClassValue[]) {
  return twMerge(clsx(inputs));
}
