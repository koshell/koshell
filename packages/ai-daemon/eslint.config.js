import js from "@eslint/js";
import { defineConfig, globalIgnores } from "eslint/config";
import prettierConfig from "eslint-config-prettier";
import tseslint from "typescript-eslint";

export default defineConfig([
  globalIgnores(["coverage/**", "dist/**", "node_modules/**"]),
  js.configs.recommended,
  {
    languageOptions: {
      globals: {
        console: "readonly",
      },
    },
  },
  ...tseslint.configs.strictTypeChecked,
  ...tseslint.configs.stylisticTypeChecked,
  {
    files: ["**/*.ts"],
    languageOptions: {
      parserOptions: {
        projectService: true,
        tsconfigRootDir: import.meta.dirname,
      },
    },
  },
  {
    ...tseslint.configs.disableTypeChecked,
    files: ["**/*.js"],
  },
  prettierConfig,
]);
