use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::LazyLock;
use std::time::Duration;

/// Runtime context passed to resolvers.
#[derive(Debug, Clone, Default)]
pub struct Ctx {
    pub cwd: Option<PathBuf>,
    pub env: HashMap<String, String>,
    /// Words on the current command line before the one being completed.
    pub prior_words: Vec<String>,
    /// The partial token the user has typed for the slot under the cursor.
    pub partial: String,
}

impl Ctx {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn with_partial(prefix: &str) -> Self {
        Self { partial: prefix.to_string(), ..Self::default() }
    }
}

pub trait TypeResolver: Send + Sync {
    /// Return all candidate values for this type under the given context.
    fn list(&self, ctx: &Ctx) -> Vec<String>;

    /// How long a cached result stays valid. `Duration::ZERO` disables caching.
    fn cache_ttl(&self) -> Duration {
        Duration::from_secs(5)
    }

    /// Resolver's stable identifier for cache keys and debug.
    fn id(&self) -> &'static str {
        ""
    }
}

/// Map of `ARG_MODE_*` → resolver.
pub struct Registry {
    resolvers: HashMap<u8, Box<dyn TypeResolver>>,
}

impl Registry {
    pub fn new() -> Self {
        Self { resolvers: HashMap::new() }
    }
    pub fn register(&mut self, mode: u8, resolver: Box<dyn TypeResolver>) {
        self.resolvers.insert(mode, resolver);
    }
    pub fn get(&self, mode: u8) -> Option<&dyn TypeResolver> {
        self.resolvers.get(&mode).map(|b| b.as_ref())
    }
    pub fn contains(&self, mode: u8) -> bool {
        self.resolvers.contains_key(&mode)
    }
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

pub static REGISTRY: LazyLock<Registry> = LazyLock::new(build_default_registry);

fn build_default_registry() -> Registry {
    let mut r = Registry::new();
    crate::runtime_complete::register_builtins(&mut r);
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeResolver(Vec<String>);
    impl TypeResolver for FakeResolver {
        fn list(&self, _ctx: &Ctx) -> Vec<String> {
            self.0.clone()
        }
        fn cache_ttl(&self) -> Duration {
            Duration::from_secs(30)
        }
        fn id(&self) -> &'static str {
            "fake"
        }
    }

    #[test]
    fn registry_register_and_get() {
        let mut r = Registry::new();
        r.register(99, Box::new(FakeResolver(vec!["a".into(), "b".into()])));
        assert!(r.contains(99));
        let items = r.get(99).unwrap().list(&Ctx::new());
        assert_eq!(items, vec!["a", "b"]);
    }

    #[test]
    fn registry_get_missing_returns_none() {
        let r = Registry::new();
        assert!(r.get(200).is_none());
    }

    #[test]
    fn ctx_with_partial() {
        let c = Ctx::with_partial("foo");
        assert_eq!(c.partial, "foo");
        assert!(c.prior_words.is_empty());
    }

    #[test]
    fn global_registry_has_builtins() {
        use crate::trie::*;
        for mode in [
            ARG_MODE_USERS,
            ARG_MODE_GROUPS,
            ARG_MODE_HOSTS,
            ARG_MODE_SIGNALS,
            ARG_MODE_PORTS,
            ARG_MODE_NET_IFACES,
            ARG_MODE_LOCALES,
            ARG_MODE_GIT_BRANCHES,
            ARG_MODE_GIT_TAGS,
            ARG_MODE_GIT_REMOTES,
            ARG_MODE_GIT_FILES,
            ARG_MODE_USERS_GROUPS,
            ARG_MODE_GIT_STASH,
            ARG_MODE_GIT_WORKTREE,
            ARG_MODE_GIT_SUBMODULE,
            ARG_MODE_GIT_CONFIG_KEY,
            ARG_MODE_GIT_ALIAS,
            ARG_MODE_GIT_COMMIT,
            ARG_MODE_GIT_REFLOG,
            // Docker
            ARG_MODE_DOCKER_CONTAINER,
            ARG_MODE_DOCKER_IMAGE,
            ARG_MODE_DOCKER_NETWORK,
            ARG_MODE_DOCKER_VOLUME,
            ARG_MODE_DOCKER_COMPOSE_SERVICE,
            // Kubernetes
            ARG_MODE_K8S_CONTEXT,
            ARG_MODE_K8S_NAMESPACE,
            ARG_MODE_K8S_POD,
            ARG_MODE_K8S_DEPLOYMENT,
            ARG_MODE_K8S_SERVICE,
            ARG_MODE_K8S_RESOURCE_KIND,
        ] {
            assert!(REGISTRY.contains(mode), "mode {} missing", mode);
        }
    }
}
