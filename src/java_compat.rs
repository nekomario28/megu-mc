//! One JVM owner and guarded Rust world ABI for pinned Fabric and NeoForge operations.
use crate::landing_types::{Loader, Request, Trample};
use jni::{
    objects::{JClass, JObject, JValue},
    sys::{jboolean, jint},
    InitArgsBuilder, JNIEnv, JNIVersion, JavaVM, NativeMethod,
};
use std::{
    ffi::c_void,
    panic::{catch_unwind, AssertUnwindSafe},
    path::Path,
    sync::OnceLock,
    thread::{self, ThreadId},
};

static TRAMPLE: OnceLock<bool> = OnceLock::new();
static OWNER: OnceLock<ThreadId> = OnceLock::new();
static SERVER: OnceLock<bool> = OnceLock::new();
static COMMITTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static ACTIVE: std::sync::Mutex<Option<Trample>> = std::sync::Mutex::new(None);
fn on_owner(env: &mut JNIEnv) -> bool {
    if OWNER.get() == Some(&thread::current().id()) {
        return true;
    }
    let _ = env.throw_new(
        "java/lang/IllegalStateException",
        "World access requires the owning thread",
    );
    false
}
fn valid_position(x: jint, y: jint, z: jint) -> bool {
    (x, y, z) == (8, 70, 8)
}
extern "system" fn set_block(
    mut env: JNIEnv,
    _class: JClass,
    x: jint,
    y: jint,
    z: jint,
    state: jint,
) -> jboolean {
    if !on_owner(&mut env) {
        return 0;
    }
    let valid_state = if TRAMPLE.get() == Some(&true) {
        state == 10 || state == 4286
    } else {
        (1..=2).contains(&state)
    };
    if SERVER.get() == Some(&true) {
        let active = *ACTIVE.lock().unwrap();
        if let Some(active) = active {
            if active.position == (x, y, z) && state == 10 {
                let dim = super::DimId::ALL
                    .get(usize::from(active.dim))
                    .copied()
                    .unwrap_or(super::DimId::Overworld);
                return match catch_unwind(AssertUnwindSafe(|| {
                    super::set_block_if_dim(dim, x, y, z, 10, Some(active.state))
                })) {
                    Ok(changed) => {
                        COMMITTED.store(changed, super::Ordering::Relaxed);
                        changed as u8
                    }
                    Err(_) => {
                        let _ = env.throw_new(
                            "java/lang/IllegalStateException",
                            "Rust world mutation failed",
                        );
                        0
                    }
                };
            }
        }
        let _ = env.throw_new(
            "java/lang/IllegalStateException",
            "No matching native landing operation",
        );
        return 0;
    }
    if !valid_position(x, y, z) || !valid_state {
        let _ = env.throw_new(
            "java/lang/IllegalArgumentException",
            "Position or state outside the scoped diagnostic ABI",
        );
        return 0;
    }
    match catch_unwind(AssertUnwindSafe(|| super::set_block(x, y, z, state as u32))) {
        Ok(()) => 1,
        Err(_) => {
            let _ = env.throw_new(
                "java/lang/IllegalStateException",
                "Rust world mutation failed",
            );
            0
        }
    }
}
extern "system" fn get_block(mut env: JNIEnv, _class: JClass, x: jint, y: jint, z: jint) -> jint {
    if !on_owner(&mut env) {
        return -1;
    }
    let active_dim = if SERVER.get() == Some(&true) {
        let active = *ACTIVE.lock().unwrap();
        match active {
            Some(a) if a.position == (x, y, z) => Some(
                super::DimId::ALL
                    .get(usize::from(a.dim))
                    .copied()
                    .unwrap_or(super::DimId::Overworld),
            ),
            _ => None,
        }
    } else if valid_position(x, y, z) {
        Some(super::DimId::Overworld)
    } else {
        None
    };
    let Some(dim) = active_dim else {
        let _ = env.throw_new(
            "java/lang/IllegalArgumentException",
            "Position outside diagnostic ABI",
        );
        return -1;
    };
    match catch_unwind(AssertUnwindSafe(|| super::block_at_dim(dim, x, y, z))) {
        Ok(state) => state as jint,
        Err(_) => {
            let _ = env.throw_new("java/lang/IllegalStateException", "Rust world read failed");
            -1
        }
    }
}

pub fn run(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let trample = args[0].starts_with("--fabric-trample");
    let valid_expected = |state: u32| {
        if trample {
            state == 10 || state == 4286
        } else {
            (1..=2).contains(&state)
        }
    };
    if args[0] == "--fabric-probe-reload" || args[0] == "--fabric-trample-reload" {
        if args.len() != 2 {
            return Err("usage: --fabric-probe-reload <expected-state>".into());
        }
        let expected: u32 = args[1].parse()?;
        if !valid_expected(expected) || !Path::new(super::WORLD_FILE).is_file() {
            return Err("Missing diagnostic world or invalid expected state".into());
        }
        super::load_world();
        let actual = super::block_at(8, 70, 8);
        if actual != expected {
            return Err(format!("Reload mismatch: {actual} != {expected}").into());
        }
        println!("{{\"reloaded_state\":{actual}}}");
        return Ok(());
    }
    if args[0] != "--fabric-probe" && args[0] != "--fabric-trample" {
        return Err("Unknown diagnostic command".into());
    }
    if args.len() != 3 {
        return Err("usage: --fabric-probe <classpath-file> <expected-state>".into());
    }
    let expected: u32 = args[2].parse()?;
    if !valid_expected(expected) {
        return Err("Expected state outside the selected contract".into());
    }
    if Path::new(super::WORLD_FILE).symlink_metadata().is_ok() {
        return Err("Refusing to overwrite an existing world".into());
    }
    if Path::new(&format!("{}.tmp", super::WORLD_FILE))
        .symlink_metadata()
        .is_ok()
    {
        return Err("Refusing to overwrite an existing temporary world".into());
    }
    TRAMPLE
        .set(trample)
        .map_err(|_| "Probe already initialized")?;
    if trample {
        if super::name_to_state("farmland") != Some(4286)
            || super::name_to_state("dirt") != Some(10)
        {
            return Err("Native state IDs differ from the pinned contract".into());
        }
        super::set_block(8, 70, 8, 4286);
    }
    let classpath = std::fs::read_to_string(&args[1])?;
    let jvm = create_jvm(&classpath)?;
    let mut env = jvm.attach_current_thread()?;
    let result = (|| -> Result<bool, Box<dyn std::error::Error>> {
        initialize_fabric(&mut env, &classpath, trample)?;
        Ok(env
            .call_static_method("net/mcspine/probe/Bootstrap", "run", "()Z", &[])?
            .z()?)
    })();
    let changed = match result {
        Ok(value) => value,
        Err(error) => {
            if env.exception_check()? {
                env.exception_describe()?;
            }
            return Err(error.into());
        }
    };
    let actual = super::block_at(8, 70, 8);
    if actual != expected || changed != (expected == if trample { 10 } else { 2 }) {
        return Err(format!(
            "World/Mixin mismatch: state={actual}, changed={changed}, expected={expected}"
        )
        .into());
    }
    super::save_world();
    if !Path::new(super::WORLD_FILE).is_file() {
        return Err("World persistence failed".into());
    }
    println!("{{\"stored_state\":{actual},\"changed\":{changed}}}");
    Ok(())
}

fn create_jvm(classpath: &str) -> Result<JavaVM, Box<dyn std::error::Error>> {
    let game_jar = classpath
        .trim()
        .split(':')
        .next()
        .ok_or("Missing game JAR")?;
    if !Path::new(game_jar).is_absolute() {
        return Err("Classpath must contain absolute paths".into());
    }
    let cp_option = format!("-Djava.class.path={}", classpath.trim());
    let init = InitArgsBuilder::new()
        .version(JNIVersion::V8)
        .option("-Xms16m")
        .option("-Xmx128m")
        .option("-Xcheck:jni")
        .option("-Djava.awt.headless=true")
        .option(&cp_option)
        .build()?;
    OWNER
        .set(thread::current().id())
        .map_err(|_| "JVM owner already initialized")?;
    Ok(JavaVM::new(init)?)
}
fn initialize_fabric(
    env: &mut JNIEnv,
    classpath: &str,
    trample: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let game_jar = classpath
        .trim()
        .split(':')
        .next()
        .ok_or("Missing game JAR")?;
    let directory = std::env::current_dir()?;
    let jar = JObject::from(env.new_string(game_jar)?);
    let dir = JObject::from(env.new_string(directory.to_string_lossy())?);
    let host = env
        .call_static_method(
            "net/mcspine/probe/Bootstrap",
            "initialize",
            "(Ljava/lang/String;Ljava/lang/String;Z)Ljava/lang/Class;",
            &[
                JValue::Object(&jar),
                JValue::Object(&dir),
                JValue::Bool(trample as u8),
            ],
        )?
        .l()?;
    register_host(env, host)?;
    Ok(())
}
pub fn serve(
    classpath_file: &str,
    loader: Loader,
    receive: std::sync::mpsc::Receiver<Request>,
    ready: std::sync::mpsc::Sender<Result<(), String>>,
) {
    let started = (|| -> Result<(), Box<dyn std::error::Error>> {
        SERVER
            .set(true)
            .map_err(|_| "Fabric server already initialized")?;
        TRAMPLE
            .set(true)
            .map_err(|_| "Fabric contract already initialized")?;
        if super::name_to_state("farmland") != Some(4286)
            || super::name_to_state("dirt") != Some(10)
        {
            return Err("Native state IDs differ from the pinned contract".into());
        }
        let classpath = std::fs::read_to_string(classpath_file)?;
        let jvm = create_jvm(&classpath)?;
        let mut env = jvm.attach_current_thread()?;
        let setup = (|| -> Result<jni::objects::GlobalRef, Box<dyn std::error::Error>> {
            let bootstrap = match loader {
                Loader::Fabric => {
                    initialize_fabric(&mut env, &classpath, true)?;
                    "net/mcspine/probe/Bootstrap"
                }
                Loader::NeoForge => {
                    let directory =
                        JObject::from(env.new_string(std::env::current_dir()?.to_string_lossy())?);
                    let host = env
                        .call_static_method(
                            "net/mcspine/neoforge/Bootstrap",
                            "initialize",
                            "(Ljava/lang/String;)Ljava/lang/Class;",
                            &[JValue::Object(&directory)],
                        )?
                        .l()?;
                    register_host(&mut env, host)?;
                    "net/mcspine/neoforge/Bootstrap"
                }
            };
            let hooks = env
                .call_static_method(bootstrap, "initializeServer", "()Ljava/lang/Class;", &[])?
                .l()?;
            Ok(env.new_global_ref(hooks)?)
        })();
        let hooks = match setup {
            Ok(hooks) => hooks,
            Err(error) => {
                if env.exception_check()? {
                    env.exception_describe()?;
                }
                return Err(error);
            }
        };
        ready.send(Ok(())).map_err(|_| "Server startup abandoned")?;
        for request in receive {
            let input = request.trample;
            COMMITTED.store(false, super::Ordering::Relaxed);
            *ACTIVE.lock().unwrap() = Some(input);
            let (x, y, z) = input.position;
            let outcome = env.with_local_frame(16, |env| {
                env.call_static_method(
                    <&JClass>::from(hooks.as_obj()),
                    "land",
                    "(IIIF)V",
                    &[
                        JValue::Int(x),
                        JValue::Int(y),
                        JValue::Int(z),
                        JValue::Float(input.distance),
                    ],
                )?;
                Ok::<(), jni::errors::Error>(())
            });
            *ACTIVE.lock().unwrap() = None;
            let result = match outcome {
                Ok(()) => Ok(COMMITTED.load(super::Ordering::Relaxed)),
                Err(error) => {
                    if env.exception_check()? {
                        env.exception_describe()?;
                        env.exception_clear()?;
                    }
                    Err(error.to_string())
                }
            };
            let failed = result.is_err();
            let _ = request.reply.send(result);
            if failed {
                return Err("Java callback failed; owner stopped".into());
            }
        }
        Ok(())
    })();
    if let Err(error) = started {
        let _ = ready.send(Err(error.to_string()));
        eprintln!("[mod-compatibility] {error}");
    }
}

fn register_host(env: &mut JNIEnv, host: JObject) -> Result<(), jni::errors::Error> {
    env.register_native_methods(
        JClass::from(host),
        &[
            NativeMethod {
                name: "setBlock".into(),
                sig: "(IIII)Z".into(),
                fn_ptr: set_block as *mut c_void,
            },
            NativeMethod {
                name: "getBlock".into(),
                sig: "(III)I".into(),
                fn_ptr: get_block as *mut c_void,
            },
        ],
    )?;
    Ok(())
}
