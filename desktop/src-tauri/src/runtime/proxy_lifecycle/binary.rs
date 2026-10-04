fn find_gateway_in(dir: &Path) -> Option<PathBuf> {
    let exact = dir.join(if cfg!(windows) {
        "csswitch-gateway.exe"
    } else {
        "csswitch-gateway"
    });
    if exact.is_file() {
        return Some(exact);
    }
    // Tauri's staged sidecar must match this Desktop build, even when binaries
    // from several architectures coexist in a developer's checkout.
    let suffixed = dir.join(format!(
        "csswitch-gateway-{}{}",
        env!("CSSWITCH_BUILD_TARGET"),
        if cfg!(windows) { ".exe" } else { "" }
    ));
    suffixed.is_file().then_some(suffixed)
}

pub(crate) fn gateway_bin_path<R: Runtime>(app: &tauri::AppHandle<R>) -> Option<PathBuf> {
    gateway_bin_path_from(
        std::env::var_os("CSSWITCH_GATEWAY_BIN").map(PathBuf::from),
        std::env::current_exe().ok(),
        app.path().resource_dir().ok(),
        repo_root(),
    )
}

/// Gateway lookup for read-only Doctor. Unlike the runtime launcher, this never
/// consumes parent `CSSWITCH_GATEWAY_BIN` or `CSSWITCH_REPO` overrides.
pub(crate) fn doctor_gateway_bin_path<R: Runtime>(app: &tauri::AppHandle<R>) -> Option<PathBuf> {
    gateway_bin_path_from(
        None,
        std::env::current_exe().ok(),
        app.path().resource_dir().ok(),
        canonical_repo_root(),
    )
}

pub(crate) fn gateway_bin_path_from(
    env_bin: Option<PathBuf>,
    current_exe: Option<PathBuf>,
    resource_dir: Option<PathBuf>,
    repo_root: Option<PathBuf>,
) -> Option<PathBuf> {
    if let Some(path) = env_bin {
        return explicit_gateway_bin_is_safe(&path).then_some(path);
    }
    if let Some(exe) = current_exe {
        if let Some(dir) = exe.parent().and_then(find_gateway_in) {
            return Some(dir);
        }
    }
    if let Some(res) = resource_dir {
        if let Some(path) = find_gateway_in(&res) {
            return Some(path);
        }
        if let Some(path) = find_gateway_in(&res.join("binaries")) {
            return Some(path);
        }
    }
    if let Some(root) = repo_root {
        for dir in [
            root.join("desktop/src-tauri/binaries"),
            root.join("desktop/gateway/target/release"),
            root.join("desktop/gateway/target/debug"),
        ] {
            if let Some(path) = find_gateway_in(&dir) {
                return Some(path);
            }
        }
    }
    None
}

fn explicit_gateway_bin_is_safe(path: &Path) -> bool {
    if !path.is_absolute() {
        return false;
    }
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        let Ok(metadata) = std::fs::symlink_metadata(&current) else {
            return false;
        };
        if metadata.file_type().is_symlink() {
            return false;
        }
    }
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 == 0 {
            return false;
        }
    }
    true
}
