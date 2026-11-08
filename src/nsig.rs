//! nsig (n-parameter) decoder using yt-dlp/ejs solver running in QuickJS.
//!
//! Embeds the yt-dlp/ejs bundled JS files (meriyah parser + solver core, ~165KB)
//! and runs them in rquickjs. No SWC, no regex extraction — proper AST analysis.
//!
//! Caching strategy:
//! - First decode parses base.js with meriyah (~40s, ~256MB). Produces a
//!   "preprocessed player" string that's cached both in memory and to disk.
//! - Subsequent decodes reuse the preprocessed player (~0.6s, ~50MB).
//! - Disk cache survives reboots. Only re-parses when base.js hash changes.

use std::path::PathBuf;
use std::sync::Mutex;

const LIB_JS: &str = include_str!("vendor/yt.solver.lib.min.js");
const CORE_JS: &str = include_str!("vendor/yt.solver.core.min.js");
/// yt-dlp/ejs release of the vendored files (see `vendor/README.md`). Part of
/// the disk cache key: a player preprocessed by another core is not reused.
const EJS_VERSION: &str = "0.8.0";

/// Stack size for the solver thread. meriyah recurses deeply parsing 2.7MB base.js.
const SOLVER_STACK_SIZE: usize = 8 * 1024 * 1024; // 8MB

/// In-memory cache: (player_hash, preprocessed_player_js)
static PREPROCESSED_CACHE: Mutex<Option<(String, String)>> = Mutex::new(None);

/// Disk cache file path: `$YTCAST_DATA_DIR/nsig-cache/` or `~/.cache/ytcast/`,
/// file `nsig-preprocessed-ejs{EJS_VERSION}-{hash}.js`.
fn cache_path(player_hash: &str) -> Option<PathBuf> {
    cache_path_in(crate::storage::data_dir_env().as_deref(), player_hash)
}

fn cache_path_in(data_dir: Option<&std::path::Path>, player_hash: &str) -> Option<PathBuf> {
    crate::storage::nsig_cache_dir(data_dir)
        .map(|d| d.join(format!("nsig-preprocessed-ejs{}-{}.js", EJS_VERSION, player_hash)))
}

fn load_disk_cache_at(path: &std::path::Path) -> Option<String> {
    match std::fs::read_to_string(path) {
        Ok(s) if !s.is_empty() => {
            crate::storage::warn_if_loose_permissions(path);
            tracing::info!("[nsig] loaded disk cache: {} ({} bytes)", path.display(), s.len());
            Some(s)
        }
        _ => None,
    }
}

/// The cached JS is executed later, so it is written like a credential:
/// atomically, 0600, replacing (not following) whatever is at `path`.
fn save_disk_cache_at(path: &std::path::Path, preprocessed: &str) {
    match crate::storage::write_private_atomic(path, preprocessed.as_bytes()) {
        Ok(()) => tracing::info!("[nsig] saved disk cache: {} ({} bytes)", path.display(), preprocessed.len()),
        Err(e) => tracing::warn!("[nsig] failed to save disk cache: {}", e),
    }
}

/// Decode an nsig challenge value using yt-dlp/ejs solver in QuickJS.
///
/// `player_hash` identifies the base.js version (used for cache key).
/// First call with a new player_hash parses base.js (~40s, ~256MB).
/// Subsequent calls (same hash) reuse cached preprocessed player (~0.6s).
/// Cache persists to disk so reboots don't trigger re-parse.
pub fn decode_nsig(player_js: &str, n_value: &str, player_hash: &str) -> Result<String, String> {
    decode_nsig_cached_at(player_js, n_value, player_hash, cache_path(player_hash))
}

/// [`decode_nsig`] with an explicit disk cache file.
pub fn decode_nsig_cached_at(
    player_js: &str,
    n_value: &str,
    player_hash: &str,
    disk_cache: Option<PathBuf>,
) -> Result<String, String> {
    let player_js = player_js.to_owned();
    let n_value = n_value.to_owned();
    let player_hash = player_hash.to_owned();

    // Check memory cache first, then disk cache
    let cached = {
        let guard = PREPROCESSED_CACHE.lock().unwrap();
        if let Some((ref h, ref pp)) = *guard {
            if h == &player_hash {
                Some(pp.clone())
            } else {
                None
            }
        } else {
            None
        }
    };
    let cached = cached.or_else(|| load_disk_cache_at(disk_cache.as_deref()?));

    std::thread::Builder::new()
        .name("nsig-solver".into())
        .stack_size(SOLVER_STACK_SIZE)
        .spawn(move || {
            let result = decode_nsig_inner(&player_js, &n_value, cached.as_deref())
                .inspect_err(|_| {
                    if cached.is_some() {
                        discard_cache(&player_hash, disk_cache.as_deref());
                    }
                })?;
            // An unusable answer from a player that ran is not a cache fault.
            check_decoded(&n_value, &result.decoded)?;

            // Cache the preprocessed player if we got one back
            if let Some(ref pp) = result.preprocessed {
                *PREPROCESSED_CACHE.lock().unwrap() = Some((player_hash.clone(), pp.clone()));
                if let Some(ref path) = disk_cache {
                    save_disk_cache_at(path, pp);
                }
            }

            Ok(result.decoded)
        })
        .map_err(|e| format!("spawn solver thread: {}", e))?
        .join()
        .map_err(|_| "solver thread panicked".to_string())?
}

fn check_decoded(n_value: &str, decoded: &str) -> Result<(), String> {
    if decoded.is_empty() || decoded == n_value {
        return Err(format!(
            "nsig decode returned invalid result: '{}'",
            &decoded[..decoded.floor_char_boundary(64)]
        ));
    }
    tracing::info!(
        "[nsig] decoded: {} -> {}",
        n_value,
        &decoded[..decoded.floor_char_boundary(16)]
    );
    Ok(())
}

/// A cached player that fails to run is never retried: drop it from memory
/// and disk so the next call re-parses base.js.
fn discard_cache(player_hash: &str, disk_cache: Option<&std::path::Path>) {
    let mut guard = PREPROCESSED_CACHE.lock().unwrap();
    if guard.as_ref().is_some_and(|(h, _)| h == player_hash) {
        *guard = None;
    }
    drop(guard);
    if let Some(path) = disk_cache {
        match std::fs::remove_file(path) {
            Ok(()) => tracing::warn!("[nsig] discarded failing disk cache: {}", path.display()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => tracing::warn!("[nsig] failed to remove disk cache {}: {}", path.display(), e),
        }
    }
}

struct DecodeResult {
    decoded: String,
    preprocessed: Option<String>,
}

fn decode_nsig_inner(
    player_js: &str,
    n_value: &str,
    cached_preprocessed: Option<&str>,
) -> Result<DecodeResult, String> {
    let is_cached = cached_preprocessed.is_some();
    let mem_limit = if is_cached { 64 * 1024 * 1024 } else { 256 * 1024 * 1024 };

    let rt = rquickjs::Runtime::new()
        .map_err(|e| format!("QuickJS runtime failed: {}", e))?;
    rt.set_memory_limit(mem_limit);
    rt.set_max_stack_size(4 * 1024 * 1024);
    let ctx = rquickjs::Context::full(&rt)
        .map_err(|e| format!("QuickJS context failed: {}", e))?;

    ctx.with(|ctx| {
        // 1. Load meriyah + astring library
        ctx.eval::<rquickjs::Value, _>(LIB_JS.to_owned().into_bytes())
            .map_err(|e| fmt_js_error(&ctx, e, "lib.js eval"))?;

        // 2. Copy exports to globalThis
        ctx.eval::<(), _>(b"Object.assign(globalThis, lib);".to_vec())
            .map_err(|e| fmt_js_error(&ctx, e, "globalThis assign"))?;

        // 3. Load the solver core
        ctx.eval::<rquickjs::Value, _>(CORE_JS.to_owned().into_bytes())
            .map_err(|e| fmt_js_error(&ctx, e, "core.js eval"))?;

        // 4. Set variables
        let global = ctx.globals();
        global.set("_n_value", n_value)
            .map_err(|e| format!("set _n_value failed: {}", e))?;

        let result_json: String = if let Some(preprocessed) = cached_preprocessed {
            // Fast path: use cached preprocessed player (~0.6s, ~50MB)
            tracing::info!("[nsig] using cached preprocessed player");
            global.set("_preprocessed", preprocessed)
                .map_err(|e| format!("set _preprocessed failed: {}", e))?;

            let call_js = r#"
                try {
                    var _r = jsc({
                        type: "preprocessed",
                        preprocessed_player: _preprocessed,
                        requests: [{type: "n", challenges: [_n_value]}],
                        output_preprocessed: false
                    });
                    JSON.stringify(_r);
                } catch(e) {
                    var msg = e ? (e.message || String(e)) : "null exception (OOM?)";
                    JSON.stringify({type: "error", message: msg});
                }
            "#;
            ctx.eval(call_js.as_bytes().to_vec())
                .map_err(|e| fmt_js_error(&ctx, e, "jsc(preprocessed)"))?
        } else {
            // Slow path: parse base.js from scratch (~40s, ~256MB)
            tracing::info!("[nsig] parsing base.js with meriyah (first time, ~40s)...");
            global.set("_player_js", player_js)
                .map_err(|e| format!("set _player_js failed: {}", e))?;

            let call_js = r#"
                try {
                    var _r = jsc({
                        type: "player",
                        player: _player_js,
                        requests: [{type: "n", challenges: [_n_value]}],
                        output_preprocessed: true
                    });
                    JSON.stringify(_r);
                } catch(e) {
                    var msg = e ? (e.message || String(e)) : "null exception (OOM?)";
                    JSON.stringify({type: "error", message: msg});
                }
            "#;
            ctx.eval(call_js.as_bytes().to_vec())
                .map_err(|e| fmt_js_error(&ctx, e, "jsc(player)"))?
        };

        // 5. Parse result
        let result: serde_json::Value = serde_json::from_str(&result_json)
            .map_err(|e| format!("parse jsc result: {}", e))?;

        if result.get("type").and_then(|t| t.as_str()) == Some("error") {
            let msg = result.get("message").and_then(|m| m.as_str()).unwrap_or("unknown");
            return Err(format!("jsc error: {}", msg));
        }

        // Extract preprocessed player for caching
        let preprocessed = result
            .get("preprocessed_player")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let response = result
            .get("responses")
            .and_then(|r| r.as_array())
            .and_then(|arr| arr.first());
        if let Some(err) = response
            .filter(|r| r.get("type").and_then(|t| t.as_str()) == Some("error"))
            .and_then(|r| r.get("error"))
            .and_then(|e| e.as_str())
        {
            return Err(format!("jsc n error: {}", &err[..err.floor_char_boundary(300)]));
        }

        // Extract decoded n from responses[0].data[n_value]
        let decoded = response
            .and_then(|resp| resp.get("data"))
            .and_then(|data| data.get(n_value))
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                format!(
                    "unexpected jsc response: {}",
                    &result_json[..result_json.floor_char_boundary(300)]
                )
            })?;

        Ok(DecodeResult {
            decoded: decoded.to_string(),
            preprocessed,
        })
    })
}

fn fmt_js_error(ctx: &rquickjs::Ctx<'_>, _e: rquickjs::Error, stage: &str) -> String {
    let caught = ctx.catch();
    if let Some(exc) = caught.as_exception() {
        let msg = exc.message().unwrap_or_default();
        let stack = exc
            .get::<_, rquickjs::String>("stack")
            .ok()
            .and_then(|s| s.to_string().ok());
        if let Some(stack) = stack {
            format!(
                "{} failed: {} | {}",
                stage,
                msg,
                &stack[..stack.floor_char_boundary(300)]
            )
        } else {
            format!("{} failed: {}", stage, msg)
        }
    } else {
        format!("{} failed: {:?}", stage, caught)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::test_util::TempDir;

    #[test]
    fn cache_path_unchanged_without_data_dir() {
        assert_eq!(
            cache_path_in(None, "abc"),
            dirs::cache_dir().map(|d| d.join("ytcast").join("nsig-preprocessed-ejs0.8.0-abc.js"))
        );
    }

    #[test]
    fn cache_written_under_data_dir() {
        let tmp = TempDir::new("nsig-cache");
        let path = cache_path_in(Some(tmp.path()), "abc").unwrap();
        assert_eq!(path, tmp.path().join("nsig-cache").join("nsig-preprocessed-ejs0.8.0-abc.js"));

        save_disk_cache_at(&path, "preprocessed");
        assert_eq!(load_disk_cache_at(&path).as_deref(), Some("preprocessed"));
        #[cfg(unix)]
        assert_eq!(crate::storage::test_util::mode(path.parent().unwrap()), 0o700);
    }

    #[cfg(unix)]
    #[test]
    fn cache_file_is_private() {
        let tmp = TempDir::new("nsig-mode");
        let path = tmp.path().join("nsig-preprocessed-abc.js");
        save_disk_cache_at(&path, "preprocessed");
        assert_eq!(crate::storage::test_util::mode(&path), 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn loose_cache_file_warns_with_path_only() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new("nsig-loose");
        let path = tmp.path().join("nsig-preprocessed-abc.js");
        std::fs::write(&path, "PREPROCESSED-PLAYER-BODY").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        let logs = crate::storage::test_util::capture_logs(|| {
            assert!(load_disk_cache_at(&path).is_some());
        });
        assert!(logs.contains("WARN"), "{logs}");
        assert!(logs.contains(path.to_str().unwrap()), "{logs}");
        assert!(!logs.contains("PREPROCESSED-PLAYER-BODY"), "{logs}");
    }

    #[test]
    fn decoded_value_must_differ_from_input() {
        assert!(check_decoded("abc", "").is_err());
        assert!(check_decoded("abc", "abc").is_err());
        // Multi-byte char across the 16-byte log cut.
        check_decoded("abc", &format!("{}\u{20ac}tail", "x".repeat(15))).unwrap();
    }

    #[test]
    fn invalid_result_error_is_truncated() {
        let decoded = format!("{}\u{20ac}{}", "x".repeat(63), "TAIL".repeat(50));
        let err = check_decoded(&decoded, &decoded).unwrap_err();
        assert!(!err.contains("TAIL"), "{err}");
    }

    #[test]
    fn cached_player_returning_unchanged_n_keeps_cache() {
        let tmp = TempDir::new("nsig-identity");
        let path = tmp.path().join("nsig-preprocessed-identity.js");
        // A preprocessed player whose n function is the identity.
        std::fs::write(&path, "_result.n = function (n) { return n; };").unwrap();

        let result = decode_nsig_cached_at("", "abc", "test-identity-player", Some(path.clone()));

        assert!(result.unwrap_err().contains("invalid result"));
        assert!(path.exists(), "cache was discarded for a player that ran");
    }

    #[test]
    fn corrupt_cache_is_removed_after_failed_run() {
        let tmp = TempDir::new("nsig-corrupt");
        let path = tmp.path().join("nsig-preprocessed-corrupt.js");
        std::fs::write(&path, "this is not a preprocessed player").unwrap();

        // Empty player JS: the fresh path is never taken, so no network or
        // real base.js is involved.
        let result = decode_nsig_cached_at("", "abc", "test-corrupt-cache", Some(path.clone()));

        assert!(result.is_err(), "{result:?}");
        assert!(!path.exists(), "corrupt cache file was kept");
    }

    /// Minimal player in base.js's shape: one IIFE whose URL builder calls
    /// `x.set("alr", "yes")`; its first non-accessor URL method reverses n.
    const SYNTHETIC_PLAYER: &str = r#"(function(g){
var Q = function(u){ this.p = {}; };
Q.prototype.set = function(k, v){ this.p[k] = v; };
Q.prototype.get = function(k){ return this.p[k]; };
Q.prototype.clone = function(){ return this; };
Q.prototype.mutate = function(){ var n = this.p.n; if (n) this.p.n = n.split("").reverse().join(""); };
g.mk = function(url, key, val){ var u = new Q(url); u.set("alr", "yes"); if (val) u.set(key, val); return u; };
}).call(this, {});"#;

    #[test]
    fn solver_extracts_n_from_player_and_reuses_preprocessed() {
        let tmp = TempDir::new("nsig-synthetic");
        let path = tmp.path().join("nsig-preprocessed-synthetic.js");

        let fresh = decode_nsig_cached_at(SYNTHETIC_PLAYER, "abc123", "test-synthetic", Some(path.clone()));
        assert_eq!(fresh.unwrap(), "321cba");
        assert!(path.exists(), "preprocessed player not cached");

        let cached = decode_nsig_cached_at("", "xyz", "test-synthetic-disk", Some(path));
        assert_eq!(cached.unwrap(), "zyx");
    }

    #[test]
    fn player_without_n_function_reports_solver_error() {
        let player = "(function(g){ var a = 1; }).call(this, {});";
        let err = decode_nsig_cached_at(player, "abc", "test-no-solution", None).unwrap_err();
        assert!(err.starts_with("jsc n error: no solutions"), "{err}");
    }
}
