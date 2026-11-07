import { render } from "preact";
import { App } from "./app.tsx";
import { startLive } from "./live/runtime.ts";
import { registerFonts } from "./ui/fonts.ts";

registerFonts();
const root = document.getElementById("app");
if (!root) throw new Error("index.html has no #app element");
render(<App />, root);
startLive();
