// Component tests render into happy-dom; Testing Library unmounts after each test.
import { cleanup } from "@testing-library/react";
import { afterEach } from "vite-plus/test";

(globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

afterEach(cleanup);
