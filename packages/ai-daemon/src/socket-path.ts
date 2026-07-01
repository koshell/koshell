import { homedir } from "node:os";
import { join } from "node:path";
import process from "node:process";

// Resolves the daemon socket path, mirroring the Rust terminal-core resolution:
// $XDG_RUNTIME_DIR/koshell/daemon.sock, then $XDG_CACHE_HOME/koshell/daemon.sock,
// falling back to ~/.cache/koshell/daemon.sock.
export function resolveSocketPath(): string {
  const runtime = process.env.XDG_RUNTIME_DIR;
  if (runtime !== undefined && runtime.length > 0) {
    return join(runtime, "koshell", "daemon.sock");
  }

  const cache = process.env.XDG_CACHE_HOME;
  if (cache !== undefined && cache.length > 0) {
    return join(cache, "koshell", "daemon.sock");
  }

  return join(homedir(), ".cache", "koshell", "daemon.sock");
}
