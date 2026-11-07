// Component tests render into happy-dom; Testing Library unmounts after each test.
import { GlobalRegistrator } from "@happy-dom/global-registrator";
import { afterEach } from "bun:test";

GlobalRegistrator.register({ url: "http://localhost/" });

const { cleanup } = await import("@testing-library/react");
afterEach(cleanup);
