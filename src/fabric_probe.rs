//! Explicit diagnostic mode: real Fabric/Mixin, existing Rust world, isolated storage.
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
    if !valid_position(x, y, z) {
        let _ = env.throw_new(
            "java/lang/IllegalArgumentException",
            "Position outside diagnostic ABI",
        );
        return -1;
    }
    match catch_unwind(AssertUnwindSafe(|| super::block_at(x, y, z))) {
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
    let game_jar = classpath.split(':').next().ok_or("Missing game JAR")?;
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
        .map_err(|_| "Probe already initialized")?;
    let jvm = JavaVM::new(init)?;
    let mut env = jvm.attach_current_thread()?;
    let directory = std::env::current_dir()?;
    let result = (|| -> Result<bool, jni::errors::Error> {
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
        env.call_static_method("net/mcspine/probe/Bootstrap", "run", "()Z", &[])?
            .z()
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
