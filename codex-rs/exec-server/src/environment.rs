use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::RwLock;

use crate::ExecServerError;
use crate::ExecServerRuntimePaths;
use crate::ExecutorFileSystem;
use crate::HttpClient;
use crate::NoiseChannelIdentity;
use crate::NoiseRendezvousConnectProvider;
use crate::client::LazyRemoteExecServerClient;
use crate::client::http_client::ReqwestHttpClient;
use crate::client_api::DEFAULT_REMOTE_EXEC_SERVER_CONNECT_TIMEOUT;
use crate::client_api::ExecServerTransportParams;
use crate::client_api::StdioExecServerCommand;
use crate::environment_provider::DefaultEnvironmentProvider;
use crate::environment_provider::EnvironmentDefault;
use crate::environment_provider::EnvironmentProvider;
use crate::environment_provider::EnvironmentProviderSnapshot;
use crate::environment_provider::normalize_exec_server_url;
use crate::environment_toml::environment_provider_from_codex_home;
use crate::local_file_system::LocalFileSystem;
use crate::local_process::LocalProcess;
use crate::process::ExecBackend;
use crate::protocol::EnvironmentInfo;
use crate::protocol::ShellInfo;
use crate::provision::RemoteLauncher;
use crate::remote::NoiseRendezvousEnvironmentConfig;
use crate::remote_file_system::RemoteFileSystem;
use crate::remote_process::RemoteProcess;
use tokio_util::task::AbortOnDropHandle;

pub const CODEX_EXEC_SERVER_URL_ENV_VAR: &str = "CODEX_EXEC_SERVER_URL";
pub const CODEX_EXEC_SERVER_NOISE_REGISTRY_URL_ENV_VAR: &str =
    "CODEX_EXEC_SERVER_NOISE_REGISTRY_URL";
pub const CODEX_EXEC_SERVER_NOISE_ENVIRONMENT_ID_ENV_VAR: &str =
    "CODEX_EXEC_SERVER_NOISE_ENVIRONMENT_ID";
pub const CODEX_EXEC_SERVER_NOISE_AUTH_TOKEN_ENV_VAR: &str = "CODEX_EXEC_SERVER_NOISE_AUTH_TOKEN";
pub const CODEX_EXEC_SERVER_NOISE_CHATGPT_ACCOUNT_ID_ENV_VAR: &str =
    "CODEX_EXEC_SERVER_NOISE_CHATGPT_ACCOUNT_ID";

/// Maximum number of entries in the per-environment metadata map.
///
/// This is a soft guard: environments registered by `env_switch` live for the
/// lifetime of the parent session (there is no teardown / env_drop today), so
/// the map can grow without bound inside a very long session.  Capping at 64
/// makes runaway growth immediately visible rather than silently leaking
/// memory.  A follow-up issue should implement proper env_drop / teardown.
const MAX_ENV_METADATA_ENTRIES: usize = 256;

/// Metadata associated with a dynamically-registered remote environment.
///
/// Stored inside [`EnvironmentManager`] so the data is accessible to
/// sub-agents that share the same `Arc<EnvironmentManager>` but run in a
/// separate [`Session`] (and therefore have separate `SessionServices`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvironmentMetadata {
    /// Working directory that `env_switch` created / confirmed exists on the
    /// remote host.  Expressed as a raw absolute-path string (the caller is
    /// responsible for converting to `AbsolutePathBuf`).
    pub cwd: String,
    /// Preferred shell path on the remote host (e.g. `/bin/bash`), as
    /// reported by the probe script.  `None` when the remote probe did not
    /// emit a `CODEX_SHELL:` line.
    pub shell: Option<String>,
}

/// Side-effect-free snapshot of an environment registry entry.
///
/// This intentionally avoids calling [`Environment::info`], because status
/// checks should not connect to remote exec-servers just to list known ids.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvironmentSnapshot {
    /// Stable id used by tool calls as `environment_id`.
    pub environment_id: String,
    /// True when the environment is backed by a remote exec-server transport.
    pub is_remote: bool,
    /// True when this is the manager's configured default environment.
    pub is_default: bool,
    /// Global metadata recorded for this environment id, if any.
    pub metadata: Option<EnvironmentMetadata>,
}

/// Owns the execution/filesystem environments available to the Codex runtime.
///
/// `EnvironmentManager` is a shared registry for concrete environments. Its
/// default constructor preserves the legacy `CODEX_EXEC_SERVER_URL` behavior
/// while configured construction accepts a provider-supplied snapshot.
///
/// Setting `CODEX_EXEC_SERVER_URL=none` disables environment access by leaving
/// the default environment unset and omitting the local environment. Callers
/// use `default_environment().is_some()` as the signal for model-facing
/// shell/filesystem tool availability.
///
/// Remote environments begin connecting when added to the manager. Their
/// filesystem and execution backends share that startup result and reconnect
/// after later disconnects as needed.
#[derive(Debug)]
pub struct EnvironmentManager {
    default_environment: Option<String>,
    pub(super) environments: RwLock<HashMap<String, Arc<Environment>>>,
    local_environment: Option<Arc<Environment>>,
    local_runtime_paths: Option<ExecServerRuntimePaths>,
    /// Per-environment metadata (cwd, shell) registered by `env_switch`.
    ///
    /// Stored here (rather than in per-session `SessionServices`) so that
    /// sub-agents that share this `Arc<EnvironmentManager>` can look up
    /// metadata that the parent session registered.
    ///
    /// See [`MAX_ENV_METADATA_ENTRIES`] for the soft size cap.
    env_metadata: Mutex<HashMap<String, EnvironmentMetadata>>,
    /// Per-thread metadata for dynamically-registered environments.
    ///
    /// The same environment id can be registered by different threads with
    /// different cwd/shell metadata.  Keep this map thread-scoped so a shared
    /// manager does not let one thread overwrite another thread's cwd.
    thread_env_metadata: Mutex<HashMap<String, HashMap<String, EnvironmentMetadata>>>,
    /// The most-recently registered [`RemoteLauncher`] per session-thread id
    /// string (opaque key, typically `ThreadId::to_string()`).
    ///
    /// Updated every time `env_switch` successfully registers a remote
    /// environment.  Used as the base launcher when `env_switch` is called in
    /// relative mode (`extend` present, `base` absent).  Stored here so
    /// sub-agents share the parent's cursor.
    ///
    /// See [`MAX_ENV_METADATA_ENTRIES`] for the soft size cap.
    last_launcher: Mutex<HashMap<String, RemoteLauncher>>,
    /// The most-recent environment id successfully selected via `env_switch`
    /// per session-thread id. Unlike [`Self::last_launcher`], this includes
    /// `local`, so status tools can report what the model last asked for.
    last_environment_id: Mutex<HashMap<String, String>>,
    /// Environment ids registered or explicitly selected via `env_switch`,
    /// keyed by session-thread id. Used by status tools to show only
    /// thread-relevant dynamic environments instead of the whole shared
    /// registry.
    thread_environment_ids: Mutex<HashMap<String, Vec<String>>>,
}

pub const LOCAL_ENVIRONMENT_ID: &str = "local";
pub const REMOTE_ENVIRONMENT_ID: &str = "remote";

impl EnvironmentManager {
    /// Builds a test-only manager without configured sandbox helper paths.
    pub fn default_for_tests() -> Self {
        Self {
            default_environment: Some(LOCAL_ENVIRONMENT_ID.to_string()),
            environments: RwLock::new(HashMap::from([(
                LOCAL_ENVIRONMENT_ID.to_string(),
                Arc::new(Environment::default_for_tests()),
            )])),
            local_environment: Some(Arc::new(Environment::default_for_tests())),
            local_runtime_paths: None,
            env_metadata: Mutex::new(HashMap::new()),
            thread_env_metadata: Mutex::new(HashMap::new()),
            last_launcher: Mutex::new(HashMap::new()),
            last_environment_id: Mutex::new(HashMap::new()),
            thread_environment_ids: Mutex::new(HashMap::new()),
        }
    }

    /// Builds a manager with no configured execution environments.
    pub fn without_environments() -> Self {
        Self {
            default_environment: None,
            environments: RwLock::new(HashMap::new()),
            local_environment: None,
            local_runtime_paths: None,
            env_metadata: Mutex::new(HashMap::new()),
            thread_env_metadata: Mutex::new(HashMap::new()),
            last_launcher: Mutex::new(HashMap::new()),
            last_environment_id: Mutex::new(HashMap::new()),
            thread_environment_ids: Mutex::new(HashMap::new()),
        }
    }

    /// Builds a test-only manager from a raw exec-server URL value.
    pub async fn create_for_tests(
        exec_server_url: Option<String>,
        local_runtime_paths: Option<ExecServerRuntimePaths>,
    ) -> Self {
        Self::from_default_provider_url(exec_server_url, local_runtime_paths).await
    }

    /// Builds a manager from `CODEX_HOME` and local runtime paths used when
    /// creating local filesystem helpers.
    ///
    /// If `CODEX_HOME/environments.toml` is present, it defines the configured
    /// environments. Otherwise this preserves the legacy
    /// `CODEX_EXEC_SERVER_URL` behavior.
    pub async fn from_codex_home(
        codex_home: impl AsRef<std::path::Path>,
        local_runtime_paths: Option<ExecServerRuntimePaths>,
    ) -> Result<Self, ExecServerError> {
        if let Some(config) = noise_environment_config_from_env()? {
            return Self::from_noise_environment_config(config, local_runtime_paths);
        }
        let provider = environment_provider_from_codex_home(codex_home.as_ref())?;
        Self::from_snapshot(provider.snapshot().await?, local_runtime_paths)
    }

    /// Builds a manager from the legacy environment-variable provider without
    /// reading user config files from `CODEX_HOME`.
    pub async fn from_env(
        local_runtime_paths: Option<ExecServerRuntimePaths>,
    ) -> Result<Self, ExecServerError> {
        if let Some(config) = noise_environment_config_from_env()? {
            return Self::from_noise_environment_config(config, local_runtime_paths);
        }
        let provider = DefaultEnvironmentProvider::from_env();
        Self::from_snapshot(provider.snapshot().await?, local_runtime_paths)
    }

    async fn from_default_provider_url(
        exec_server_url: Option<String>,
        local_runtime_paths: Option<ExecServerRuntimePaths>,
    ) -> Self {
        let provider = DefaultEnvironmentProvider::new(exec_server_url);
        match Self::from_snapshot(provider.snapshot_inner(), local_runtime_paths) {
            Ok(manager) => manager,
            Err(err) => panic!("default provider should create valid environments: {err}"),
        }
    }

    fn from_noise_environment_config(
        config: NoiseRendezvousEnvironmentConfig,
        local_runtime_paths: Option<ExecServerRuntimePaths>,
    ) -> Result<Self, ExecServerError> {
        let manager = Self {
            default_environment: Some(REMOTE_ENVIRONMENT_ID.to_string()),
            environments: RwLock::new(HashMap::new()),
            local_environment: None,
            local_runtime_paths,
            env_metadata: Mutex::new(HashMap::new()),
            thread_env_metadata: Mutex::new(HashMap::new()),
            last_launcher: Mutex::new(HashMap::new()),
            last_environment_id: Mutex::new(HashMap::new()),
            thread_environment_ids: Mutex::new(HashMap::new()),
        };
        manager.upsert_noise_environment(
            REMOTE_ENVIRONMENT_ID.to_string(),
            config.connect_provider(),
        )?;
        Ok(manager)
    }

    /// Builds a test-only manager that keeps the provider default while also
    /// allowing tests to select the local environment explicitly.
    pub async fn create_for_tests_with_local(
        exec_server_url: Option<String>,
        local_runtime_paths: ExecServerRuntimePaths,
    ) -> Self {
        let mut snapshot = DefaultEnvironmentProvider::new(exec_server_url).snapshot_inner();
        snapshot.include_local = true;
        match Self::from_snapshot(snapshot, Some(local_runtime_paths)) {
            Ok(manager) => manager,
            Err(err) => panic!("test provider with local should create valid environments: {err}"),
        }
    }

    fn from_snapshot(
        snapshot: EnvironmentProviderSnapshot,
        local_runtime_paths: Option<ExecServerRuntimePaths>,
    ) -> Result<Self, ExecServerError> {
        let EnvironmentProviderSnapshot {
            environments,
            default,
            include_local,
        } = snapshot;
        let mut environment_map =
            HashMap::with_capacity(environments.len() + usize::from(include_local));
        let local_environment = if include_local {
            let local_runtime_paths = local_runtime_paths.clone().ok_or_else(|| {
                ExecServerError::Protocol(
                    "local environment requires configured runtime paths".to_string(),
                )
            })?;
            let local_environment = Arc::new(Environment::local(local_runtime_paths));
            environment_map.insert(
                LOCAL_ENVIRONMENT_ID.to_string(),
                Arc::clone(&local_environment),
            );
            Some(local_environment)
        } else {
            None
        };
        for (id, environment) in environments {
            if id.is_empty() {
                return Err(ExecServerError::Protocol(
                    "environment id cannot be empty".to_string(),
                ));
            }
            if id == LOCAL_ENVIRONMENT_ID {
                return Err(ExecServerError::Protocol(format!(
                    "environment id `{LOCAL_ENVIRONMENT_ID}` is reserved for EnvironmentManager"
                )));
            }
            if environment_map
                .insert(id.clone(), Arc::new(environment))
                .is_some()
            {
                return Err(ExecServerError::Protocol(format!(
                    "environment id `{id}` is duplicated"
                )));
            }
        }
        let default_environment = match default {
            EnvironmentDefault::Disabled => None,
            EnvironmentDefault::EnvironmentId(environment_id) => {
                if !environment_map.contains_key(&environment_id) {
                    return Err(ExecServerError::Protocol(format!(
                        "default environment `{environment_id}` is not configured"
                    )));
                }
                Some(environment_id)
            }
        };
        // The snapshot is valid; start connecting its remote environments in the background.
        for environment in environment_map.values() {
            environment.start_connecting();
        }
        Ok(Self {
            default_environment,
            environments: RwLock::new(environment_map),
            local_environment,
            local_runtime_paths,
            env_metadata: Mutex::new(HashMap::new()),
            thread_env_metadata: Mutex::new(HashMap::new()),
            last_launcher: Mutex::new(HashMap::new()),
            last_environment_id: Mutex::new(HashMap::new()),
            thread_environment_ids: Mutex::new(HashMap::new()),
        })
    }

    /// Returns the default environment instance.
    pub fn default_environment(&self) -> Option<Arc<Environment>> {
        self.default_environment
            .as_deref()
            .and_then(|environment_id| self.get_environment(environment_id))
    }

    /// Returns the id of the default environment.
    pub fn default_environment_id(&self) -> Option<&str> {
        self.default_environment.as_deref()
    }

    /// Returns the ordered environment ids used for new thread startup.
    pub fn default_environment_ids(&self) -> Vec<String> {
        let Some(default_environment_id) = self.default_environment.as_ref() else {
            return Vec::new();
        };
        let environments = self
            .environments
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut environment_ids = Vec::with_capacity(environments.len());
        environment_ids.push(default_environment_id.clone());
        environment_ids.extend(
            environments
                .keys()
                .filter(|environment_id| *environment_id != default_environment_id)
                .cloned(),
        );
        environment_ids
    }

    /// Returns the local environment instance when one is configured.
    pub fn try_local_environment(&self) -> Option<Arc<Environment>> {
        self.local_environment.as_ref().map(Arc::clone)
    }

    /// Returns the default environment or local environment when either exists.
    pub fn default_or_local_environment(&self) -> Option<Arc<Environment>> {
        self.default_environment()
            .or_else(|| self.try_local_environment())
    }

    /// Returns a named environment instance.
    pub fn get_environment(&self, environment_id: &str) -> Option<Arc<Environment>> {
        self.environments
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(environment_id)
            .cloned()
    }

    /// Returns a deterministic snapshot of the registered environment ids.
    ///
    /// The default environment is listed first when present, followed by the
    /// remaining environment ids in lexical order. Dynamic metadata is copied
    /// from the side map populated by `env_switch`.
    pub fn environment_snapshots(&self) -> Vec<EnvironmentSnapshot> {
        let default_environment_id = self.default_environment.clone();
        let mut entries = {
            let environments = self
                .environments
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            environments
                .iter()
                .map(|(environment_id, environment)| {
                    (environment_id.clone(), environment.is_remote())
                })
                .collect::<Vec<_>>()
        };
        let metadata = self
            .env_metadata
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();

        entries.sort_by(|(left_id, _), (right_id, _)| {
            match (
                default_environment_id.as_ref() == Some(left_id),
                default_environment_id.as_ref() == Some(right_id),
            ) {
                (true, false) => std::cmp::Ordering::Less,
                (false, true) => std::cmp::Ordering::Greater,
                _ => left_id.cmp(right_id),
            }
        });

        entries
            .into_iter()
            .map(|(environment_id, is_remote)| {
                let is_default = default_environment_id.as_deref() == Some(environment_id.as_str());
                let metadata = metadata.get(&environment_id).cloned();
                EnvironmentSnapshot {
                    environment_id,
                    is_remote,
                    is_default,
                    metadata,
                }
            })
            .collect()
    }

    /// Adds or replaces a named remote environment without changing the
    /// manager's default environment selection. Uses the default WebSocket
    /// connection timeout when none is provided.
    pub fn upsert_environment(
        &self,
        environment_id: String,
        exec_server_url: String,
        connect_timeout: Option<std::time::Duration>,
    ) -> Result<(), ExecServerError> {
        if environment_id.is_empty() {
            return Err(ExecServerError::Protocol(
                "environment id cannot be empty".to_string(),
            ));
        }
        let (exec_server_url, disabled) = normalize_exec_server_url(Some(exec_server_url));
        if disabled {
            return Err(ExecServerError::Protocol(
                "remote environment cannot use disabled exec-server url".to_string(),
            ));
        }
        let Some(exec_server_url) = exec_server_url else {
            return Err(ExecServerError::Protocol(
                "remote environment requires an exec-server url".to_string(),
            ));
        };
        let environment = Arc::new(Environment::remote_with_transport(
            ExecServerTransportParams::websocket_url(
                exec_server_url,
                connect_timeout.unwrap_or(DEFAULT_REMOTE_EXEC_SERVER_CONNECT_TIMEOUT),
            ),
            self.local_runtime_paths.clone(),
        ));
        environment.start_connecting();
        self.environments
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(environment_id, environment);
        Ok(())
    }

    /// Adds or replaces a named remote environment that connects through an
    /// authenticated, end-to-end encrypted rendezvous stream.
    ///
    /// The provider is retained so every reconnect obtains fresh authorization.
    /// This transport never falls back to the URL-only remote environment path.
    pub fn upsert_noise_environment(
        &self,
        environment_id: String,
        provider: Arc<dyn NoiseRendezvousConnectProvider>,
    ) -> Result<(), ExecServerError> {
        if environment_id.is_empty() {
            return Err(ExecServerError::Protocol(
                "environment id cannot be empty".to_string(),
            ));
        }
        let identity = NoiseChannelIdentity::generate().map_err(|error| {
            ExecServerError::Protocol(format!(
                "failed to generate Noise harness identity: {error}"
            ))
        })?;
        let environment = Arc::new(Environment::remote_with_transport(
            ExecServerTransportParams::NoiseRendezvous { provider, identity },
            self.local_runtime_paths.clone(),
        ));
        environment.start_connecting();
        self.environments
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(environment_id, environment);
        Ok(())
    }

    /// Records cwd and shell metadata for a dynamically-registered environment.
    ///
    /// Call this **before** [`Self::upsert_stdio_environment`] so that any
    /// concurrent lookup immediately after `upsert` finds correct values.
    /// Both operations together are not atomic, but recording metadata first
    /// eliminates the window where an environment is registered without cwd/shell.
    ///
    /// If the map already holds [`MAX_ENV_METADATA_ENTRIES`] entries for other
    /// ids, an arbitrary entry is evicted to cap memory growth.  Active
    /// environments will re-register metadata on the next `env_switch` call.
    ///
    /// Note: remote `~/.codex/bin` cleanup and connection teardown (`env_drop`)
    /// are not yet implemented; this is a known follow-up item.
    pub fn set_environment_metadata(&self, environment_id: String, metadata: EnvironmentMetadata) {
        let mut map = self
            .env_metadata
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if map.len() >= MAX_ENV_METADATA_ENTRIES && !map.contains_key(&environment_id) {
            // Evict an arbitrary entry to stay within the cap.
            if let Some(oldest_key) = map.keys().next().cloned() {
                map.remove(&oldest_key);
            }
        }
        map.insert(environment_id, metadata);
    }

    /// Returns the metadata for a dynamically-registered environment, if any.
    pub fn get_environment_metadata(&self, environment_id: &str) -> Option<EnvironmentMetadata> {
        self.env_metadata
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(environment_id)
            .cloned()
    }

    /// Records cwd and shell metadata for one thread's view of an environment.
    pub fn set_thread_environment_metadata(
        &self,
        thread_key: String,
        environment_id: String,
        metadata: EnvironmentMetadata,
    ) {
        let mut map = self
            .thread_env_metadata
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if map.len() >= MAX_ENV_METADATA_ENTRIES
            && !map.contains_key(&thread_key)
            && let Some(oldest_key) = map.keys().next().cloned()
        {
            map.remove(&oldest_key);
        }
        let entries = map.entry(thread_key).or_default();
        if entries.len() >= MAX_ENV_METADATA_ENTRIES
            && !entries.contains_key(&environment_id)
            && let Some(oldest_key) = entries.keys().next().cloned()
        {
            entries.remove(&oldest_key);
        }
        entries.insert(environment_id, metadata);
    }

    /// Returns metadata for an environment visible through any of the supplied
    /// thread keys, checking keys in order.
    pub fn get_thread_environment_metadata_for_keys(
        &self,
        thread_keys: &[String],
        environment_id: &str,
    ) -> Option<EnvironmentMetadata> {
        let map = self
            .thread_env_metadata
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        thread_keys
            .iter()
            .find_map(|thread_key| map.get(thread_key)?.get(environment_id).cloned())
    }

    /// Records the most-recently registered [`RemoteLauncher`] for a given
    /// thread key (typically `thread_id.to_string()`).
    ///
    /// Used by `env_switch`'s relative mode (`extend` present, `base` absent).
    /// Keyed by thread id so parallel threads each maintain an independent cursor.
    ///
    /// Subject to the same [`MAX_ENV_METADATA_ENTRIES`] soft cap as metadata.
    pub fn set_last_launcher(&self, thread_key: String, launcher: RemoteLauncher) {
        let mut map = self
            .last_launcher
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if map.len() >= MAX_ENV_METADATA_ENTRIES
            && !map.contains_key(&thread_key)
            && let Some(oldest_key) = map.keys().next().cloned()
        {
            map.remove(&oldest_key);
        }
        map.insert(thread_key, launcher);
    }

    /// Returns the most-recently registered [`RemoteLauncher`] for a given
    /// thread key, if any.
    pub fn get_last_launcher(&self, thread_key: &str) -> Option<RemoteLauncher> {
        self.last_launcher
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(thread_key)
            .cloned()
    }

    /// Clears the implicit relative-mode launcher cursor for a thread.
    pub fn clear_last_launcher(&self, thread_key: &str) {
        self.last_launcher
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(thread_key);
    }

    /// Records the most recent environment id selected through `env_switch`.
    ///
    /// Tool handlers use this thread-scoped cursor as the effective default
    /// environment for compatible calls that omit `environment_id`.
    pub fn set_last_environment_id(&self, thread_key: String, environment_id: String) {
        let mut map = self
            .last_environment_id
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if map.len() >= MAX_ENV_METADATA_ENTRIES
            && !map.contains_key(&thread_key)
            && let Some(oldest_key) = map.keys().next().cloned()
        {
            map.remove(&oldest_key);
        }
        map.insert(thread_key, environment_id);
    }

    /// Returns the most recent environment id selected through `env_switch`.
    pub fn get_last_environment_id(&self, thread_key: &str) -> Option<String> {
        self.last_environment_id
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(thread_key)
            .cloned()
    }

    /// Records that an environment id is relevant to a given thread.
    ///
    /// This is used by status tools to list dynamic environments created by
    /// the current thread (or inherited from a parent thread) without exposing
    /// unrelated environments from the shared registry.
    pub fn record_thread_environment_id(&self, thread_key: String, environment_id: String) {
        let mut map = self
            .thread_environment_ids
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if map.len() >= MAX_ENV_METADATA_ENTRIES
            && !map.contains_key(&thread_key)
            && let Some(oldest_key) = map.keys().next().cloned()
        {
            map.remove(&oldest_key);
        }
        let ids = map.entry(thread_key).or_default();
        if !ids.iter().any(|id| id == &environment_id) {
            if ids.len() >= MAX_ENV_METADATA_ENTRIES {
                ids.remove(0);
            }
            ids.push(environment_id);
        }
    }

    /// Returns environment ids recorded for a given thread.
    pub fn get_thread_environment_ids(&self, thread_key: &str) -> Vec<String> {
        self.thread_environment_ids
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(thread_key)
            .cloned()
            .unwrap_or_default()
    }

    /// Adds or replaces a named remote environment backed by a stdio command
    /// (e.g. `docker exec -i <c> <codex> exec-server --listen stdio`).
    ///
    /// This is the stdio counterpart to [`upsert_environment`] which only
    /// accepts WebSocket URLs.  Use this when the remote exec-server is reached
    /// through an arbitrary subprocess whose stdin/stdout carry the JSON-RPC
    /// protocol.
    pub fn upsert_stdio_environment(
        &self,
        environment_id: String,
        program: String,
        args: Vec<String>,
        env: HashMap<String, String>,
        cwd: Option<PathBuf>,
    ) -> Result<(), ExecServerError> {
        if environment_id.is_empty() {
            return Err(ExecServerError::Protocol(
                "environment id cannot be empty".to_string(),
            ));
        }
        let command = StdioExecServerCommand {
            program,
            args,
            env,
            cwd,
        };
        let transport = ExecServerTransportParams::StdioCommand {
            command,
            initialize_timeout: crate::client_api::PROVISIONED_STDIO_INITIALIZE_TIMEOUT,
        };
        let environment =
            Environment::remote_with_transport(transport, self.local_runtime_paths.clone());
        self.environments
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(environment_id, Arc::new(environment));
        Ok(())
    }
}

fn noise_environment_config_from_env()
-> Result<Option<NoiseRendezvousEnvironmentConfig>, ExecServerError> {
    noise_environment_config_from_values(
        optional_environment_value(CODEX_EXEC_SERVER_NOISE_REGISTRY_URL_ENV_VAR),
        optional_environment_value(CODEX_EXEC_SERVER_NOISE_ENVIRONMENT_ID_ENV_VAR),
        optional_environment_value(CODEX_EXEC_SERVER_NOISE_AUTH_TOKEN_ENV_VAR),
        optional_environment_value(CODEX_EXEC_SERVER_NOISE_CHATGPT_ACCOUNT_ID_ENV_VAR),
    )
}

fn noise_environment_config_from_values(
    registry_url: Option<String>,
    environment_id: Option<String>,
    auth_token: Option<String>,
    chatgpt_account_id: Option<String>,
) -> Result<Option<NoiseRendezvousEnvironmentConfig>, ExecServerError> {
    let (registry_url, environment_id, auth_token) =
        match (registry_url, environment_id, auth_token) {
            (None, None, None) => return Ok(None),
            (Some(registry_url), Some(environment_id), Some(auth_token)) => {
                (registry_url, environment_id, auth_token)
            }
            _ => {
                return Err(ExecServerError::EnvironmentRegistryConfig(format!(
                    "Noise environment requires {CODEX_EXEC_SERVER_NOISE_REGISTRY_URL_ENV_VAR}, \
{CODEX_EXEC_SERVER_NOISE_ENVIRONMENT_ID_ENV_VAR}, and \
{CODEX_EXEC_SERVER_NOISE_AUTH_TOKEN_ENV_VAR}"
                )));
            }
        };

    let config = NoiseRendezvousEnvironmentConfig::new(
        registry_url,
        environment_id,
        auth_token,
        chatgpt_account_id,
    )?;
    Ok(Some(config))
}

fn optional_environment_value(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// Concrete execution/filesystem environment selected for a session.
///
/// This bundles the selected backend metadata together with the local runtime
/// paths used by filesystem helpers.
#[derive(Clone)]
pub struct Environment {
    exec_server_url: Option<String>,
    remote_client: Option<LazyRemoteExecServerClient>,
    // Dropping the environment stops unfinished background startup work.
    startup_task: Arc<Mutex<Option<AbortOnDropHandle<()>>>>,
    exec_backend: Arc<dyn ExecBackend>,
    filesystem: Arc<dyn ExecutorFileSystem>,
    http_client: Arc<dyn HttpClient>,
    local_runtime_paths: Option<ExecServerRuntimePaths>,
}

impl Environment {
    /// Builds a test-only local environment without configured sandbox helper paths.
    pub fn default_for_tests() -> Self {
        Self {
            exec_server_url: None,
            remote_client: None,
            startup_task: Arc::new(Mutex::new(None)),
            exec_backend: Arc::new(LocalProcess::default()),
            filesystem: Arc::new(LocalFileSystem::unsandboxed()),
            http_client: Arc::new(ReqwestHttpClient),
            local_runtime_paths: None,
        }
    }
}

impl std::fmt::Debug for Environment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Environment")
            .field("exec_server_url", &self.exec_server_url)
            .finish_non_exhaustive()
    }
}

impl Environment {
    /// Builds an environment from the raw `CODEX_EXEC_SERVER_URL` value.
    pub fn create(
        exec_server_url: Option<String>,
        local_runtime_paths: ExecServerRuntimePaths,
    ) -> Result<Self, ExecServerError> {
        Self::create_inner(exec_server_url, Some(local_runtime_paths))
    }

    /// Builds a test-only environment without configured sandbox helper paths.
    pub fn create_for_tests(exec_server_url: Option<String>) -> Result<Self, ExecServerError> {
        Self::create_inner(exec_server_url, /*local_runtime_paths*/ None)
    }

    /// Builds an environment from the raw `CODEX_EXEC_SERVER_URL` value and
    /// local runtime paths used when creating local filesystem helpers.
    fn create_inner(
        exec_server_url: Option<String>,
        local_runtime_paths: Option<ExecServerRuntimePaths>,
    ) -> Result<Self, ExecServerError> {
        let (exec_server_url, disabled) = normalize_exec_server_url(exec_server_url);
        if disabled {
            return Err(ExecServerError::Protocol(
                "disabled mode does not create an Environment".to_string(),
            ));
        }

        Ok(match exec_server_url {
            Some(exec_server_url) => Self::remote_inner(exec_server_url, local_runtime_paths),
            None => match local_runtime_paths {
                Some(local_runtime_paths) => Self::local(local_runtime_paths),
                None => Self::default_for_tests(),
            },
        })
    }

    pub(crate) fn local(local_runtime_paths: ExecServerRuntimePaths) -> Self {
        Self {
            exec_server_url: None,
            remote_client: None,
            startup_task: Arc::new(Mutex::new(None)),
            exec_backend: Arc::new(LocalProcess::with_local_runtime_paths(
                local_runtime_paths.clone(),
            )),
            filesystem: Arc::new(LocalFileSystem::with_runtime_paths(
                local_runtime_paths.clone(),
            )),
            http_client: Arc::new(ReqwestHttpClient),
            local_runtime_paths: Some(local_runtime_paths),
        }
    }

    pub(crate) fn remote_inner(
        exec_server_url: String,
        local_runtime_paths: Option<ExecServerRuntimePaths>,
    ) -> Self {
        Self::remote_with_transport(
            ExecServerTransportParams::websocket_url(
                exec_server_url,
                DEFAULT_REMOTE_EXEC_SERVER_CONNECT_TIMEOUT,
            ),
            local_runtime_paths,
        )
    }

    pub(crate) fn remote_with_transport(
        remote_transport: ExecServerTransportParams,
        local_runtime_paths: Option<ExecServerRuntimePaths>,
    ) -> Self {
        let exec_server_url = match &remote_transport {
            ExecServerTransportParams::WebSocketUrl {
                websocket_url: exec_server_url,
                ..
            } => Some(exec_server_url.clone()),
            ExecServerTransportParams::NoiseRendezvous { .. } => None,
            ExecServerTransportParams::StdioCommand { .. } => None,
        };
        let client = LazyRemoteExecServerClient::new(remote_transport);
        let exec_backend: Arc<dyn ExecBackend> = Arc::new(RemoteProcess::new(client.clone()));
        let filesystem: Arc<dyn ExecutorFileSystem> =
            Arc::new(RemoteFileSystem::new(client.clone()));

        Self {
            exec_server_url,
            remote_client: Some(client.clone()),
            startup_task: Arc::new(Mutex::new(None)),
            exec_backend,
            filesystem,
            http_client: Arc::new(client),
            local_runtime_paths,
        }
    }

    pub fn is_remote(&self) -> bool {
        self.remote_client.is_some()
    }

    /// Returns the remote exec-server URL when this environment is remote.
    pub fn exec_server_url(&self) -> Option<&str> {
        self.exec_server_url.as_deref()
    }

    pub fn local_runtime_paths(&self) -> Option<&ExecServerRuntimePaths> {
        self.local_runtime_paths.as_ref()
    }

    /// Returns environment information from the selected execution/filesystem environment.
    pub async fn info(&self) -> Result<EnvironmentInfo, ExecServerError> {
        match &self.remote_client {
            Some(client) => client.environment_info().await,
            None => Ok(EnvironmentInfo::local()),
        }
    }

    /// Starts connecting a remote environment without waiting for it.
    /// Requires an active Tokio runtime when background startup is supported.
    pub fn start_connecting(&self) {
        let Some(client) = &self.remote_client else {
            return;
        };
        let mut startup_task = self
            .startup_task
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if startup_task.is_none() {
            *startup_task = client.start_connecting();
        }
    }

    /// Starts the initial connection after an environment is actually selected for use.
    pub(crate) fn start_connecting_for_use(environment: &Arc<Self>) {
        if environment.remote_client.is_none() {
            return;
        }
        let mut startup_task = environment
            .startup_task
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if startup_task.is_none() {
            let environment = Arc::clone(environment);
            *startup_task = Some(AbortOnDropHandle::new(tokio::spawn(async move {
                if let Err(error) = environment.wait_until_ready().await {
                    tracing::debug!(%error, "exec-server environment startup failed");
                }
            })));
        }
    }

    /// Returns whether initial startup has either succeeded or permanently failed.
    pub fn startup_finished(&self) -> bool {
        self.remote_client
            .as_ref()
            .is_none_or(LazyRemoteExecServerClient::startup_finished)
    }

    /// Waits for initial startup. A failed startup is never attempted again.
    pub async fn wait_until_ready(&self) -> Result<(), ExecServerError> {
        match &self.remote_client {
            Some(client) => client.wait_until_ready().await,
            None => Ok(()),
        }
    }

    pub fn get_exec_backend(&self) -> Arc<dyn ExecBackend> {
        Arc::clone(&self.exec_backend)
    }

    pub fn get_http_client(&self) -> Arc<dyn HttpClient> {
        Arc::clone(&self.http_client)
    }

    pub fn get_filesystem(&self) -> Arc<dyn ExecutorFileSystem> {
        Arc::clone(&self.filesystem)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::time::Duration;

    use super::Environment;
    use super::EnvironmentManager;
    use super::EnvironmentMetadata;
    use super::LOCAL_ENVIRONMENT_ID;
    use super::REMOTE_ENVIRONMENT_ID;
    use super::noise_environment_config_from_values;
    use crate::ExecServerRuntimePaths;
    use crate::ProcessId;
    use crate::client_api::ExecServerTransportParams;
    use crate::client_api::StdioExecServerCommand;
    use crate::environment_provider::EnvironmentDefault;
    use crate::environment_provider::EnvironmentProviderSnapshot;
    use codex_utils_path_uri::PathUri;
    use pretty_assertions::assert_eq;
    use tokio::net::TcpListener;
    use tokio::time::timeout;

    fn test_runtime_paths() -> ExecServerRuntimePaths {
        ExecServerRuntimePaths::new(
            std::env::current_exe().expect("current exe"),
            /*codex_linux_sandbox_exe*/ None,
        )
        .expect("runtime paths")
    }

    fn assert_local_environment_unavailable(manager: &EnvironmentManager) {
        assert!(manager.try_local_environment().is_none());
    }

    #[test]
    fn local_environment_info_includes_current_directory() {
        let info = super::EnvironmentInfo::local();

        assert_eq!(
            info.cwd,
            Some(
                PathUri::from_host_native_path(std::env::current_dir().expect("current directory"))
                    .expect("cwd URI")
            )
        );
    }

    #[tokio::test]
    async fn noise_environment_config_selects_remote_as_default() {
        let config = noise_environment_config_from_values(
            Some("http://registry.example/api".to_string()),
            Some("environment-requested".to_string()),
            Some("registry-token".to_string()),
            Some("workspace-123".to_string()),
        )
        .expect("parse noise environment configuration")
        .expect("noise environment configuration");

        let manager = EnvironmentManager::from_noise_environment_config(
            config, /*local_runtime_paths*/ None,
        )
        .expect("build environment manager");

        assert_eq!(
            manager.default_environment_id(),
            Some(REMOTE_ENVIRONMENT_ID)
        );
        assert!(
            manager
                .default_environment()
                .expect("remote environment")
                .is_remote()
        );
        assert_local_environment_unavailable(&manager);
    }

    #[tokio::test]
    async fn create_local_environment_does_not_connect() {
        let environment = Environment::create(/*exec_server_url*/ None, test_runtime_paths())
            .expect("create environment");

        assert_eq!(environment.exec_server_url(), None);
        assert!(!environment.is_remote());
        assert!(environment.info().await.is_ok());
    }

    #[tokio::test]
    async fn environment_manager_normalizes_empty_url() {
        let manager =
            EnvironmentManager::create_for_tests(Some(String::new()), Some(test_runtime_paths()))
                .await;

        let environment = manager.default_environment().expect("default environment");
        assert_eq!(manager.default_environment_id(), Some(LOCAL_ENVIRONMENT_ID));
        assert!(Arc::ptr_eq(
            &environment,
            &manager
                .get_environment(LOCAL_ENVIRONMENT_ID)
                .expect("local environment")
        ));
        assert!(Arc::ptr_eq(
            &environment,
            &manager.try_local_environment().expect("local environment")
        ));
        assert!(manager.try_local_environment().is_some());
        assert!(manager.get_environment(REMOTE_ENVIRONMENT_ID).is_none());
        assert!(!environment.is_remote());
    }

    #[tokio::test]
    async fn disabled_environment_manager_has_no_default_or_local_environment() {
        let manager = EnvironmentManager::without_environments();

        assert!(manager.default_environment().is_none());
        assert_eq!(manager.default_environment_id(), None);
        assert_local_environment_unavailable(&manager);
        assert!(manager.get_environment(LOCAL_ENVIRONMENT_ID).is_none());
        assert!(manager.get_environment(REMOTE_ENVIRONMENT_ID).is_none());
    }

    #[tokio::test]
    async fn environment_manager_reports_remote_url() {
        let manager = EnvironmentManager::create_for_tests(
            Some("ws://127.0.0.1:8765".to_string()),
            Some(test_runtime_paths()),
        )
        .await;

        let environment = manager.default_environment().expect("default environment");
        assert_eq!(
            manager.default_environment_id(),
            Some(REMOTE_ENVIRONMENT_ID)
        );
        assert!(environment.is_remote());
        assert_eq!(environment.exec_server_url(), Some("ws://127.0.0.1:8765"));
        assert!(Arc::ptr_eq(
            &environment,
            &manager
                .get_environment(REMOTE_ENVIRONMENT_ID)
                .expect("remote environment")
        ));
        assert!(manager.get_environment(LOCAL_ENVIRONMENT_ID).is_none());
        assert_local_environment_unavailable(&manager);
    }

    #[tokio::test]
    async fn environment_manager_default_environment_caches_environment() {
        let manager = EnvironmentManager::default_for_tests();

        let first = manager.default_environment().expect("default environment");
        let second = manager.default_environment().expect("default environment");

        assert!(Arc::ptr_eq(&first, &second));
        assert!(Arc::ptr_eq(
            &first.get_filesystem(),
            &second.get_filesystem()
        ));
    }

    #[tokio::test]
    async fn environment_manager_builds_from_snapshot() {
        let snapshot = EnvironmentProviderSnapshot {
            environments: vec![(
                REMOTE_ENVIRONMENT_ID.to_string(),
                Environment::create_for_tests(Some("ws://127.0.0.1:8765".to_string()))
                    .expect("remote environment"),
            )],
            default: EnvironmentDefault::EnvironmentId(REMOTE_ENVIRONMENT_ID.to_string()),
            include_local: false,
        };
        let manager = EnvironmentManager::from_snapshot(snapshot, Some(test_runtime_paths()))
            .expect("environment manager");

        assert_eq!(
            manager.default_environment_id(),
            Some(REMOTE_ENVIRONMENT_ID)
        );
        assert!(
            manager
                .get_environment(REMOTE_ENVIRONMENT_ID)
                .expect("remote environment")
                .is_remote()
        );
        assert!(manager.get_environment(LOCAL_ENVIRONMENT_ID).is_none());
        assert_local_environment_unavailable(&manager);
    }

    #[tokio::test]
    async fn environment_manager_rejects_empty_environment_id() {
        let snapshot = EnvironmentProviderSnapshot {
            environments: vec![("".to_string(), Environment::default_for_tests())],
            default: EnvironmentDefault::Disabled,
            include_local: false,
        };
        let err = EnvironmentManager::from_snapshot(snapshot, Some(test_runtime_paths()))
            .expect_err("empty id should fail");

        assert_eq!(
            err.to_string(),
            "exec-server protocol error: environment id cannot be empty"
        );
    }

    #[tokio::test]
    async fn environment_manager_rejects_provider_supplied_local_environment() {
        let snapshot = EnvironmentProviderSnapshot {
            environments: vec![(
                LOCAL_ENVIRONMENT_ID.to_string(),
                Environment::default_for_tests(),
            )],
            default: EnvironmentDefault::Disabled,
            include_local: false,
        };
        let err = EnvironmentManager::from_snapshot(snapshot, Some(test_runtime_paths()))
            .expect_err("local id should fail");

        assert_eq!(
            err.to_string(),
            "exec-server protocol error: environment id `local` is reserved for EnvironmentManager"
        );
    }

    #[tokio::test]
    async fn environment_manager_uses_explicit_provider_default() {
        let snapshot = EnvironmentProviderSnapshot {
            environments: vec![(
                "devbox".to_string(),
                Environment::create_for_tests(Some("ws://127.0.0.1:8765".to_string()))
                    .expect("remote environment"),
            )],
            default: EnvironmentDefault::EnvironmentId("devbox".to_string()),
            include_local: true,
        };
        let manager = EnvironmentManager::from_snapshot(snapshot, Some(test_runtime_paths()))
            .expect("manager");

        assert_eq!(manager.default_environment_id(), Some("devbox"));
        assert_eq!(
            manager.default_environment_ids(),
            vec!["devbox".to_string(), LOCAL_ENVIRONMENT_ID.to_string()]
        );
        assert!(manager.default_environment().expect("default").is_remote());
    }

    #[tokio::test]
    async fn environment_manager_disables_provider_default() {
        let snapshot = EnvironmentProviderSnapshot {
            environments: vec![(
                "devbox".to_string(),
                Environment::create_for_tests(Some("ws://127.0.0.1:8765".to_string()))
                    .expect("remote environment"),
            )],
            default: EnvironmentDefault::Disabled,
            include_local: true,
        };
        let manager = EnvironmentManager::from_snapshot(snapshot, Some(test_runtime_paths()))
            .expect("manager");

        assert_eq!(manager.default_environment_id(), None);
        assert!(manager.default_environment().is_none());
        assert!(Arc::ptr_eq(
            &manager
                .get_environment(LOCAL_ENVIRONMENT_ID)
                .expect("local environment"),
            &manager.try_local_environment().expect("local environment")
        ));
    }

    #[tokio::test]
    async fn environment_manager_rejects_unknown_provider_default() {
        let snapshot = EnvironmentProviderSnapshot {
            environments: vec![(
                "devbox".to_string(),
                Environment::create_for_tests(Some("ws://127.0.0.1:8765".to_string()))
                    .expect("remote environment"),
            )],
            default: EnvironmentDefault::EnvironmentId("missing".to_string()),
            include_local: true,
        };
        let err = EnvironmentManager::from_snapshot(snapshot, Some(test_runtime_paths()))
            .expect_err("unknown default should fail");

        assert_eq!(
            err.to_string(),
            "exec-server protocol error: default environment `missing` is not configured"
        );
    }

    #[tokio::test]
    async fn environment_manager_includes_local_for_default_provider_without_url() {
        let manager = EnvironmentManager::create_for_tests(
            /*exec_server_url*/ None,
            Some(test_runtime_paths()),
        )
        .await;

        let environment = manager.default_environment().expect("default environment");
        assert_eq!(manager.default_environment_id(), Some(LOCAL_ENVIRONMENT_ID));
        assert!(Arc::ptr_eq(
            &environment,
            &manager
                .get_environment(LOCAL_ENVIRONMENT_ID)
                .expect("local environment")
        ));
        assert!(Arc::ptr_eq(
            &environment,
            &manager.try_local_environment().expect("local environment")
        ));
        assert!(!environment.is_remote());
    }

    #[tokio::test]
    async fn environment_manager_carries_local_runtime_paths() {
        let runtime_paths = test_runtime_paths();
        let manager = EnvironmentManager::create_for_tests(
            /*exec_server_url*/ None,
            Some(runtime_paths.clone()),
        )
        .await;

        let environment = manager.try_local_environment().expect("local environment");

        assert_eq!(environment.local_runtime_paths(), Some(&runtime_paths));
        let manager = EnvironmentManager::create_for_tests(
            environment.exec_server_url().map(str::to_owned),
            Some(
                environment
                    .local_runtime_paths()
                    .expect("local runtime paths")
                    .clone(),
            ),
        )
        .await;
        let environment = manager.try_local_environment().expect("local environment");
        assert_eq!(environment.local_runtime_paths(), Some(&runtime_paths));
    }

    #[tokio::test]
    async fn environment_manager_omits_default_provider_local_lookup_when_default_disabled() {
        let manager = EnvironmentManager::create_for_tests(
            Some("none".to_string()),
            Some(test_runtime_paths()),
        )
        .await;

        assert!(manager.default_environment().is_none());
        assert_eq!(manager.default_environment_id(), None);
        assert!(manager.get_environment(LOCAL_ENVIRONMENT_ID).is_none());
        assert!(manager.get_environment(REMOTE_ENVIRONMENT_ID).is_none());
        assert_local_environment_unavailable(&manager);
    }

    #[tokio::test]
    async fn environment_manager_snapshot_without_local_environment_disables_local_default() {
        let mut snapshot = EnvironmentProviderSnapshot {
            environments: Vec::new(),
            default: EnvironmentDefault::EnvironmentId(LOCAL_ENVIRONMENT_ID.to_string()),
            include_local: true,
        };
        snapshot.include_local = false;
        snapshot.default = EnvironmentDefault::Disabled;
        let manager =
            EnvironmentManager::from_snapshot(snapshot, /*local_runtime_paths*/ None)
                .expect("environment manager");

        assert!(manager.default_environment().is_none());
        assert_eq!(manager.default_environment_id(), None);
        assert!(manager.get_environment(LOCAL_ENVIRONMENT_ID).is_none());
        assert_local_environment_unavailable(&manager);
    }

    #[tokio::test]
    async fn get_environment_returns_none_for_unknown_id() {
        let manager = EnvironmentManager::default_for_tests();

        assert!(manager.get_environment("does-not-exist").is_none());
    }

    #[tokio::test]
    async fn environment_manager_snapshots_default_first_and_include_metadata() {
        let manager = EnvironmentManager::default_for_tests();
        manager
            .upsert_environment(
                "remote-b".to_string(),
                "ws://127.0.0.1:8765".to_string(),
                None,
            )
            .expect("remote-b environment");
        manager
            .upsert_environment(
                "remote-a".to_string(),
                "ws://127.0.0.1:9876".to_string(),
                None,
            )
            .expect("remote-a environment");
        manager.set_environment_metadata(
            "remote-a".to_string(),
            EnvironmentMetadata {
                cwd: "/workspace".to_string(),
                shell: Some("/bin/bash".to_string()),
            },
        );

        let snapshots = manager.environment_snapshots();

        assert_eq!(
            snapshots
                .iter()
                .map(|snapshot| snapshot.environment_id.as_str())
                .collect::<Vec<_>>(),
            vec![LOCAL_ENVIRONMENT_ID, "remote-a", "remote-b"]
        );
        assert!(snapshots[0].is_default);
        assert!(!snapshots[0].is_remote);
        assert!(!snapshots[1].is_default);
        assert!(snapshots[1].is_remote);
        assert_eq!(
            snapshots[1].metadata,
            Some(EnvironmentMetadata {
                cwd: "/workspace".to_string(),
                shell: Some("/bin/bash".to_string()),
            })
        );
        assert_eq!(snapshots[2].metadata, None);
    }

    #[tokio::test]
    async fn environment_manager_last_environment_id_roundtrip() {
        let manager = EnvironmentManager::without_environments();

        assert!(manager.get_last_environment_id("thread-123").is_none());
        manager.set_last_environment_id("thread-123".to_string(), LOCAL_ENVIRONMENT_ID.to_string());
        assert_eq!(
            manager.get_last_environment_id("thread-123").as_deref(),
            Some(LOCAL_ENVIRONMENT_ID)
        );
        manager.set_last_environment_id("thread-123".to_string(), "remote-a".to_string());
        assert_eq!(
            manager.get_last_environment_id("thread-123").as_deref(),
            Some("remote-a")
        );
        assert!(manager.get_last_environment_id("thread-456").is_none());
    }

    #[tokio::test]
    async fn environment_manager_thread_environment_ids_roundtrip() {
        let manager = EnvironmentManager::without_environments();

        assert!(manager.get_thread_environment_ids("thread-123").is_empty());
        manager.record_thread_environment_id("thread-123".to_string(), "remote-a".to_string());
        manager.record_thread_environment_id("thread-123".to_string(), "remote-b".to_string());
        manager.record_thread_environment_id("thread-123".to_string(), "remote-a".to_string());

        assert_eq!(
            manager.get_thread_environment_ids("thread-123"),
            vec!["remote-a".to_string(), "remote-b".to_string()]
        );
        assert!(manager.get_thread_environment_ids("thread-456").is_empty());
    }

    #[tokio::test]
    async fn environment_manager_clear_last_launcher() {
        let manager = EnvironmentManager::without_environments();
        manager.set_last_launcher(
            "thread-123".to_string(),
            crate::provision::RemoteLauncher::ssh("hostname"),
        );

        assert!(manager.get_last_launcher("thread-123").is_some());
        manager.clear_last_launcher("thread-123");
        assert!(manager.get_last_launcher("thread-123").is_none());
    }

    #[tokio::test]
    async fn environment_manager_upserts_named_remote_environment() {
        let manager = EnvironmentManager::without_environments();

        manager
            .upsert_environment(
                "executor-a".to_string(),
                "ws://127.0.0.1:8765".to_string(),
                /*connect_timeout*/ None,
            )
            .expect("remote environment");
        let first = manager
            .get_environment("executor-a")
            .expect("first remote environment");
        assert!(first.is_remote());
        assert_eq!(first.exec_server_url(), Some("ws://127.0.0.1:8765"));
        assert_eq!(manager.default_environment_id(), None);

        manager
            .upsert_environment(
                "executor-a".to_string(),
                "ws://127.0.0.1:9876".to_string(),
                /*connect_timeout*/ None,
            )
            .expect("updated remote environment");
        let second = manager
            .get_environment("executor-a")
            .expect("second remote environment");
        assert!(second.is_remote());
        assert_eq!(second.exec_server_url(), Some("ws://127.0.0.1:9876"));
        assert!(!Arc::ptr_eq(&first, &second));
    }

    #[tokio::test]
    async fn environment_manager_starts_remote_environment_when_upserted() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind websocket listener");
        let manager = EnvironmentManager::without_environments();

        manager
            .upsert_environment(
                "executor-a".to_string(),
                format!("ws://{}", listener.local_addr().expect("listener address")),
                /*connect_timeout*/ None,
            )
            .expect("remote environment");

        timeout(Duration::from_secs(5), listener.accept())
            .await
            .expect("environment should start connecting when registered")
            .expect("accept connection");
    }

    #[tokio::test]
    async fn environment_manager_leaves_stdio_environment_lazy() {
        let environment = Environment::remote_with_transport(
            ExecServerTransportParams::StdioCommand {
                command: StdioExecServerCommand {
                    program: "codex-missing-exec-server-for-test".to_string(),
                    args: Vec::new(),
                    env: HashMap::new(),
                    cwd: None,
                },
                initialize_timeout: Duration::from_secs(1),
            },
            /*local_runtime_paths*/ None,
        );
        let manager = EnvironmentManager::from_snapshot(
            EnvironmentProviderSnapshot {
                environments: vec![("stdio".to_string(), environment)],
                default: EnvironmentDefault::Disabled,
                include_local: false,
            },
            /*local_runtime_paths*/ None,
        )
        .expect("environment manager");
        let environment = manager.get_environment("stdio").expect("stdio environment");

        assert!(!environment.startup_finished());
        assert!(environment.wait_until_ready().await.is_err());
        assert!(environment.startup_finished());
    }

    #[tokio::test]
    async fn replacing_environment_stops_its_startup_task() {
        let first_listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind first websocket listener");
        let second_listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind second websocket listener");
        let manager = EnvironmentManager::without_environments();
        manager
            .upsert_environment(
                "executor-a".to_string(),
                format!(
                    "ws://{}",
                    first_listener.local_addr().expect("first listener address")
                ),
                /*connect_timeout*/ None,
            )
            .expect("first remote environment");
        let environment = manager
            .get_environment("executor-a")
            .expect("first remote environment");
        let startup_abort = environment
            .startup_task
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .expect("startup task")
            .abort_handle();
        assert!(!startup_abort.is_finished());
        drop(environment);

        manager
            .upsert_environment(
                "executor-a".to_string(),
                format!(
                    "ws://{}",
                    second_listener
                        .local_addr()
                        .expect("second listener address")
                ),
                /*connect_timeout*/ None,
            )
            .expect("replacement remote environment");

        timeout(Duration::from_secs(1), async {
            while !startup_abort.is_finished() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("replacing the environment should cancel its startup task");
    }

    #[tokio::test]
    async fn environment_manager_rejects_empty_remote_environment_url() {
        let manager = EnvironmentManager::without_environments();

        let err = manager
            .upsert_environment(
                "executor-a".to_string(),
                String::new(),
                /*connect_timeout*/ None,
            )
            .expect_err("empty URL should fail");

        assert_eq!(
            err.to_string(),
            "exec-server protocol error: remote environment requires an exec-server url"
        );
    }

    #[tokio::test]
    async fn default_environment_has_ready_local_executor() {
        let environment = Environment::default_for_tests();

        let response = environment
            .get_exec_backend()
            .start(crate::ExecParams {
                process_id: ProcessId::from("default-env-proc"),
                argv: vec!["true".to_string()],
                cwd: PathUri::from_host_native_path(
                    std::env::current_dir().expect("read current dir"),
                )
                .expect("cwd URI"),
                env_policy: None,
                env: Default::default(),
                tty: false,
                pipe_stdin: false,
                arg0: None,
                sandbox: None,
                enforce_managed_network: false,
                managed_network: None,
            })
            .await
            .expect("start process");

        assert_eq!(response.process.process_id().as_str(), "default-env-proc");
    }

    #[tokio::test]
    async fn local_environment_passes_runtime_paths_to_exec_backend() {
        let environment = Environment::local(test_runtime_paths());
        #[cfg(unix)]
        let uri = "file://server/share/checkout";
        #[cfg(windows)]
        let uri = "file:///usr/local/checkout";
        let sandbox_cwd = PathUri::parse(uri).expect("non-native sandbox cwd URI");
        let source = sandbox_cwd
            .to_abs_path()
            .expect_err("sandbox cwd should not be native to this host");
        let sandbox = crate::FileSystemSandboxContext::from_permission_profile_with_cwd(
            codex_protocol::models::PermissionProfile::workspace_write(),
            sandbox_cwd.clone(),
        );

        let result = environment
            .get_exec_backend()
            .start(crate::ExecParams {
                process_id: ProcessId::from("local-sandbox-proc"),
                argv: vec!["true".to_string()],
                cwd: PathUri::from_host_native_path(
                    std::env::current_dir().expect("read current dir"),
                )
                .expect("cwd URI"),
                env_policy: None,
                env: Default::default(),
                tty: false,
                pipe_stdin: false,
                arg0: None,
                sandbox: Some(sandbox),
                enforce_managed_network: false,
                managed_network: None,
            })
            .await;
        let Err(err) = result else {
            panic!("sandbox cwd should be rejected after resolving runtime paths");
        };

        assert_eq!(
            err.to_string(),
            format!(
                "exec-server rejected request (-32602): sandbox cwd URI `{sandbox_cwd}` is not valid on this exec-server host: {source}"
            )
        );
    }

    #[tokio::test]
    async fn test_environment_rejects_sandboxed_filesystem_without_runtime_paths() {
        let environment = Environment::default_for_tests();
        let path = codex_utils_absolute_path::AbsolutePathBuf::from_absolute_path(
            std::env::current_exe().expect("current exe").as_path(),
        )
        .expect("absolute current exe");
        let path = codex_utils_path_uri::PathUri::from_abs_path(&path);
        let sandbox = crate::FileSystemSandboxContext::from_permission_profile(
            codex_protocol::models::PermissionProfile::from_runtime_permissions(
                &codex_protocol::permissions::FileSystemSandboxPolicy::restricted(Vec::new()),
                codex_protocol::permissions::NetworkSandboxPolicy::Restricted,
            ),
        );

        let err = environment
            .get_filesystem()
            .read_file(&path, Some(&sandbox))
            .await
            .expect_err("sandboxed read should require runtime paths");

        assert_eq!(
            err.to_string(),
            "sandboxed filesystem operations require configured runtime paths"
        );
    }
}
