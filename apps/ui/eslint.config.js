import js from "@eslint/js";
import svelte from "eslint-plugin-svelte";
import globals from "globals";
import ts from "typescript-eslint";
import svelteConfig from "./svelte.config.js";

export default ts.config(
  { ignores: ["dist/", "node_modules/", "static/", "src/lib/generated/"] },
  js.configs.recommended,
  ...ts.configs.recommended,
  ...svelte.configs.recommended,
  {
    languageOptions: { globals: { ...globals.browser, ...globals.node } },
  },
  {
    files: ["**/*.svelte", "**/*.svelte.ts"],
    languageOptions: {
      parserOptions: {
        projectService: true,
        extraFileExtensions: [".svelte"],
        parser: ts.parser,
        svelteConfig,
      },
    },
  },
  {
    rules: {
      "@typescript-eslint/no-unused-vars": ["error", { argsIgnorePattern: "^_" }],
      // api.ts is the only module allowed to talk to Tauri.
      "no-restricted-imports": [
        "error",
        {
          patterns: [
            { group: ["@tauri-apps/*"], message: "Import Tauri only from src/lib/api.ts." },
          ],
        },
      ],
    },
  },
  {
    files: ["src/lib/api.ts", "src/lib/api.test.ts"],
    rules: { "no-restricted-imports": "off" },
  },
);
