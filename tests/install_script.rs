use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const ASSET: &str = "moraebot-aarch64-apple-darwin.tar.gz";

fn fake_moraebot(version_line: &str, version_exit: i32) -> Vec<u8> {
    format!(
        "#!/bin/sh\nif [ \"$1\" = --version ]; then\n  echo '{version_line}'\n  exit {version_exit}\nfi\nexit 0\n"
    )
    .into_bytes()
}

fn run_ok(cmd: &mut Command) -> Output {
    let output = cmd.output().expect("spawn helper command");
    assert!(output.status.success(), "helper command failed: {cmd:?}");
    output
}

fn publish_release(dir: &Path, binary: &[u8]) {
    fs::create_dir_all(dir).expect("create release dir");
    let stage = dir.join("stage");
    fs::create_dir_all(&stage).expect("create stage dir");
    let exe = stage.join("moraebot");
    fs::write(&exe, binary).expect("write fake moraebot");
    fs::set_permissions(&exe, fs::Permissions::from_mode(0o755)).expect("chmod fake");
    run_ok(
        Command::new("tar")
            .env("COPYFILE_DISABLE", "1")
            .arg("-czf")
            .arg(dir.join(ASSET))
            .arg("-C")
            .arg(&stage)
            .arg("moraebot"),
    );
    fs::remove_dir_all(&stage).expect("remove stage dir");
    let sums = run_ok(
        Command::new("shasum")
            .args(["-a", "256", ASSET])
            .current_dir(dir),
    );
    fs::write(dir.join("SHA256SUMS.txt"), sums.stdout).expect("write sums");
}

struct Sandbox {
    tmp: tempfile::TempDir,
}

impl Sandbox {
    fn new() -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        fs::create_dir_all(tmp.path().join("user")).expect("create user dir");
        Self { tmp }
    }

    fn root(&self) -> &Path {
        self.tmp.path()
    }

    fn latest(&self) -> PathBuf {
        self.root().join("release/latest/download")
    }

    fn pinned(&self, version: &str) -> PathBuf {
        self.root().join("release/download").join(version)
    }

    fn bin_dir(&self) -> PathBuf {
        self.root().join("bin")
    }

    fn installed(&self) -> PathBuf {
        self.bin_dir().join("moraebot")
    }

    fn put_existing(&self, bytes: &[u8]) {
        fs::create_dir_all(self.bin_dir()).expect("create bin dir");
        fs::write(self.installed(), bytes).expect("write existing binary");
        fs::set_permissions(self.installed(), fs::Permissions::from_mode(0o755))
            .expect("chmod existing");
    }

    fn command(&self, version: Option<&str>) -> Command {
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("install.sh");
        let mut cmd = Command::new("/bin/sh");
        cmd.arg(script)
            .env_clear()
            .env("HOME", self.root().join("user"))
            .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
            .env(
                "MORAEBOT_DOWNLOAD_BASE",
                format!("file://{}", self.root().join("release").display()),
            )
            .stdin(Stdio::null());
        if let Some(version) = version {
            cmd.env("MORAEBOT_VERSION", version);
        }
        cmd
    }

    fn install(&self, version: Option<&str>) -> Output {
        self.command(version)
            .env("MORAEBOT_INSTALL_DIR", self.bin_dir())
            .output()
            .expect("run install.sh")
    }

    fn leftover_temp_files(&self) -> Vec<String> {
        let Ok(entries) = fs::read_dir(self.bin_dir()) else {
            return Vec::new();
        };
        entries
            .map(|entry| {
                entry
                    .expect("dir entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .filter(|name| name.starts_with(".moraebot.new."))
            .collect()
    }
}

fn exit_code(output: &Output) -> Option<i32> {
    output.status.code()
}

#[test]
fn fresh_install_from_latest() {
    let sb = Sandbox::new();
    let fake = fake_moraebot("moraebot 1.0.0", 0);
    publish_release(&sb.latest(), &fake);

    let output = sb.install(None);

    assert_eq!(exit_code(&output), Some(0), "{output:?}");
    assert_eq!(fs::read(sb.installed()).expect("read installed"), fake);
    let mode = fs::metadata(sb.installed())
        .expect("stat")
        .permissions()
        .mode();
    assert_ne!(mode & 0o100, 0, "user execute bit missing: {mode:o}");
    assert!(sb.leftover_temp_files().is_empty());
}

#[test]
fn upgrade_replaces_existing_binary() {
    let sb = Sandbox::new();
    sb.put_existing(&fake_moraebot("moraebot 0.9.0", 0));
    let new = fake_moraebot("moraebot 1.0.0", 0);
    publish_release(&sb.latest(), &new);

    let output = sb.install(None);

    assert_eq!(exit_code(&output), Some(0), "{output:?}");
    assert_eq!(fs::read(sb.installed()).expect("read installed"), new);
    assert!(sb.leftover_temp_files().is_empty());
}

#[test]
fn bad_checksum_keeps_old_binary() {
    let sb = Sandbox::new();
    let old = fake_moraebot("moraebot 0.9.0", 0);
    sb.put_existing(&old);
    publish_release(&sb.latest(), &fake_moraebot("moraebot 1.0.0", 0));
    fs::write(
        sb.latest().join("SHA256SUMS.txt"),
        format!("{}  {ASSET}\n", "0".repeat(64)),
    )
    .expect("corrupt sums");

    let output = sb.install(None);

    assert_eq!(exit_code(&output), Some(1), "{output:?}");
    assert_eq!(fs::read(sb.installed()).expect("read installed"), old);
    assert!(sb.leftover_temp_files().is_empty());
}

#[test]
fn pinned_version_installs_that_release() {
    let sb = Sandbox::new();
    let pinned = fake_moraebot("moraebot 9.9.9", 0);
    publish_release(&sb.pinned("v9.9.9"), &pinned);
    publish_release(&sb.latest(), &fake_moraebot("moraebot 10.0.0", 0));

    let output = sb.install(Some("v9.9.9"));

    assert_eq!(exit_code(&output), Some(0), "{output:?}");
    assert_eq!(fs::read(sb.installed()).expect("read installed"), pinned);
}

#[test]
fn version_without_v_prefix_is_refused() {
    let sb = Sandbox::new();
    publish_release(&sb.pinned("9.9.9"), &fake_moraebot("moraebot 9.9.9", 0));

    let output = sb.install(Some("9.9.9"));

    assert_eq!(exit_code(&output), Some(1), "{output:?}");
    assert!(!sb.installed().exists());
}

#[test]
fn failing_smoke_run_keeps_old_binary() {
    let sb = Sandbox::new();
    let old = fake_moraebot("moraebot 0.9.0", 0);
    sb.put_existing(&old);
    publish_release(&sb.latest(), &fake_moraebot("broken", 1));

    let output = sb.install(None);

    assert_eq!(exit_code(&output), Some(1), "{output:?}");
    assert_eq!(fs::read(sb.installed()).expect("read installed"), old);
    assert!(sb.leftover_temp_files().is_empty());
}

#[test]
fn default_install_dir_is_user_local_bin() {
    let sb = Sandbox::new();
    let fake = fake_moraebot("moraebot 1.0.0", 0);
    publish_release(&sb.latest(), &fake);

    let output = sb.command(None).output().expect("run install.sh");

    assert_eq!(exit_code(&output), Some(0), "{output:?}");
    let installed = sb.root().join("user/.local/bin/moraebot");
    assert_eq!(fs::read(installed).expect("read installed"), fake);
    assert!(!sb.installed().exists());
}
