// SPDX-License-Identifier: LGPL-3.0-or-later
#![forbid(unsafe_code)]
//! Pure planning and hermetic file operations for explicitly opted-in server
//! autostart services. Manager commands are returned as argv vectors; this
//! module never starts or enables a service itself.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ServiceBackend {
    SystemdUser,
    SystemdSystem,
    OpenRc,
    Runit,
    LaunchdUser,
    WindowsTaskScheduler,
    FreeBsdRc,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WindowsTrigger {
    OnStart,
    OnLogon,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServiceSpec {
    pub name: String,
    pub backend: ServiceBackend,
    /// Executable path. Must be absolute in the target platform's syntax.
    pub binary_path: String,
    /// Server world path. Must be absolute in the target platform's syntax.
    pub world_dir: String,
    /// Each entry is one argument, never a shell command fragment.
    pub extra_args: Vec<String>,
    /// Required for Runit; this is the service-specific directory.
    pub service_dir: Option<String>,
    pub windows_trigger: WindowsTrigger,
}

impl ServiceSpec {
    pub fn new(
        name: impl Into<String>,
        backend: ServiceBackend,
        binary_path: impl Into<String>,
        world_dir: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            backend,
            binary_path: binary_path.into(),
            world_dir: world_dir.into(),
            extra_args: Vec::new(),
            service_dir: None,
            windows_trigger: WindowsTrigger::OnStart,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlannedFile {
    /// Relative destination inside the root supplied to apply/status.
    pub path: PathBuf,
    pub contents: Vec<u8>,
    /// Unix permission bits; ignored on platforms without Unix permissions.
    pub mode: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServicePlan {
    pub files_to_write: Vec<PlannedFile>,
    pub files_to_remove: Vec<PathBuf>,
    /// Explicit argv calls for the caller to review and execute if desired.
    pub commands: Vec<Vec<String>>,
    owned_files: Vec<PlannedFile>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InstallStatus {
    NotInstalled,
    InstalledFilesOnly,
    Enabled,
    Corrupt,
}

#[derive(Debug)]
pub enum ServiceError {
    InvalidInput(&'static str),
    Io(io::Error),
}

impl std::fmt::Display for ServiceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidInput(reason) => write!(f, "invalid service specification: {reason}"),
            Self::Io(error) => write!(f, "service file operation failed: {error}"),
        }
    }
}
impl std::error::Error for ServiceError {}
impl From<io::Error> for ServiceError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

pub fn plan_install(spec: &ServiceSpec) -> Result<ServicePlan, ServiceError> {
    validate_spec(spec)?;
    let name = &spec.name;
    let args = std::iter::once(spec.binary_path.clone())
        .chain(std::iter::once("--world".to_owned()))
        .chain(std::iter::once(spec.world_dir.clone()))
        .chain(spec.extra_args.iter().cloned())
        .collect::<Vec<_>>();
    let mut files = Vec::new();
    let mut commands = Vec::new();
    match spec.backend {
        ServiceBackend::SystemdUser | ServiceBackend::SystemdSystem => {
            let user = spec.backend == ServiceBackend::SystemdUser;
            let path = if user {
                PathBuf::from(format!(".config/systemd/user/{name}.service"))
            } else {
                PathBuf::from(format!("etc/systemd/system/{name}.service"))
            };
            let mut unit = String::from("[Unit]\nDescription=Megu server instance ");
            unit.push_str(name);
            unit.push_str("\nAfter=network.target\n\n[Service]\nType=notify\nExecStart=");
            unit.push_str(
                &args
                    .iter()
                    .map(|s| systemd_quote(s))
                    .collect::<Vec<_>>()
                    .join(" "),
            );
            unit.push_str(
                "\nRestart=on-failure\nRestartSec=5\nTimeoutStopSec=60\nKillSignal=SIGTERM\n",
            );
            if !user {
                unit.push_str("NoNewPrivileges=yes\nProtectSystem=strict\nReadWritePaths=");
                unit.push_str(&systemd_quote(&spec.world_dir));
                unit.push_str("\nPrivateTmp=yes\n");
            }
            unit.push_str("\n[Install]\nWantedBy=");
            unit.push_str(if user {
                "default.target\n"
            } else {
                "multi-user.target\n"
            });
            files.push(file(path, unit, 0o644));
            if user {
                commands.push(vec![
                    "systemctl".into(),
                    "--user".into(),
                    "daemon-reload".into(),
                ]);
                commands.push(vec![
                    "systemctl".into(),
                    "--user".into(),
                    "enable".into(),
                    format!("{name}.service"),
                ]);
                commands.push(vec![
                    "loginctl".into(),
                    "enable-linger".into(),
                    "<user>".into(),
                ]);
            } else {
                commands.push(vec!["systemctl".into(), "daemon-reload".into()]);
                commands.push(vec![
                    "systemctl".into(),
                    "enable".into(),
                    format!("{name}.service"),
                ]);
            }
        }
        ServiceBackend::OpenRc => {
            let mut command_args = vec!["--world".to_owned(), spec.world_dir.clone()];
            command_args.extend(spec.extra_args.iter().cloned());
            let contents = format!("#!/sbin/openrc-run\nname={}\ncommand={}\ncommand_args={}\ncommand_background=true\npidfile=\"/run/${{RC_SVCNAME}}.pid\"\nretry=\"TERM/60\"\n", shell_quote(name), shell_quote(&spec.binary_path), shell_quote(&command_args.iter().map(|v| shell_quote(v)).collect::<Vec<_>>().join(" ")));
            files.push(file(format!("etc/init.d/{name}"), contents, 0o755));
            commands.push(vec![
                "rc-update".into(),
                "add".into(),
                name.clone(),
                "default".into(),
            ]);
        }
        ServiceBackend::Runit => {
            let service_dir = spec
                .service_dir
                .as_ref()
                .expect("validated runit directory");
            let base = PathBuf::from(service_dir.trim_start_matches('/'));
            let run = format!(
                "#!/bin/sh\nexec {}\n",
                args.iter()
                    .map(|v| shell_quote(v))
                    .collect::<Vec<_>>()
                    .join(" ")
            );
            let finish = "#!/bin/sh\nexit 0\n";
            files.push(file(base.join("run"), run, 0o755));
            files.push(file(base.join("finish"), finish, 0o755));
        }
        ServiceBackend::LaunchdUser => {
            let path = format!("Library/LaunchAgents/dev.megu.{name}.plist");
            let plist = format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\"><dict><key>Label</key><string>dev.megu.{}</string><key>ProgramArguments</key><array>{}</array><key>RunAtLoad</key><true/><key>KeepAlive</key><true/><key>WorkingDirectory</key><string>{}</string><key>StandardOutPath</key><string>/tmp/dev.megu.{}.out.log</string><key>StandardErrorPath</key><string>/tmp/dev.megu.{}.err.log</string></dict></plist>\n", xml_escape(name), args.iter().map(|v| format!("<string>{}</string>", xml_escape(v))).collect::<String>(), xml_escape(&spec.world_dir), xml_escape(name), xml_escape(name));
            files.push(file(path, plist, 0o644));
            commands.push(vec![
                "launchctl".into(),
                "load".into(),
                format!("~/Library/LaunchAgents/dev.megu.{name}.plist"),
            ]);
        }
        ServiceBackend::WindowsTaskScheduler => {
            let path = format!("Windows/System32/Tasks/Megu/{name}");
            let trigger = match spec.windows_trigger {
                WindowsTrigger::OnStart => "<BootTrigger><Enabled>true</Enabled></BootTrigger>",
                WindowsTrigger::OnLogon => "<LogonTrigger><Enabled>true</Enabled></LogonTrigger>",
            };
            let arguments = args
                .iter()
                .skip(1)
                .map(|value| xml_escape(&windows_quote(value)))
                .collect::<Vec<_>>()
                .join(" ");
            let xml = format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<Task version=\"1.2\" xmlns=\"http://schemas.microsoft.com/windows/2004/02/mit/task\"><Triggers>{trigger}</Triggers><Principals><Principal id=\"Author\"><RunLevel>LeastPrivilege</RunLevel></Principal></Principals><Settings><MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy><RestartOnFailure><Interval>PT5M</Interval><Count>3</Count></RestartOnFailure></Settings><Actions Context=\"Author\"><Exec><Command>{}</Command><Arguments>{arguments}</Arguments><WorkingDirectory>{}</WorkingDirectory></Exec></Actions></Task>\n", xml_escape(&spec.binary_path), xml_escape(&spec.world_dir));
            files.push(file(path, xml, 0o644));
            let task = format!("\\Megu\\{name}");
            commands.push(vec![
                "schtasks.exe".into(),
                "/Create".into(),
                "/TN".into(),
                task,
                "/XML".into(),
                format!("C:\\Windows\\System32\\Tasks\\Megu\\{name}"),
            ]);
        }
        ServiceBackend::FreeBsdRc => {
            let mut command_args = vec!["--world".to_owned(), spec.world_dir.clone()];
            command_args.extend(spec.extra_args.iter().cloned());
            let script = format!("#!/bin/sh\n# PROVIDE: {name}\n# REQUIRE: LOGIN\n. /etc/rc.subr\nname={}\nrcvar={}\ncommand={}\ncommand_args={}\npidfile=\"/var/run/${{name}}.pid\"\nstop_cmd=\"${{name}}_stop\"\n\n${{name}}_stop() {{\n  pid=\"$(cat \"${{pidfile}}\")\"\n  kill -TERM \"$pid\"\n  tries=0\n  while kill -0 \"$pid\" 2>/dev/null && [ \"$tries\" -lt 60 ]; do\n    sleep 1\n    tries=$((tries + 1))\n  done\n  kill -0 \"$pid\" 2>/dev/null && kill -KILL \"$pid\"\n}}\n\nload_rc_config ${{name}}\n: ${{{}_enable:=NO}}\nrun_rc_command \"$1\"\n", name, name, shell_quote(&spec.binary_path), shell_quote(&command_args.iter().map(|v| shell_quote(v)).collect::<Vec<_>>().join(" ")), name);
            files.push(file(format!("usr/local/etc/rc.d/{name}"), script, 0o755));
            commands.push(vec!["sysrc".into(), format!("{name}_enable=YES")]);
        }
    }
    let remove = files.iter().map(|item| item.path.clone()).collect();
    Ok(ServicePlan {
        files_to_write: files.clone(),
        files_to_remove: remove,
        commands,
        owned_files: files,
    })
}

pub fn plan_uninstall(spec: &ServiceSpec) -> Result<ServicePlan, ServiceError> {
    let mut plan = plan_install(spec)?;
    plan.files_to_remove = plan
        .owned_files
        .iter()
        .map(|item| item.path.clone())
        .collect();
    plan.files_to_write.clear();
    plan.commands.retain(|argv| {
        !argv
            .iter()
            .any(|arg| arg == "enable" || arg == "add" || arg == "/Create")
    });
    match spec.backend {
        ServiceBackend::SystemdUser => {
            plan.commands.push(vec![
                "systemctl".into(),
                "--user".into(),
                "disable".into(),
                format!("{}.service", spec.name),
            ]);
            plan.commands.push(vec![
                "systemctl".into(),
                "--user".into(),
                "daemon-reload".into(),
            ]);
        }
        ServiceBackend::SystemdSystem => {
            plan.commands.push(vec![
                "systemctl".into(),
                "disable".into(),
                format!("{}.service", spec.name),
            ]);
            plan.commands
                .push(vec!["systemctl".into(), "daemon-reload".into()]);
        }
        ServiceBackend::OpenRc => plan.commands.push(vec![
            "rc-update".into(),
            "del".into(),
            spec.name.clone(),
            "default".into(),
        ]),
        ServiceBackend::FreeBsdRc => plan
            .commands
            .push(vec!["sysrc".into(), format!("{}_enable=NO", spec.name)]),
        ServiceBackend::LaunchdUser => plan.commands.push(vec![
            "launchctl".into(),
            "unload".into(),
            format!("~/Library/LaunchAgents/dev.megu.{}.plist", spec.name),
        ]),
        ServiceBackend::WindowsTaskScheduler => {
            plan.commands = vec![vec![
                "schtasks.exe".into(),
                "/Delete".into(),
                "/TN".into(),
                format!("\\Megu\\{}", spec.name),
                "/F".into(),
            ]]
        }
        ServiceBackend::Runit => {}
    }
    Ok(plan)
}

pub fn apply_file_install(root: &Path, plan: &ServicePlan) -> Result<(), ServiceError> {
    for planned in &plan.files_to_write {
        validate_relative(&planned.path)?;
        let destination = safe_destination(root, &planned.path)?;
        let parent = destination
            .parent()
            .ok_or(ServiceError::InvalidInput("file has no parent"))?;
        create_dirs_no_symlinks(root, parent)?;
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let temp = parent.join(format!(
            ".megu-autostart-{}-{sequence}.tmp",
            std::process::id()
        ));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        let mut handle = options.open(&temp)?;
        if let Err(error) = handle
            .write_all(&planned.contents)
            .and_then(|_| handle.sync_all())
        {
            let _ = fs::remove_file(&temp);
            return Err(error.into());
        }
        set_mode(&temp, planned.mode)?;
        drop(handle);
        if let Err(error) = fs::rename(&temp, &destination) {
            let _ = fs::remove_file(&temp);
            return Err(error.into());
        }
    }
    Ok(())
}

pub fn apply_file_uninstall(root: &Path, plan: &ServicePlan) -> Result<(), ServiceError> {
    for relative in &plan.files_to_remove {
        validate_relative(relative)?;
        let owned = plan
            .owned_files
            .iter()
            .find(|item| item.path == *relative)
            .ok_or(ServiceError::InvalidInput(
                "uninstall path is not owned by this service plan",
            ))?;
        let destination = safe_destination(root, relative)?;
        match fs::read(&destination) {
            Ok(contents) if contents == owned.contents => fs::remove_file(destination)?,
            Ok(_) => {
                return Err(ServiceError::InvalidInput(
                    "refusing to remove a modified or foreign file",
                ))
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

pub fn inspect_status(root: &Path, plan: &ServicePlan) -> Result<InstallStatus, ServiceError> {
    inspect_status_with_enabled(root, plan, false)
}

/// Inspect files and apply an enabled-state observation obtained by the
/// caller from the native manager (for example `systemctl is-enabled`).
/// Keeping that observation explicit makes the function hermetic and avoids
/// executing host commands while examining a projected install root.
pub fn inspect_status_with_enabled(
    root: &Path,
    plan: &ServicePlan,
    manager_reports_enabled: bool,
) -> Result<InstallStatus, ServiceError> {
    let mut existing = 0;
    for owned in &plan.owned_files {
        let destination = safe_destination(root, &owned.path)?;
        match fs::read(destination) {
            Ok(contents) if contents == owned.contents => existing += 1,
            Ok(_) => return Ok(InstallStatus::Corrupt),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    if existing == 0 {
        Ok(InstallStatus::NotInstalled)
    } else if existing == plan.owned_files.len() {
        Ok(if manager_reports_enabled {
            InstallStatus::Enabled
        } else {
            InstallStatus::InstalledFilesOnly
        })
    } else {
        Ok(InstallStatus::Corrupt)
    }
}

fn validate_spec(spec: &ServiceSpec) -> Result<(), ServiceError> {
    if !valid_name(&spec.name) {
        return Err(ServiceError::InvalidInput(
            "service name must match [a-z0-9][a-z0-9_-]{0,47}",
        ));
    }
    validate_value(&spec.binary_path)?;
    validate_value(&spec.world_dir)?;
    if !is_absolute_target(&spec.binary_path) || !is_absolute_target(&spec.world_dir) {
        return Err(ServiceError::InvalidInput(
            "binary_path and world_dir must be absolute",
        ));
    }
    for arg in &spec.extra_args {
        validate_value(arg)?;
    }
    if spec.backend == ServiceBackend::Runit {
        let dir = spec
            .service_dir
            .as_ref()
            .ok_or(ServiceError::InvalidInput("runit service_dir is required"))?;
        validate_value(dir)?;
        if !is_absolute_target(dir) {
            return Err(ServiceError::InvalidInput(
                "runit service_dir must be absolute",
            ));
        }
    }
    Ok(())
}
fn valid_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 48
        && (bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit())
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'_' || *b == b'-')
}
fn validate_value(value: &str) -> Result<(), ServiceError> {
    if value.is_empty()
        || value
            .bytes()
            .any(|byte| byte == 0 || byte.is_ascii_control())
    {
        return Err(ServiceError::InvalidInput(
            "paths and arguments cannot be empty or contain NUL/control characters",
        ));
    }
    if value.contains('`') || value.contains('$') || value.contains(';') || value.contains('|') {
        return Err(ServiceError::InvalidInput(
            "shell and service-manager control characters are forbidden",
        ));
    }
    if value.split(['/', '\\']).any(|part| part == "..") {
        return Err(ServiceError::InvalidInput("parent traversal is forbidden"));
    }
    Ok(())
}
fn is_absolute_target(value: &str) -> bool {
    value.starts_with('/')
        || (value.as_bytes().get(1) == Some(&b':')
            && value.as_bytes()[0].is_ascii_alphabetic()
            && (value.contains('\\') || value.contains('/')))
}
fn file(path: impl Into<PathBuf>, content: impl Into<String>, mode: u32) -> PlannedFile {
    PlannedFile {
        path: path.into(),
        contents: content.into().into_bytes(),
        mode,
    }
}
fn validate_relative(path: &Path) -> Result<(), ServiceError> {
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err(ServiceError::InvalidInput(
            "plan paths must be safe relative paths",
        ));
    }
    Ok(())
}
fn safe_destination(root: &Path, relative: &Path) -> Result<PathBuf, ServiceError> {
    validate_relative(relative)?;
    let root = fs::canonicalize(root)?;
    let target = root.join(relative);
    let mut current = root;
    for component in relative.components() {
        let Component::Normal(part) = component else {
            return Err(ServiceError::InvalidInput("unsafe plan path"));
        };
        current.push(part);
        if let Ok(meta) = fs::symlink_metadata(&current) {
            if meta.file_type().is_symlink() {
                return Err(ServiceError::InvalidInput("symlink in service root"));
            }
        }
    }
    Ok(target)
}
fn create_dirs_no_symlinks(root: &Path, parent: &Path) -> Result<(), ServiceError> {
    let root = fs::canonicalize(root)?;
    let relative = parent
        .strip_prefix(&root)
        .map_err(|_| ServiceError::InvalidInput("destination escaped root"))?;
    let mut current = root;
    for component in relative.components() {
        let Component::Normal(part) = component else {
            return Err(ServiceError::InvalidInput("unsafe parent path"));
        };
        current.push(part);
        match fs::symlink_metadata(&current) {
            Ok(meta) if meta.file_type().is_symlink() || !meta.is_dir() => {
                return Err(ServiceError::InvalidInput("unsafe directory component"))
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => fs::create_dir(&current)?,
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}
#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> Result<(), ServiceError> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    Ok(())
}
#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) -> Result<(), ServiceError> {
    Ok(())
}
fn systemd_quote(value: &str) -> String {
    format!(
        "\"{}\"",
        value
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace(' ', "\\s")
    )
}
fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}
fn windows_quote(value: &str) -> String {
    if !value.is_empty() && !value.chars().any(char::is_whitespace) && !value.contains('"') {
        return value.to_owned();
    }
    let mut result = String::from("\"");
    let mut slashes = 0;
    for ch in value.chars() {
        if ch == '\\' {
            slashes += 1;
        } else if ch == '"' {
            result.push_str(&"\\".repeat(slashes * 2 + 1));
            result.push('"');
            slashes = 0;
        } else {
            result.push_str(&"\\".repeat(slashes));
            result.push(ch);
            slashes = 0;
        }
    }
    result.push_str(&"\\".repeat(slashes * 2));
    result.push('"');
    result
}
