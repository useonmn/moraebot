//! Where the bridge socket lives, and its private folder.
//!
//! `setup` writes the path explicitly into both the core and the bridge config, so the
//! two sides never derive it on their own (macOS has no `$XDG_RUNTIME_DIR`, and a
//! launchd agent would not get one).

use std::fs;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use crate::config::{ConfigError, E_HOME_UNKNOWN, E_IO, Env};

use super::E_BRIDGE_SOCKET;
use super::peer::own_uid;

/// File name of the Slack bridge socket inside its folder.
pub const SOCKET_NAME: &str = "slack-bridge.sock";

/// Bytes `sockaddr_un.sun_path` holds on this OS (108 on Linux, 104 on macOS), the
/// terminating NUL included.
pub const SUN_PATH_LEN: usize =
    std::mem::size_of::<libc::sockaddr_un>() - std::mem::offset_of!(libc::sockaddr_un, sun_path);

/// The default socket path for `os` (a `std::env::consts::OS` value).
///
/// - macOS: `~/Library/Application Support/moraebot/slack-bridge.sock`.
/// - Elsewhere: `$XDG_RUNTIME_DIR/moraebot/slack-bridge.sock` when `xdg_runtime_dir` is
///   absolute, else `<moraebot home>/run/slack-bridge.sock`.
///
/// The result passes [`check_path`].
pub fn default_path(
    env: &Env,
    os: &str,
    xdg_runtime_dir: Option<&Path>,
) -> Result<PathBuf, ConfigError> {
    let path = if os == "macos" {
        let home = env.home_dir.as_ref().ok_or_else(|| {
            ConfigError::new(
                E_HOME_UNKNOWN,
                "HOME is not set, cannot find ~/Library/Application Support",
            )
        })?;
        home.join("Library")
            .join("Application Support")
            .join("moraebot")
            .join(SOCKET_NAME)
    } else if let Some(runtime) = xdg_runtime_dir.filter(|dir| dir.is_absolute()) {
        runtime.join("moraebot").join(SOCKET_NAME)
    } else {
        env.default_home()?.join("run").join(SOCKET_NAME)
    };
    check_path(&path)?;
    Ok(path)
}

/// `Ok` when `path` is absolute and fits `sockaddr_un` with its NUL.
pub fn check_path(path: &Path) -> Result<(), ConfigError> {
    if !path.is_absolute() {
        return Err(ConfigError::new(
            E_BRIDGE_SOCKET,
            format!(
                "the bridge socket must be an absolute path: {}",
                path.display()
            ),
        ));
    }
    let len = path.as_os_str().len();
    if len >= SUN_PATH_LEN {
        return Err(ConfigError::new(
            E_BRIDGE_SOCKET,
            format!(
                "the bridge socket path is {len} bytes, this OS allows at most {}: {}",
                SUN_PATH_LEN - 1,
                path.display()
            ),
        ));
    }
    Ok(())
}

/// Checks `socket` with [`check_path`] and makes its folder private: created with mode
/// 0700 when missing, tightened to 0700 when it is ours but open to others, refused when
/// it is a symlink, not a folder, or owned by another uid.
pub fn prepare_dir(socket: &Path) -> Result<(), ConfigError> {
    check_path(socket)?;
    let dir = socket.parent().ok_or_else(|| {
        ConfigError::new(
            E_BRIDGE_SOCKET,
            format!("the bridge socket has no folder: {}", socket.display()),
        )
    })?;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(|err| ConfigError::io(E_IO, &format!("create {}", dir.display()), &err))?;
    let meta = fs::symlink_metadata(dir)
        .map_err(|err| ConfigError::io(E_IO, &format!("inspect {}", dir.display()), &err))?;
    if !meta.file_type().is_dir() {
        return Err(ConfigError::new(
            E_BRIDGE_SOCKET,
            format!(
                "the bridge socket folder is not a folder: {}",
                dir.display()
            ),
        ));
    }
    if meta.uid() != own_uid() {
        return Err(ConfigError::new(
            E_BRIDGE_SOCKET,
            format!(
                "the bridge socket folder belongs to uid {}, not to this user: {}",
                meta.uid(),
                dir.display()
            ),
        ));
    }
    if meta.mode() & 0o777 != 0o700 {
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
            .map_err(|err| ConfigError::io(E_IO, &format!("chmod 0700 {}", dir.display()), &err))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(home: &str) -> Env {
        Env {
            home_dir: Some(PathBuf::from(home)),
            moraebot_home: None,
            path: None,
        }
    }

    #[test]
    fn linux_uses_the_runtime_dir_when_absolute() {
        assert_eq!(
            default_path(&env("/h/u"), "linux", Some(Path::new("/run/user/1000"))).unwrap(),
            PathBuf::from("/run/user/1000/moraebot/slack-bridge.sock")
        );
    }

    #[test]
    fn linux_without_a_runtime_dir_uses_the_moraebot_home() {
        let fallback = PathBuf::from("/h/u/.moraebot/run/slack-bridge.sock");
        assert_eq!(default_path(&env("/h/u"), "linux", None).unwrap(), fallback);
        assert_eq!(
            default_path(&env("/h/u"), "linux", Some(Path::new("run/user"))).unwrap(),
            fallback
        );
        let custom = Env {
            moraebot_home: Some(PathBuf::from("/srv/mb")),
            ..env("/h/u")
        };
        assert_eq!(
            default_path(&custom, "linux", None).unwrap(),
            PathBuf::from("/srv/mb/run/slack-bridge.sock")
        );
        let err = default_path(&Env::default(), "linux", None).unwrap_err();
        assert_eq!(err.code, E_HOME_UNKNOWN);
    }

    #[test]
    fn macos_uses_application_support_and_ignores_the_runtime_dir() {
        assert_eq!(
            default_path(&env("/m/u"), "macos", Some(Path::new("/tmp/xdg"))).unwrap(),
            PathBuf::from("/m/u/Library/Application Support/moraebot/slack-bridge.sock")
        );
        let err = default_path(&Env::default(), "macos", None).unwrap_err();
        assert_eq!(err.code, E_HOME_UNKNOWN);
    }

    #[test]
    fn a_path_too_long_for_sockaddr_un_is_refused() {
        let longest = format!("/{}", "a".repeat(SUN_PATH_LEN - 2));
        check_path(Path::new(&longest)).expect("one byte left for the NUL");
        let too_long = format!("/{}", "a".repeat(SUN_PATH_LEN - 1));
        assert_eq!(
            check_path(Path::new(&too_long)).unwrap_err().code,
            E_BRIDGE_SOCKET
        );
        let deep_home = format!("/home/{}", "u".repeat(120));
        assert_eq!(
            default_path(&env(&deep_home), "linux", None)
                .unwrap_err()
                .code,
            E_BRIDGE_SOCKET
        );
        assert_eq!(
            check_path(Path::new("run/slack-bridge.sock"))
                .unwrap_err()
                .code,
            E_BRIDGE_SOCKET
        );
    }

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().mode() & 0o777
    }

    #[test]
    fn the_folder_is_created_private() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("a").join("moraebot");
        prepare_dir(&dir.join(SOCKET_NAME)).expect("prepared");
        assert_eq!(mode(&dir), 0o700);
    }

    #[test]
    fn an_open_folder_of_ours_is_tightened() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("moraebot");
        fs::create_dir(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        prepare_dir(&dir.join(SOCKET_NAME)).expect("prepared");
        assert_eq!(mode(&dir), 0o700);
    }

    #[test]
    fn a_symlinked_folder_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("real");
        fs::create_dir(&real).unwrap();
        fs::set_permissions(&real, fs::Permissions::from_mode(0o755)).unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let err = prepare_dir(&link.join(SOCKET_NAME)).unwrap_err();
        assert_eq!(err.code, E_BRIDGE_SOCKET);
        assert_eq!(mode(&real), 0o755, "the link target is left alone");
    }
}
