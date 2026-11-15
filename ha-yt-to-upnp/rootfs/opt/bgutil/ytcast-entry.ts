// The app's entry to the PoToken helper (bgutil-ytdlp-pot-provider): bgutils-js
// fetches BotGuard with a fixed macOS user agent while the challenge sees the
// real navigator.platform ("Linux x86_64" in the app), and YouTube rejects
// tokens minted with that mismatch. So every request bgutil makes through axios
// (the bare import resolves to bgutil's own instance) gets a user agent in
// bgutils-js's format that names the real platform; nothing is spoofed. A full
// Chrome user agent would be rejected as well.
import axios from "axios";

const userAgent = `Mozilla/5.0 (X11; ${navigator.platform}) AppleWebKit/537.36(KHTML, like Gecko)`;
axios.interceptors.request.use((config) => {
  config.headers.set("user-agent", userAgent);
  return config;
});

await import("./src/main.ts");
