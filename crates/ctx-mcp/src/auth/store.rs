use super::*;

pub(super) struct AuthLock {
    path: PathBuf,
}

impl Drop for AuthLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

pub(super) fn xai_state_and_tokens(store: &AuthStore) -> Result<(XaiProviderState, XaiTokens)> {
    let state =
        store.providers.get(PROVIDER_ID).cloned().ok_or_else(|| {
            anyhow!("no xAI OAuth credentials stored; run `ctx-mcp auth login xai`")
        })?;
    let tokens = state.tokens.clone().ok_or_else(|| {
        anyhow!("xAI OAuth state is missing tokens; run `ctx-mcp auth login xai --force`")
    })?;
    Ok((state, tokens))
}

pub(super) fn save_xai_state(state: XaiProviderState) -> Result<()> {
    let path = auth_file_path()?;
    let _lock = acquire_auth_lock(&path)?;
    let mut store = load_store(&path)?;
    store.providers.insert(PROVIDER_ID.to_string(), state);
    save_store(&path, &store)
}

pub(super) fn load_xai_state() -> Result<Option<XaiProviderState>> {
    let path = auth_file_path()?;
    let store = load_store(&path)?;
    Ok(store.providers.get(PROVIDER_ID).cloned())
}

pub(super) fn load_store(path: &Path) -> Result<AuthStore> {
    if !path.exists() {
        return Ok(AuthStore::default());
    }
    let text =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    if text.trim().is_empty() {
        return Ok(AuthStore::default());
    }
    serde_json::from_str(&text).with_context(|| format!("failed to parse {}", path.display()))
}

pub(super) fn save_store(path: &Path, store: &AuthStore) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let tmp = path.with_extension(format!("json.tmp.{}", std::process::id()));
    let bytes = serde_json::to_vec_pretty(store).context("failed to encode auth store")?;
    write_private_file(&tmp, &bytes)?;
    replace_file(&tmp, path).with_context(|| format!("failed to save {}", path.display()))
}

pub(super) fn auth_file_path() -> Result<PathBuf> {
    if let Ok(path) = std::env::var("CTX_MCP_AUTH_FILE") {
        return Ok(PathBuf::from(path));
    }
    Ok(auth_home()?.join("auth.json"))
}

pub(super) fn auth_home() -> Result<PathBuf> {
    if let Ok(path) = std::env::var("CTX_MCP_HOME") {
        return Ok(PathBuf::from(path));
    }
    if let Ok(path) = std::env::var("XDG_CONFIG_HOME") {
        return Ok(PathBuf::from(path).join("ctx-mcp"));
    }
    #[cfg(target_os = "windows")]
    if let Ok(path) = std::env::var("APPDATA") {
        return Ok(PathBuf::from(path).join("ctx-mcp"));
    }
    if let Ok(home) = std::env::var("HOME") {
        return Ok(PathBuf::from(home).join(".ctx-mcp"));
    }
    bail!("could not determine auth directory; set CTX_MCP_HOME or CTX_MCP_AUTH_FILE")
}

pub(super) fn acquire_auth_lock(auth_path: &Path) -> Result<AuthLock> {
    if let Some(parent) = auth_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let lock_path = auth_path.with_file_name("auth.json.lock");
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock_path)
        {
            Ok(mut file) => {
                writeln!(file, "pid={}", std::process::id()).ok();
                return Ok(AuthLock { path: lock_path });
            }
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                if remove_stale_lock(&lock_path)? {
                    continue;
                }
                if Instant::now() >= deadline {
                    bail!("timed out waiting for auth lock: {}", lock_path.display());
                }
                sleep(Duration::from_millis(100));
            }
            Err(err) => {
                return Err(err)
                    .with_context(|| format!("failed to create {}", lock_path.display()));
            }
        }
    }
}

pub(super) fn remove_stale_lock(lock_path: &Path) -> Result<bool> {
    let stale_after = Duration::from_secs(300);
    let stale = fs::metadata(lock_path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| modified.elapsed().ok())
        .is_some_and(|elapsed| elapsed > stale_after);
    if stale {
        fs::remove_file(lock_path).with_context(|| {
            format!("failed to remove stale auth lock: {}", lock_path.display())
        })?;
    }
    Ok(stale)
}

#[cfg(unix)]
pub(super) fn write_private_file(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("failed to write {}", path.display()))?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
pub(super) fn write_private_file(path: &Path, bytes: &[u8]) -> Result<()> {
    fs::write(path, bytes).with_context(|| format!("failed to write {}", path.display()))
}

#[cfg(not(windows))]
pub(super) fn replace_file(tmp: &Path, target: &Path) -> Result<()> {
    fs::rename(tmp, target).map_err(Into::into)
}

#[cfg(windows)]
pub(super) fn replace_file(tmp: &Path, target: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;

    const MOVEFILE_REPLACE_EXISTING: u32 = 0x0000_0001;
    const MOVEFILE_WRITE_THROUGH: u32 = 0x0000_0008;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn MoveFileExW(existing: *const u16, new_name: *const u16, flags: u32) -> i32;
    }

    fn wide(path: &Path) -> Vec<u16> {
        path.as_os_str().encode_wide().chain([0]).collect()
    }

    let tmp_wide = wide(tmp);
    let target_wide = wide(target);
    let ok = unsafe {
        MoveFileExW(
            tmp_wide.as_ptr(),
            target_wide.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if ok == 0 {
        Err(io::Error::last_os_error()).with_context(|| {
            format!(
                "failed to atomically replace {} with {}",
                target.display(),
                tmp.display()
            )
        })
    } else {
        Ok(())
    }
}
