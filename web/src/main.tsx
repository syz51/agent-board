import { QueryClientProvider } from "@tanstack/react-query";
import { RouterProvider } from "@tanstack/react-router";
import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { installCspNonce } from "./lib/csp-nonce.ts";
import { queryClient, startLive } from "./live/runtime.ts";
import { createAppRouter } from "./router.tsx";
import { registerFonts } from "./ui/fonts.ts";

installCspNonce();
registerFonts();
const root = document.getElementById("app");
if (!root) throw new Error("index.html has no #app element");
const router = createAppRouter();
createRoot(root).render(
  <StrictMode>
    <QueryClientProvider client={queryClient}>
      <RouterProvider router={router} />
    </QueryClientProvider>
  </StrictMode>,
);
startLive();
