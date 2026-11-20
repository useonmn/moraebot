use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const ASSET: &str = "moraebot-aarch64-apple-darwin.tar.gz";
const LINUX_X86: &str = "moraebot-x86_64-unknown-linux-gnu.tar.gz";
const LINUX_ARM: &str = "moraebot-aarch64-unknown-linux-gnu.tar.gz";

fn fake_moraebot(version_line: &str, version_exit: i32) -> Vec<u8> {
    format!(
        "#!/bin/sh\nif [ \"$1\" = --version ]; then\n  echo '{version_line}'\n  exit {version_exit}\nfi\nexit 0\n"
    )
    .into_bytes()
}

fn fake_bridge(version_exit: i32) -> Vec<u8> {
    format!(
        "#!/bin/sh\nif [ \"$1\" = --version ]; then\n  echo 'moraebot-slack-bridge 1.0.0'\n  exit {version_exit}\nfi\nexit 0\n"
    )
    .into_bytes()
}

fn run_ok(cmd: &mut Command) -> Output {
    let output = cmd.output().expect("spawn helper command");
    assert!(output.status.success(), "helper command failed: {cmd:?}");
    output
}

/// Writes `body` to `path` (mode 755) from a child process, so this test process never holds
/// a write handle to a file that is executed later (ETXTBSY on Linux).
fn write_script(path: &Path, body: &str) {
    let mut child = Command::new("/bin/sh")
        .args(["-c", "cat > \"$1\" && chmod 755 \"$1\"", "sh"])
        .arg(path)
        .stdin(Stdio::piped())
        .spawn()
        .expect("spawn write_script");
    child
        .stdin
        .take()
        .expect("write_script stdin")
        .write_all(body.as_bytes())
        .expect("write script body");
    let status = child.wait().expect("wait write_script");
    assert!(
        status.success(),
        "write_script failed for {}",
        path.display()
    );
}

fn publish_release(dir: &Path, binary: &[u8]) {
    publish_assets(dir, &[(ASSET, binary)]);
}

/// Writes each archive (holding `moraebot` = the bytes and a working fake bridge) and one
/// `SHA256SUMS.txt` for all of them.
fn publish_assets(dir: &Path, assets: &[(&str, &[u8])]) {
    publish_assets_with(dir, assets, Some(&fake_bridge(0)));
}

/// Like `publish_assets`, but the archives hold `bridge` as `moraebot-slack-bridge`, or no
/// bridge binary when it is `None`.
fn publish_assets_with(dir: &Path, assets: &[(&str, &[u8])], bridge: Option<&[u8]>) {
    fs::create_dir_all(dir).expect("create release dir");
    let stage = dir.join("stage");
    for (name, binary) in assets {
        fs::create_dir_all(&stage).expect("create stage dir");
        let exe = stage.join("moraebot");
        fs::write(&exe, binary).expect("write fake moraebot");
        fs::set_permissions(&exe, fs::Permissions::from_mode(0o755)).expect("chmod fake");
        let mut members = vec!["moraebot"];
        if let Some(bridge) = bridge {
            let exe = stage.join("moraebot-slack-bridge");
            fs::write(&exe, bridge).expect("write fake bridge");
            fs::set_permissions(&exe, fs::Permissions::from_mode(0o755)).expect("chmod fake");
            members.push("moraebot-slack-bridge");
        }
        run_ok(
            Command::new("tar")
                .env("COPYFILE_DISABLE", "1")
                .arg("-czf")
                .arg(dir.join(name))
                .arg("-C")
                .arg(&stage)
                .args(&members),
        );
        fs::remove_dir_all(&stage).expect("remove stage dir");
    }
    let sums = run_ok(
        Command::new("shasum")
            .args(["-a", "256"])
            .args(assets.iter().map(|(name, _)| *name))
            .current_dir(dir),
    );
    fs::write(dir.join("SHA256SUMS.txt"), sums.stdout).expect("write sums");
}

/// Publishes the darwin, Linux x86_64 and Linux aarch64 fakes to `latest`; returns them in that order.
fn publish_all(sb: &Sandbox) -> [Vec<u8>; 3] {
    let fakes = [
        fake_moraebot("moraebot 1.0.0", 0),
        fake_moraebot("moraebot 1.0.1", 0),
        fake_moraebot("moraebot 1.0.2", 0),
    ];
    publish_assets(
        &sb.latest(),
        &[
            (ASSET, &fakes[0]),
            (LINUX_X86, &fakes[1]),
            (LINUX_ARM, &fakes[2]),
        ],
    );
    fakes
}

struct Sandbox {
    tmp: tempfile::TempDir,
}

impl Sandbox {
    fn new() -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        fs::create_dir_all(tmp.path().join("user")).expect("create user dir");
        fs::create_dir_all(tmp.path().join("shims")).expect("create shims dir");
        let sb = Self { tmp };
        sb.platform("Darwin", "arm64", "1");
        sb
    }

    fn shims(&self) -> PathBuf {
        self.root().join("shims")
    }

    /// Makes install.sh see `uname -s` = `os`, `uname -m` = `machine` and
    /// `sysctl -n hw.optional.arm64` = `arm64`.
    fn platform(&self, os: &str, machine: &str, arm64: &str) {
        write_script(
            &self.shims().join("uname"),
            &format!(
                "#!/bin/sh\ncase \"$1\" in -s) echo {os} ;; -m) echo {machine} ;; *) echo {os} ;; esac\n"
            ),
        );
        write_script(
            &self.shims().join("sysctl"),
            &format!("#!/bin/sh\necho {arm64}\n"),
        );
        if os == "Linux" {
            write_script(
                &self.shims().join("sha256sum"),
                "#!/bin/sh\nexec shasum -a 256 \"$@\"\n",
            );
        }
    }

    fn path(&self) -> String {
        format!("{}:/usr/bin:/bin:/usr/sbin:/sbin", self.shims().display())
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

    fn installed_bridge(&self) -> PathBuf {
        self.bin_dir().join("moraebot-slack-bridge")
    }

    fn put_existing(&self, bytes: &[u8]) {
        fs::create_dir_all(self.bin_dir()).expect("create bin dir");
        fs::write(self.installed(), bytes).expect("write existing binary");
        fs::set_permissions(self.installed(), fs::Permissions::from_mode(0o755))
            .expect("chmod existing");
    }

    fn put_existing_bridge(&self, bytes: &[u8]) {
        fs::create_dir_all(self.bin_dir()).expect("create bin dir");
        fs::write(self.installed_bridge(), bytes).expect("write existing bridge");
        fs::set_permissions(self.installed_bridge(), fs::Permissions::from_mode(0o755))
            .expect("chmod existing bridge");
    }

    fn command(&self, version: Option<&str>) -> Command {
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("install.sh");
        let mut cmd = Command::new("/bin/sh");
        cmd.arg(script)
            .env_clear()
            .env("HOME", self.root().join("user"))
            .env("PATH", self.path())
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
            .filter(|name| {
                name.starts_with(".moraebot.new.")
                    || name.starts_with(".moraebot-slack-bridge.new.")
            })
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
fn fresh_install_puts_both_binaries_in_place() {
    let sb = Sandbox::new();
    publish_release(&sb.latest(), &fake_moraebot("moraebot 1.0.0", 0));

    let output = sb.install(None);

    assert_eq!(exit_code(&output), Some(0), "{output:?}");
    assert_eq!(
        fs::read(sb.installed_bridge()).expect("read installed bridge"),
        fake_bridge(0)
    );
    let mode = fs::metadata(sb.installed_bridge())
        .expect("stat")
        .permissions()
        .mode();
    assert_ne!(mode & 0o100, 0, "user execute bit missing: {mode:o}");
    assert!(sb.leftover_temp_files().is_empty());
}

#[test]
fn pinned_pre_bridge_release_installs_moraebot_alone_with_a_warning() {
    let sb = Sandbox::new();
    let pinned = fake_moraebot("moraebot 0.1.0", 0);
    publish_assets_with(&sb.pinned("v0.1.0"), &[(ASSET, &pinned)], None);

    let output = sb.install(Some("v0.1.0"));

    assert_eq!(exit_code(&output), Some(0), "{output:?}");
    assert_eq!(fs::read(sb.installed()).expect("read installed"), pinned);
    assert!(!sb.installed_bridge().exists());
    let warnings: Vec<String> = String::from_utf8_lossy(&output.stderr)
        .lines()
        .filter(|line| line.starts_with("moraebot install warning:"))
        .map(str::to_owned)
        .collect();
    assert_eq!(warnings.len(), 1, "{output:?}");
    assert!(warnings[0].contains("moraebot-slack-bridge"), "{output:?}");
    assert!(sb.leftover_temp_files().is_empty());
}

#[test]
fn failing_bridge_smoke_run_keeps_both_old_files() {
    let sb = Sandbox::new();
    let old = fake_moraebot("moraebot 0.9.0", 0);
    let old_bridge = fake_bridge(0);
    sb.put_existing(&old);
    sb.put_existing_bridge(&old_bridge);
    publish_assets_with(
        &sb.latest(),
        &[(ASSET, &fake_moraebot("moraebot 1.0.0", 0))],
        Some(&fake_bridge(1)),
    );

    let output = sb.install(None);

    assert_eq!(exit_code(&output), Some(1), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).starts_with("moraebot install error: E_SMOKE:"),
        "{output:?}"
    );
    assert_eq!(fs::read(sb.installed()).expect("read installed"), old);
    assert_eq!(
        fs::read(sb.installed_bridge()).expect("read installed bridge"),
        old_bridge
    );
    assert!(sb.leftover_temp_files().is_empty());
}

#[test]
fn failing_core_smoke_run_keeps_the_old_bridge() {
    let sb = Sandbox::new();
    let old_bridge = b"#!/bin/sh\necho old bridge\n".to_vec();
    sb.put_existing_bridge(&old_bridge);
    publish_release(&sb.latest(), &fake_moraebot("broken", 1));

    let output = sb.install(None);

    assert_eq!(exit_code(&output), Some(1), "{output:?}");
    assert_eq!(
        fs::read(sb.installed_bridge()).expect("read installed bridge"),
        old_bridge
    );
    assert!(!sb.installed().exists());
    assert!(sb.leftover_temp_files().is_empty());
}

fn stdout_lines(output: &Output) -> Vec<String> {
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::to_owned)
        .collect()
}

fn next_step_lines(output: &Output, cmd: &str) -> Vec<String> {
    let lines = stdout_lines(output);
    let first = lines
        .iter()
        .position(|line| *line == format!("  {cmd} init"))
        .expect("init line present");
    lines[first..].iter().take(3).cloned().collect()
}

#[test]
fn fresh_install_prints_next_steps() {
    let sb = Sandbox::new();
    publish_release(&sb.latest(), &fake_moraebot("moraebot 1.0.0", 0));

    let output = sb.install(None);

    assert_eq!(exit_code(&output), Some(0), "{output:?}");
    let cmd = sb.installed().display().to_string();
    assert_eq!(
        next_step_lines(&output, &cmd),
        [
            format!("  {cmd} init"),
            format!("  {cmd} check-config"),
            format!("  {cmd} install-launchd"),
        ]
    );
}

#[test]
fn next_steps_use_the_bare_name_on_path() {
    let sb = Sandbox::new();
    publish_release(&sb.latest(), &fake_moraebot("moraebot 1.0.0", 0));

    let output = sb
        .command(None)
        .env("MORAEBOT_INSTALL_DIR", sb.bin_dir())
        .env("PATH", format!("{}:{}", sb.bin_dir().display(), sb.path()))
        .output()
        .expect("run install.sh");

    assert_eq!(exit_code(&output), Some(0), "{output:?}");
    assert_eq!(
        next_step_lines(&output, "moraebot"),
        [
            "  moraebot init",
            "  moraebot check-config",
            "  moraebot install-launchd",
        ]
    );
}

#[test]
fn existing_config_prints_no_next_steps() {
    let sb = Sandbox::new();
    publish_release(&sb.latest(), &fake_moraebot("moraebot 1.0.0", 0));
    let home = sb.root().join("user/.moraebot");
    fs::create_dir_all(&home).expect("create moraebot home");
    fs::write(home.join("config.toml"), "").expect("write config");

    let output = sb.install(None);

    assert_eq!(exit_code(&output), Some(0), "{output:?}");
    let lines = stdout_lines(&output);
    assert!(
        !lines
            .iter()
            .any(|line| [" init", " check-config", " install-launchd"]
                .iter()
                .any(|end| line.ends_with(end))),
        "{lines:?}"
    );
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

#[test]
fn linux_picks_the_asset_for_its_arch() {
    for (machine, index) in [("x86_64", 1), ("amd64", 1), ("aarch64", 2), ("arm64", 2)] {
        let sb = Sandbox::new();
        let fakes = publish_all(&sb);
        sb.platform("Linux", machine, "0");

        let output = sb.install(None);

        assert_eq!(exit_code(&output), Some(0), "{machine}: {output:?}");
        assert_eq!(
            fs::read(sb.installed()).expect("read installed"),
            fakes[index],
            "{machine}"
        );
    }
}

#[test]
fn darwin_ignores_uname_m() {
    let sb = Sandbox::new();
    sb.platform("Darwin", "x86_64", "1");
    let fakes = publish_all(&sb);

    let output = sb.install(None);

    assert_eq!(exit_code(&output), Some(0), "{output:?}");
    assert_eq!(fs::read(sb.installed()).expect("read installed"), fakes[0]);
}

#[test]
fn unsupported_platforms_are_refused() {
    for (os, machine, arm64) in [
        ("Linux", "i686", "1"),
        ("Linux", "armv7l", "1"),
        ("FreeBSD", "amd64", "1"),
        ("Darwin", "arm64", "0"),
        ("Darwin", "x86_64", "0"),
    ] {
        let sb = Sandbox::new();
        publish_all(&sb);
        sb.platform(os, machine, arm64);

        let output = sb.install(None);

        let case = format!("{os} {machine} {arm64}");
        assert_eq!(exit_code(&output), Some(1), "{case}: {output:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .starts_with("moraebot install error: E_PLATFORM:"),
            "{case}: {output:?}"
        );
        assert!(!sb.installed().exists(), "{case}");
    }
}

fn marker_sha256sum(sb: &Sandbox) -> PathBuf {
    let marker = sb.root().join("sha256sum-used");
    write_script(
        &sb.shims().join("sha256sum"),
        &format!(
            "#!/bin/sh\ntouch '{}'\nexec shasum -a 256 \"$@\"\n",
            marker.display()
        ),
    );
    marker
}

#[test]
fn linux_checks_with_sha256sum() {
    let sb = Sandbox::new();
    sb.platform("Linux", "x86_64", "0");
    let marker = marker_sha256sum(&sb);
    publish_all(&sb);

    let output = sb.install(None);

    assert_eq!(exit_code(&output), Some(0), "{output:?}");
    assert!(marker.exists());
}

#[test]
fn darwin_checks_with_shasum() {
    let sb = Sandbox::new();
    let marker = marker_sha256sum(&sb);
    publish_all(&sb);

    let output = sb.install(None);

    assert_eq!(exit_code(&output), Some(0), "{output:?}");
    assert!(!marker.exists());
}

#[test]
fn linux_bad_checksum_keeps_old_binary() {
    let sb = Sandbox::new();
    sb.platform("Linux", "x86_64", "0");
    let old = fake_moraebot("moraebot 0.9.0", 0);
    sb.put_existing(&old);
    publish_all(&sb);
    fs::write(
        sb.latest().join("SHA256SUMS.txt"),
        format!("{}  {LINUX_X86}\n", "0".repeat(64)),
    )
    .expect("corrupt sums");

    let output = sb.install(None);

    assert_eq!(exit_code(&output), Some(1), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).starts_with("moraebot install error: E_CHECKSUM:"),
        "{output:?}"
    );
    assert_eq!(fs::read(sb.installed()).expect("read installed"), old);
    assert!(sb.leftover_temp_files().is_empty());

    // Reach: the same tool accepts the correct sums, so the refusal was a real mismatch.
    let fakes = publish_all(&sb);
    let output = sb.install(None);
    assert_eq!(exit_code(&output), Some(0), "{output:?}");
    assert_eq!(fs::read(sb.installed()).expect("read installed"), fakes[1]);
}

#[test]
fn linux_next_steps_name_install_systemd() {
    let sb = Sandbox::new();
    sb.platform("Linux", "x86_64", "0");
    publish_all(&sb);

    let output = sb.install(None);

    assert_eq!(exit_code(&output), Some(0), "{output:?}");
    let cmd = sb.installed().display().to_string();
    assert_eq!(
        next_step_lines(&output, &cmd),
        [
            format!("  {cmd} init"),
            format!("  {cmd} check-config"),
            format!("  {cmd} install-systemd"),
        ]
    );
}

#[test]
fn background_copy_hint_names_the_platform_service() {
    for (os, machine, arm64, service) in [
        ("Darwin", "arm64", "1", "install-launchd"),
        ("Linux", "x86_64", "0", "install-systemd"),
    ] {
        let sb = Sandbox::new();
        publish_all(&sb);
        sb.platform(os, machine, arm64);
        let home = sb.root().join("user/.moraebot");
        fs::create_dir_all(home.join("bin")).expect("create background bin");
        fs::write(home.join("bin/moraebot"), "").expect("write background copy");
        fs::write(home.join("config.toml"), "").expect("write config");

        let output = sb.install(None);

        assert_eq!(exit_code(&output), Some(0), "{os}: {output:?}");
        let hint = format!("A background copy exists: run \"moraebot {service}\" to update it.");
        assert!(stdout_lines(&output).contains(&hint), "{os}: {output:?}");
    }
}

#[test]
fn background_copy_hint_sends_a_legacy_slack_config_to_setup_first() {
    let sb = Sandbox::new();
    publish_all(&sb);
    sb.platform("Linux", "x86_64", "0");
    let home = sb.root().join("user/.moraebot");
    fs::create_dir_all(home.join("bin")).expect("create background bin");
    fs::write(home.join("bin/moraebot"), "").expect("write background copy");
    fs::write(home.join("config.toml"), "messenger = \"slack\"\n[slack]\n").expect("write config");

    let output = sb.install(None);

    assert_eq!(exit_code(&output), Some(0), "{output:?}");
    let hint = "A background copy exists and config.toml still has a [slack] section: \
                run \"moraebot setup slack\", then \"moraebot install-systemd\".";
    assert!(
        stdout_lines(&output).iter().any(|line| line == hint),
        "{output:?}"
    );
}
