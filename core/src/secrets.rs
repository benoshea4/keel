// secrets.rs — the secret-store PORT (SPEC-AMENDMENT-4, the first hexagonal
// port). Adapters supply raw secret VALUES by name; ALL journaling, the
// salted-hash-on-replay rotation check, and wire redaction stay ABOVE this
// line (host.rs::secret), so no adapter can weaken the v2.1 security posture —
// the port abstracts only WHERE a value comes from.

use std::sync::Arc;

/// CONTRACT: `get` is re-read LIVE on every call — rotation detection depends
/// on it (host.rs re-verifies a salted hash on replay). `Ok(value)` or
/// `Err(guest-visible reason)`; the error becomes the journaled `{"err"}`, so
/// a missing secret is DATA the workflow handles, never a trap.
pub trait SecretStore: Send + Sync {
    fn get(&self, name: &str) -> Result<String, String>;
    /// One-line human description for the startup log / diagnostics.
    fn describe(&self) -> String;
}

/// The DEFAULT adapter — a strict KEY=VALUE file, re-read per call. Verbatim
/// behaviour of the pre-Amendment-4 `lookup_secret` (same messages, same
/// dup/`=` parse errors via `host::load_secrets`).
pub struct FileSecretStore {
    pub path: String,
}

impl SecretStore for FileSecretStore {
    fn get(&self, name: &str) -> Result<String, String> {
        crate::host::load_secrets(&self.path)?
            .into_iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v)
            .ok_or_else(|| format!("secret '{name}' not found in secrets file"))
    }
    fn describe(&self) -> String {
        format!("file {}", self.path)
    }
}

/// The SECOND adapter — environment variables (12-factor / k8s), the demand
/// that earns the port. `get("API_KEY")` reads `<prefix>API_KEY`, re-read per
/// call (rotation = a redeploy that changes the env). A name that is not a
/// valid env tail simply does not resolve — an `Err`, i.e. DATA.
pub struct EnvSecretStore {
    pub prefix: String,
}

pub const DEFAULT_ENV_PREFIX: &str = "KEEL_SECRET_";

impl SecretStore for EnvSecretStore {
    fn get(&self, name: &str) -> Result<String, String> {
        let var = format!("{}{}", self.prefix, name);
        std::env::var(&var).map_err(|_| format!("secret '{name}' not found in environment (${var})"))
    }
    fn describe(&self) -> String {
        format!("env {}*", self.prefix)
    }
}

/// Composition — the first adapter that RESOLVES wins; errors fall through, and
/// the last error is returned if none resolve. File-before-env when both are
/// configured (an explicit file overrides an ambient env var — least surprising).
pub struct LayeredSecretStore(pub Vec<Arc<dyn SecretStore>>);

impl SecretStore for LayeredSecretStore {
    fn get(&self, name: &str) -> Result<String, String> {
        let mut last: Option<String> = None;
        for s in &self.0 {
            match s.get(name) {
                Ok(v) => return Ok(v),
                Err(e) => last = Some(e),
            }
        }
        last.map(Err).unwrap_or_else(|| {
            Err(format!("secret '{name}' not found: no secret store configured"))
        })
    }
    fn describe(&self) -> String {
        self.0
            .iter()
            .map(|s| s.describe())
            .collect::<Vec<_>>()
            .join(" then ")
    }
}

/// Nothing configured — every `secret()` errs (guest-visible DATA).
pub struct NoSecretStore;

impl SecretStore for NoSecretStore {
    fn get(&self, _name: &str) -> Result<String, String> {
        Err("no secret store configured on this engine".to_string())
    }
    fn describe(&self) -> String {
        "none".to_string()
    }
}

/// Build the store from the two operator options. File before env when both
/// are set (Layered); neither → NoSecretStore.
pub fn build(secrets_path: Option<String>, env_prefix: Option<String>) -> Arc<dyn SecretStore> {
    let mut stores: Vec<Arc<dyn SecretStore>> = Vec::new();
    if let Some(path) = secrets_path {
        stores.push(Arc::new(FileSecretStore { path }));
    }
    if let Some(prefix) = env_prefix {
        stores.push(Arc::new(EnvSecretStore { prefix }));
    }
    match stores.len() {
        0 => Arc::new(NoSecretStore),
        1 => stores.into_iter().next().expect("len==1"),
        _ => Arc::new(LayeredSecretStore(stores)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_store_reads_prefixed_var_and_errs_when_absent() {
        // Unique name so the process-global env can't collide with other tests.
        let store = EnvSecretStore {
            prefix: "KEEL_TEST_SECRET_A4_".to_string(),
        };
        std::env::set_var("KEEL_TEST_SECRET_A4_TOKEN", "sk-live-xyz");
        assert_eq!(store.get("TOKEN").unwrap(), "sk-live-xyz");
        assert!(store.get("ABSENT").is_err());
        std::env::remove_var("KEEL_TEST_SECRET_A4_TOKEN");
    }

    #[test]
    fn layered_resolves_first_hit_and_falls_through_errors() {
        let store = EnvSecretStore {
            prefix: "KEEL_TEST_SECRET_A4L_".to_string(),
        };
        // A layered store: an EnvSecretStore that misses, then one that hits.
        std::env::set_var("KEEL_TEST_SECRET_A4L2_HIT", "from-second");
        let layered = LayeredSecretStore(vec![
            Arc::new(store),
            Arc::new(EnvSecretStore {
                prefix: "KEEL_TEST_SECRET_A4L2_".to_string(),
            }),
        ]);
        assert_eq!(layered.get("HIT").unwrap(), "from-second");
        // none resolve → an error (the last one), not a panic.
        assert!(layered.get("NOPE").is_err());
        std::env::remove_var("KEEL_TEST_SECRET_A4L2_HIT");
    }

    #[test]
    fn no_store_always_errs_as_data() {
        assert!(NoSecretStore.get("anything").is_err());
    }

    #[test]
    fn build_picks_the_right_shape() {
        assert_eq!(build(None, None).describe(), "none");
        assert_eq!(
            build(None, Some("P_".to_string())).describe(),
            "env P_*"
        );
        assert_eq!(
            build(Some("/x".to_string()), Some("P_".to_string())).describe(),
            "file /x then env P_*"
        );
    }
}
