// Starts the daemon with a fake OAuth provider registered, for an end-to-end
// `koshell auth` smoke without real OAuth or network (design 0014):
//
//   export XDG_RUNTIME_DIR=$(mktemp -d) XDG_DATA_HOME=$(mktemp -d)
//   export KOSHELL_DAEMON_CMD="bun $PWD/packages/ai-daemon/scripts/fake-oauth-daemon.ts"
//   koshell auth login fake     # type any code; lands in $XDG_DATA_HOME/koshell/auth.json
//   koshell auth status fake && koshell auth logout fake
//
// The fake flow deliberately skips onAuth so the client never opens a browser;
// it exercises progress, prompt round-trip, persistence, and logout.
import { registerOAuthProvider } from "@earendil-works/pi-ai/oauth";

registerOAuthProvider({
  id: "fake",
  name: "Fake Provider",
  async login(callbacks) {
    callbacks.onProgress?.("Fake provider: no browser needed.");
    const code = await callbacks.onPrompt({
      message: "Enter any code",
      placeholder: "anything",
    });
    return { refresh: "r", access: code, expires: Date.now() + 3_600_000 };
  },
  refreshToken(credentials) {
    return Promise.resolve(credentials);
  },
  getApiKey(credentials) {
    return credentials.access;
  },
});

await import("../src/index.ts");
