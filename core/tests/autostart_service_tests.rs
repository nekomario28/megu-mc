// SPDX-License-Identifier: LGPL-3.0-or-later
use megu_core::autostart_service::{
    apply_file_install, apply_file_uninstall, inspect_status, inspect_status_with_enabled,
    plan_install, plan_uninstall, InstallStatus, ServiceBackend, ServiceSpec, WindowsTrigger,
};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);
fn temp_root() -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "megu-autostart-test-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir_all(&path).unwrap();
    path
}
fn spec(name: &str, backend: ServiceBackend) -> ServiceSpec {
    let mut value = ServiceSpec::new(name, backend, "/opt/megu/server", "/srv/megu/world");
    value.extra_args = vec!["--port".into(), "25565".into()];
    if backend == ServiceBackend::Runit {
        value.service_dir = Some("/etc/sv/megu-survival".into());
    }
    value
}

#[test]
fn all_seven_backends_roundtrip_without_touching_world_data() {
    let backends = [
        ServiceBackend::SystemdUser,
        ServiceBackend::SystemdSystem,
        ServiceBackend::OpenRc,
        ServiceBackend::Runit,
        ServiceBackend::LaunchdUser,
        ServiceBackend::WindowsTaskScheduler,
        ServiceBackend::FreeBsdRc,
    ];
    for backend in backends {
        let root = temp_root();
        let world = root.join("srv/megu/world");
        fs::create_dir_all(&world).unwrap();
        fs::write(world.join("level.dat"), b"world data stays here").unwrap();
        let install = plan_install(&spec("megu-survival", backend)).unwrap();
        assert!(!install.files_to_write.is_empty(), "{backend:?}");
        apply_file_install(&root, &install).unwrap();
        assert_eq!(
            inspect_status(&root, &install).unwrap(),
            InstallStatus::InstalledFilesOnly,
            "{backend:?}"
        );
        assert_eq!(
            inspect_status_with_enabled(&root, &install, true).unwrap(),
            InstallStatus::Enabled,
            "{backend:?}"
        );
        let uninstall = plan_uninstall(&spec("megu-survival", backend)).unwrap();
        apply_file_uninstall(&root, &uninstall).unwrap();
        assert_eq!(
            inspect_status(&root, &install).unwrap(),
            InstallStatus::NotInstalled,
            "{backend:?}"
        );
        assert_eq!(
            fs::read(world.join("level.dat")).unwrap(),
            b"world data stays here"
        );
        fs::remove_dir_all(root).unwrap();
    }
}

#[test]
fn emits_backend_specific_service_contracts() {
    let user = plan_install(&spec("megu-survival", ServiceBackend::SystemdUser)).unwrap();
    let unit = String::from_utf8(user.files_to_write[0].contents.clone()).unwrap();
    for line in [
        "Type=notify",
        "Restart=on-failure",
        "RestartSec=5",
        "TimeoutStopSec=60",
        "KillSignal=SIGTERM",
        "WantedBy=default.target",
    ] {
        assert!(unit.contains(line));
    }
    assert!(user
        .commands
        .iter()
        .any(|argv| argv.first().is_some_and(|s| s == "loginctl")));

    let system = plan_install(&spec("megu-survival", ServiceBackend::SystemdSystem)).unwrap();
    let unit = String::from_utf8(system.files_to_write[0].contents.clone()).unwrap();
    for line in [
        "NoNewPrivileges=yes",
        "ProtectSystem=strict",
        "ReadWritePaths=",
        "PrivateTmp=yes",
    ] {
        assert!(unit.contains(line));
    }

    let openrc = plan_install(&spec("megu-survival", ServiceBackend::OpenRc)).unwrap();
    let script = String::from_utf8(openrc.files_to_write[0].contents.clone()).unwrap();
    assert!(script.contains("command_background=true"));
    assert!(script.contains("TERM/60"));
    assert!(openrc
        .commands
        .iter()
        .any(|argv| argv.first().is_some_and(|s| s == "rc-update")));

    let runit = plan_install(&spec("megu-survival", ServiceBackend::Runit)).unwrap();
    assert!(runit
        .files_to_write
        .iter()
        .any(|file| file.path.file_name().is_some_and(|name| name == "run")));
    assert!(runit
        .files_to_write
        .iter()
        .any(|file| file.path.file_name().is_some_and(|name| name == "finish")));

    let launchd = plan_install(&spec("megu-survival", ServiceBackend::LaunchdUser)).unwrap();
    let plist = String::from_utf8(launchd.files_to_write[0].contents.clone()).unwrap();
    assert!(plist.contains("<key>RunAtLoad</key><true/>"));
    assert!(plist.contains("<key>KeepAlive</key><true/>"));

    let mut windows_spec = spec("megu-survival", ServiceBackend::WindowsTaskScheduler);
    windows_spec.binary_path = r"C:\Megu\server&<main>.exe".into();
    windows_spec.world_dir = r"D:\worlds\survival&<one>".into();
    windows_spec.windows_trigger = WindowsTrigger::OnLogon;
    windows_spec.extra_args.push("summer & winter".into());
    let windows = plan_install(&windows_spec).unwrap();
    let xml = String::from_utf8(windows.files_to_write[0].contents.clone()).unwrap();
    assert!(xml.contains("<LogonTrigger>"));
    assert!(xml.contains("server&amp;&lt;main&gt;.exe"));
    assert!(xml.contains("&quot;summer &amp; winter&quot;"));
    assert!(windows
        .commands
        .iter()
        .any(|argv| argv.first().is_some_and(|s| s == "schtasks.exe")
            && argv.iter().any(|s| s == "/Create")));
    assert!(windows
        .commands
        .iter()
        .any(|argv| argv.iter().any(|s| s == "/Create")));
    assert!(!windows
        .commands
        .iter()
        .any(|argv| argv.iter().any(|s| s == "/Delete")));
    let windows_uninstall = plan_uninstall(&windows_spec).unwrap();
    assert!(windows_uninstall
        .commands
        .iter()
        .any(|argv| argv.iter().any(|s| s == "/Delete")));

    let freebsd = plan_install(&spec("megu-survival", ServiceBackend::FreeBsdRc)).unwrap();
    assert_eq!(
        freebsd.files_to_write[0].path.file_name().unwrap(),
        "megu-survival"
    );
    assert!(
        String::from_utf8(freebsd.files_to_write[0].contents.clone())
            .unwrap()
            .contains("[ \"$tries\" -lt 60 ]")
    );
    assert!(freebsd
        .commands
        .iter()
        .any(|argv| argv.first().is_some_and(|s| s == "sysrc")
            && argv.iter().any(|s| s.ends_with("_enable=YES"))));
}

#[test]
fn rejects_names_paths_and_control_character_injection() {
    for name in [
        "../escape",
        "Upper",
        "x\nEnable=yes",
        "",
        "a".repeat(49).as_str(),
    ] {
        assert!(
            plan_install(&spec(name, ServiceBackend::SystemdUser)).is_err(),
            "accepted {name:?}"
        );
    }
    for invalid in [
        "relative/server",
        "/srv/../outside/server",
        "/srv/server\nExecStart=/tmp/pwn",
        "/srv/server\r",
        "/srv/server\0x",
        "/srv/$HOME/server",
        "/srv/a;b",
    ] {
        let mut value = spec("megu-safe", ServiceBackend::SystemdUser);
        value.binary_path = invalid.to_owned();
        assert!(
            plan_install(&value).is_err(),
            "accepted binary path {invalid:?}"
        );
    }
    for arg in ["--name\nExecStart=/tmp/pwn", "$(touch pwn)", "arg|other"] {
        let mut value = spec("megu-safe", ServiceBackend::LaunchdUser);
        value.extra_args = vec![arg.into()];
        assert!(plan_install(&value).is_err(), "accepted argument {arg:?}");
    }
}

#[test]
fn named_instances_have_disjoint_install_paths() {
    for backend in [
        ServiceBackend::SystemdUser,
        ServiceBackend::SystemdSystem,
        ServiceBackend::OpenRc,
        ServiceBackend::LaunchdUser,
        ServiceBackend::WindowsTaskScheduler,
        ServiceBackend::FreeBsdRc,
    ] {
        let a = plan_install(&spec("megu-survival", backend)).unwrap();
        let b = plan_install(&spec("megu-creative", backend)).unwrap();
        let a_paths: Vec<_> = a.files_to_write.iter().map(|file| &file.path).collect();
        let b_paths: Vec<_> = b.files_to_write.iter().map(|file| &file.path).collect();
        assert!(
            a_paths.iter().all(|path| !b_paths.contains(path)),
            "{backend:?}"
        );
    }
    let mut a = spec("megu-survival", ServiceBackend::Runit);
    let mut b = spec("megu-creative", ServiceBackend::Runit);
    a.service_dir = Some("/etc/sv/megu-survival".into());
    b.service_dir = Some("/etc/sv/megu-creative".into());
    assert_ne!(
        plan_install(&a).unwrap().files_to_write[0].path,
        plan_install(&b).unwrap().files_to_write[0].path
    );
}

#[test]
fn uninstall_refuses_modified_files_and_symlink_escape() {
    let root = temp_root();
    let plan = plan_install(&spec("megu-safe", ServiceBackend::SystemdUser)).unwrap();
    apply_file_install(&root, &plan).unwrap();
    let path = root.join(&plan.files_to_write[0].path);
    fs::write(&path, b"foreign content").unwrap();
    assert!(apply_file_uninstall(
        &root,
        &plan_uninstall(&spec("megu-safe", ServiceBackend::SystemdUser)).unwrap()
    )
    .is_err());
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn status_is_corrupt_for_partial_install_and_symlinked_destination() {
    let root = temp_root();
    let runit = plan_install(&spec("megu-safe", ServiceBackend::Runit)).unwrap();
    fs::create_dir_all(root.join("etc/sv/megu-survival")).unwrap();
    fs::write(
        root.join(&runit.files_to_write[0].path),
        &runit.files_to_write[0].contents,
    )
    .unwrap();
    assert_eq!(
        inspect_status(&root, &runit).unwrap(),
        InstallStatus::Corrupt
    );
    fs::remove_dir_all(&root).unwrap();

    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        let root = temp_root();
        let plan = plan_install(&spec("megu-safe", ServiceBackend::SystemdUser)).unwrap();
        let outside = root.with_extension("outside");
        fs::create_dir_all(&outside).unwrap();
        symlink(&outside, root.join(".config")).unwrap();
        assert!(apply_file_install(&root, &plan).is_err());
        fs::remove_dir_all(&root).unwrap();
        fs::remove_dir_all(outside).unwrap();
    }
}
