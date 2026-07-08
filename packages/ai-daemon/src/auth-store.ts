// Koshell's persistent credential store, backing `koshell auth login` and
// provider resolution (design 0014).
//
// The store lives at $XDG_DATA_HOME/koshell/auth.json and is written by pi's
// file backend: 0600 file, 0700 parent directory, cross-process lock via
// proper-lockfile, with expired OAuth tokens refreshed in place under that
// lock. It is Koshell's own file — pi's ~/.pi/agent/auth.json is never read,
// preserving the design-0011 independence boundary.
import { homedir } from "node:os";
import { join } from "node:path";
import process from "node:process";

import { AuthStorage } from "@earendil-works/pi-coding-agent";

// Resolves the store path, following XDG: $XDG_DATA_HOME/koshell/auth.json,
// falling back to ~/.local/share/koshell/auth.json.
export function resolveAuthStorePath(): string {
  const dataHome = process.env.XDG_DATA_HOME;
  if (dataHome !== undefined && dataHome.length > 0) {
    return join(dataHome, "koshell", "auth.json");
  }
  return join(homedir(), ".local", "share", "koshell", "auth.json");
}

// Opens the file-backed store. Creating it reads the file synchronously and
// touches an empty one into existence on first use.
export function openAuthStorage(): AuthStorage {
  return AuthStorage.create(resolveAuthStorePath());
}
