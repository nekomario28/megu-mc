//! One JVM owner and guarded Rust world ABI for pinned Fabric and NeoForge operations.
#[cfg(feature = "neoforge-events")]
use crate::landing_types::ModRegistrySnapshot;
use crate::landing_types::{
    decode_mutation_batch, Loader, MutationOp, Request, S1EventOutcome, Trample,
};
#[cfg(feature = "neoforge-events")]
use crate::landing_types::{
    decode_s3_response_batch, encode_s3_request_batch, S3BatchOp, S3TickRequest, S3_CAPACITY,
};
#[cfg(feature = "neoforge-events")]
use jni::{
    objects::{AutoLocal, JByteBuffer, JStaticMethodID},
    signature::{Primitive, ReturnType},
};
use jni::{
    objects::{JClass, JObject, JString, JValue},
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

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct BatchCommitReport {
    pub(crate) applied_blocks: usize,
    pub(crate) skipped_cas_blocks: usize,
    pub(crate) system_messages: Vec<(i32, String)>,
    pub(crate) registered_commands: Vec<String>,
}

/// Commit in batch order. Each block write rechecks its own guard, and a save
/// waits for the whole batch. The caller delivers the returned side effects.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn apply_s1_outcome(outcome: S1EventOutcome) -> Result<BatchCommitReport, String> {
    let mut report = BatchCommitReport::default();
    if outcome.canceled {
        return Ok(report);
    }
    // Validate typed callers as well as wire callers before changing any block.
    for mutation in &outcome.mutations {
        if let MutationOp::SetBlockIf { dim, .. } = mutation {
            if usize::from(*dim) >= super::DimId::ALL.len() {
                return Err(format!("invalid S1 dimension slot {dim}"));
            }
        }
    }
    super::with_save_guard(|| {
        for mutation in outcome.mutations {
            match mutation {
                MutationOp::SetBlockIf {
                    dim,
                    pos: (x, y, z),
                    new_state,
                    expected_state,
                } => {
                    let dim = super::DimId::ALL[usize::from(dim)];
                    if super::set_block_if_dim(dim, x, y, z, new_state, expected_state) {
                        report.applied_blocks += 1;
                    } else {
                        report.skipped_cas_blocks += 1;
                    }
                }
                MutationOp::SendSystemMessage {
                    player_eid,
                    message,
                } => {
                    report.system_messages.push((player_eid, message));
                }
                MutationOp::RegisterCommand { literal } => report.registered_commands.push(literal),
            }
        }
    });
    Ok(report)
}

#[allow(dead_code)] // S1 requests will be routed by the server separately from trample.
pub(crate) enum S1Event<'a> {
    BlockBreak {
        dim: u8,
        pos: (i32, i32, i32),
        state: u32,
        player: (i32, &'a str),
        exp_to_drop: i32,
    },
    BlockPlace {
        dim: u8,
        pos: (i32, i32, i32),
        replaced: u32,
        placed: u32,
        against: u32,
        player: (i32, &'a str),
    },
    PlayerLogin(i32, &'a str),
    PlayerLogout(i32, &'a str),
    ServerChat {
        player: (i32, &'a str),
        raw_text: &'a str,
    },
    RegisterCommands,
}

/// Dispatch a NeoForge S1 event on the JVM owner. This is staging only. A
/// Java exception or an invalid wire batch returns no outcome to commit.
#[allow(dead_code)]
pub(crate) fn dispatch_s1_event(
    env: &mut JNIEnv,
    hooks: &JClass,
    event: S1Event<'_>,
) -> Result<S1EventOutcome, String> {
    if OWNER.get() != Some(&thread::current().id()) {
        return Err("S1 dispatch requires the JVM owner thread".into());
    }
    let wire = env.with_local_frame(16, |env| -> jni::errors::Result<String> {
        // Java's int carries the same 32 bits as each u32 block state ID.
        let result = match event {
            S1Event::BlockBreak {
                dim,
                pos: (x, y, z),
                state,
                player: (eid, name),
                exp_to_drop,
            } => {
                let name = env.new_string(name)?;
                env.call_static_method(
                    hooks,
                    "dispatchBlockBreakEncoded",
                    "(IIIIIILjava/lang/String;I)Ljava/lang/String;",
                    &[
                        JValue::Int(i32::from(dim)),
                        JValue::Int(x),
                        JValue::Int(y),
                        JValue::Int(z),
                        JValue::Int(state as i32),
                        JValue::Int(eid),
                        JValue::Object(&name),
                        JValue::Int(exp_to_drop),
                    ],
                )
            }
            S1Event::BlockPlace {
                dim,
                pos: (x, y, z),
                replaced,
                placed,
                against,
                player: (eid, name),
            } => {
                let name = env.new_string(name)?;
                env.call_static_method(
                    hooks,
                    "dispatchBlockPlaceEncoded",
                    "(IIIIIIIILjava/lang/String;)Ljava/lang/String;",
                    &[
                        JValue::Int(i32::from(dim)),
                        JValue::Int(x),
                        JValue::Int(y),
                        JValue::Int(z),
                        JValue::Int(replaced as i32),
                        JValue::Int(placed as i32),
                        JValue::Int(against as i32),
                        JValue::Int(eid),
                        JValue::Object(&name),
                    ],
                )
            }
            S1Event::PlayerLogin(eid, name) | S1Event::PlayerLogout(eid, name) => {
                let name = env.new_string(name)?;
                let method = if matches!(event, S1Event::PlayerLogin(..)) {
                    "dispatchPlayerLoginEncoded"
                } else {
                    "dispatchPlayerLogoutEncoded"
                };
                env.call_static_method(
                    hooks,
                    method,
                    "(ILjava/lang/String;)Ljava/lang/String;",
                    &[JValue::Int(eid), JValue::Object(&name)],
                )
            }
            S1Event::ServerChat {
                player: (eid, name),
                raw_text,
            } => {
                let name = env.new_string(name)?;
                let raw_text = env.new_string(raw_text)?;
                env.call_static_method(
                    hooks,
                    "dispatchServerChatEncoded",
                    "(ILjava/lang/String;Ljava/lang/String;)Ljava/lang/String;",
                    &[
                        JValue::Int(eid),
                        JValue::Object(&name),
                        JValue::Object(&raw_text),
                    ],
                )
            }
            S1Event::RegisterCommands => env.call_static_method(
                hooks,
                "dispatchRegisterCommandsEncoded",
                "()Ljava/lang/String;",
                &[],
            ),
        }?;
        let encoded = JString::from(result.l()?);
        let wire = env.get_string(&encoded)?.into();
        Ok(wire)
    });
    let wire = wire.map_err(|error| java_error(env, error))?;
    decode_mutation_batch(&wire)
}

#[cfg(feature = "neoforge-events")]
pub(crate) fn fetch_registry_snapshot(env: &mut JNIEnv) -> Result<ModRegistrySnapshot, String> {
    if OWNER.get() != Some(&thread::current().id()) {
        return Err("S2 registry snapshot requires the JVM owner thread".into());
    }
    let wire = env.with_local_frame(4, |env| -> jni::errors::Result<String> {
        let encoded = env
            .call_static_method(
                "net/mcspine/neoforge/Bootstrap",
                "frozenRegistryEncoded",
                "()Ljava/lang/String;",
                &[],
            )?
            .l()?;
        let encoded = JString::from(encoded);
        let wire = env.get_string(&encoded)?.into();
        Ok(wire)
    });
    ModRegistrySnapshot::from_wire(&wire.map_err(|error| java_error(env, error))?)
}

#[cfg(feature = "neoforge-events")]
struct S3DirectBuffer<'local> {
    java: AutoLocal<'local, JByteBuffer<'local>>,
    bytes: Vec<u8>,
}

#[cfg(feature = "neoforge-events")]
impl<'local> S3DirectBuffer<'local> {
    fn bind(env: &mut JNIEnv<'local>) -> jni::errors::Result<Self> {
        let mut bytes = vec![0; S3_CAPACITY];
        // The private Java entry point borrows these views synchronously. It
        // clears the response view from all callback state before returning.
        // The allocation is never resized, and the Java reference drops first.
        let java = unsafe { env.new_direct_byte_buffer(bytes.as_mut_ptr(), S3_CAPACITY)? };
        Ok(Self {
            java: env.auto_local(java),
            bytes,
        })
    }
}

/// One reusable request/response slot on the attached NeoForge JVM owner.
#[cfg(feature = "neoforge-events")]
#[allow(dead_code)] // The tick owner can use this once it supplies S3 activations.
pub(crate) struct S3BatchBridge<'local> {
    class: AutoLocal<'local, JClass<'local>>,
    dispatch: JStaticMethodID,
    request: S3DirectBuffer<'local>,
    response: S3DirectBuffer<'local>,
    sequence: u64,
    #[cfg(test)]
    jni_crossings: usize,
}

#[cfg(feature = "neoforge-events")]
#[allow(dead_code)]
impl<'local> S3BatchBridge<'local> {
    pub(crate) fn bind(env: &mut JNIEnv<'local>) -> Result<Self, String> {
        if OWNER.get() != Some(&thread::current().id()) {
            return Err("S3 bridge requires the JVM owner thread".into());
        }
        let bind = (|| -> jni::errors::Result<Self> {
            let class = env.find_class("net/mcspine/neoforge/DirectBatchBridge")?;
            let class = env.auto_local(class);
            let dispatch = env.get_static_method_id(
                &*class,
                "dispatch",
                "(Ljava/nio/ByteBuffer;ILjava/nio/ByteBuffer;)I",
            )?;
            Ok(Self {
                class,
                dispatch,
                request: S3DirectBuffer::bind(env)?,
                response: S3DirectBuffer::bind(env)?,
                sequence: 0,
                #[cfg(test)]
                jni_crossings: 0,
            })
        })();
        bind.map_err(|error| java_error(env, error))
    }

    pub(crate) fn dispatch<'wire>(
        &'wire mut self,
        env: &mut JNIEnv<'local>,
        requests: &[S3TickRequest],
    ) -> Result<impl ExactSizeIterator<Item = S3BatchOp<'wire>> + 'wire, String> {
        let sequence = self
            .sequence
            .checked_add(1)
            .ok_or("S3 sequence exhausted")?;
        let request = self.request.bytes.as_mut_ptr();
        // Rust only writes to the request before calling Java. No Rust slice
        // into either buffer lives across the call.
        let request = unsafe { std::slice::from_raw_parts_mut(request, S3_CAPACITY) };
        let len = encode_s3_request_batch(request, sequence, requests)?;
        self.sequence = sequence;
        let len = self.invoke(env, len as jint)?;
        let len = usize::try_from(len)
            .ok()
            .filter(|&len| len <= S3_CAPACITY)
            .ok_or("invalid S3 response size")?;
        // The Java writer has returned. This borrow prevents reuse while
        // decoded operations still reference the response bytes.
        let response = unsafe { std::slice::from_raw_parts(self.response.bytes.as_ptr(), len) };
        decode_s3_response_batch(response, sequence, requests).map_err(str::to_owned)
    }

    fn invoke(&mut self, env: &mut JNIEnv<'local>, len: jint) -> Result<jint, String> {
        let args = [
            JValue::Object(&*self.request.java).as_jni(),
            JValue::Int(len).as_jni(),
            JValue::Object(&*self.response.java).as_jni(),
        ];
        #[cfg(test)]
        {
            self.jni_crossings += 1;
        }
        // bind resolves this exact signature against the retained class. The
        // two direct buffers and the Int argument match that signature.
        let result = unsafe {
            env.call_static_method_unchecked(
                &*self.class,
                self.dispatch,
                ReturnType::Primitive(Primitive::Int),
                &args,
            )
        };
        result
            .and_then(|value| value.i())
            .map_err(|error| java_error(env, error))
    }
}

fn java_error(env: &mut JNIEnv, error: jni::errors::Error) -> String {
    let exception = env.exception_occurred();
    if let Err(clear_error) = env.exception_clear() {
        return format!("{error}; could not clear Java exception: {clear_error}");
    }
    let Ok(exception) = exception else {
        return error.to_string();
    };
    if exception.is_null() {
        return error.to_string();
    }
    let exception = env.auto_local(exception);
    let description = env.with_local_frame(4, |env| -> jni::errors::Result<String> {
        let text = env.call_method(&exception, "toString", "()Ljava/lang/String;", &[])?;
        let text = JString::from(text.l()?);
        let description = env.get_string(&text)?.into();
        Ok(description)
    });
    // Throwable.toString itself can throw. Keep the owner usable in that case.
    if description.is_err() {
        let _ = env.exception_clear();
    }
    description.unwrap_or_else(|_| error.to_string())
}

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
        super::load_world()?;
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
                    #[cfg(feature = "neoforge-events")]
                    fetch_registry_snapshot(&mut env)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "neoforge-events")]
    #[test]
    fn s3_direct_jni_fixture() {
        use crate::landing_types::S3Activation::*;

        // The Python suite builds and supplies the audited mod and Java 21 classpath.
        let Some(directory) = std::env::var_os("MC_SPINE_S3_FIXTURE") else {
            return;
        };
        let directory = Path::new(&directory);
        let classpath = std::fs::read_to_string(directory.join("classpath.txt")).unwrap();
        let jvm = create_jvm(&classpath).unwrap();
        let mut env = jvm.attach_current_thread().unwrap();
        let path = JObject::from(env.new_string(directory.to_str().unwrap()).unwrap());
        let host = env.call_static_method(
            "net/mcspine/neoforge/Bootstrap",
            "initialize",
            "(Ljava/lang/String;)Ljava/lang/Class;",
            &[JValue::Object(&path)],
        );
        let host = host.unwrap_or_else(|error| panic!("{}", java_error(&mut env, error)));
        register_host(&mut env, host.l().unwrap()).unwrap();
        let mut bridge = S3BatchBridge::bind(&mut env).unwrap();

        let request = |activation, kind, x| S3TickRequest {
            activation,
            dim: (x % 3) as u8,
            kind,
            id: x - 512,
            generation: 0x8000_0000 + x as u32,
            pos: (x, 64 + x, -x),
            state: 0xfedc_0000 + x as u32,
        };
        let require_rollback = |bridge: &S3BatchBridge<'_>| {
            // The Java entry point revokes the previous frame before it writes any operations.
            let header = unsafe { std::slice::from_raw_parts(bridge.response.bytes.as_ptr(), 4) };
            assert_eq!(header, [0; 4]);
        };
        let mode = std::env::var("MC_SPINE_S3_MODE").unwrap();
        match mode.as_str() {
            "stress" => {
                let blocks: Vec<_> = (0..1024).map(|i| request(BlockEntityTick, 7, i)).collect();
                let ops = bridge.dispatch(&mut env, &blocks).unwrap();
                assert_eq!(ops.len(), 1024);
                assert!(ops.eq(blocks.iter().map(|r| S3BatchOp::SetBlockIf {
                    dim: r.dim,
                    pos: r.pos,
                    new_state: r.state + 1,
                    expected_state: r.state,
                })));
                assert_eq!(bridge.jni_crossings, 1);
                println!("validated=1024 jni_crossings={}", bridge.jni_crossings);

                // Reuse the same direct views for a batch with every activation and opcode.
                let requests: Vec<_> = (0..1024)
                    .map(|i| {
                        request(
                            [BlockEntityTick, ScheduledBlockTick, Container, EntityTick][i % 4],
                            7,
                            i as i32,
                        )
                    })
                    .collect();
                let ops = bridge.dispatch(&mut env, &requests).unwrap();
                assert_eq!(ops.len(), 1024);
                for (r, actual) in requests.iter().zip(ops) {
                    let x = r.pos.0;
                    let message = format!("更新 📦 {x}");
                    let expected = match r.activation {
                        BlockEntityTick => S3BatchOp::SetBlockIf {
                            dim: r.dim,
                            pos: r.pos,
                            new_state: r.state + 1,
                            expected_state: r.state,
                        },
                        ScheduledBlockTick => S3BatchOp::ScheduleTick {
                            dim: r.dim,
                            pos: r.pos,
                            delay: x * 4,
                            priority: (x % 7 - 3) as i8,
                        },
                        Container => S3BatchOp::ContainerSlotDelta {
                            dim: r.dim,
                            pos: r.pos,
                            slot: x as u16,
                            item_id: 1400 + x as u32,
                            count: (x % 99 + 1) as u8,
                        },
                        EntityTick => S3BatchOp::SendSystemMessage {
                            player_eid: r.id,
                            message: &message,
                        },
                    };
                    assert_eq!(actual, expected);
                }
                assert_eq!(bridge.jni_crossings, 2);
            }
            "lifetime" => {
                for activation in [BlockEntityTick, EntityTick] {
                    let keep = request(activation, 11, 0);
                    let access = S3TickRequest {
                        generation: 1,
                        pos: (1, 64, 0),
                        ..keep
                    };
                    // Writing in the first activation succeeds, then use of the earlier
                    // handle from the next generation invalidates all of those writes.
                    let error = bridge.dispatch(&mut env, &[keep, access]).err().unwrap();
                    assert!(
                        error.contains("IllegalStateException: S3 batch failed"),
                        "{error}"
                    );
                    require_rollback(&bridge);
                    // The prior-generation handle must stay expired when a new, exact
                    // copy of the original tuple is active in a different batch.
                    assert_eq!(bridge.dispatch(&mut env, &[keep]).unwrap().len(), 1);
                    let same_identity = S3TickRequest {
                        generation: keep.generation,
                        ..access
                    };
                    let error = bridge.dispatch(&mut env, &[same_identity]).err().unwrap();
                    assert!(
                        error.contains("IllegalStateException: S3 batch failed"),
                        "{error}"
                    );
                    require_rollback(&bridge);
                }
                println!("expired entity and block entity handles rolled back");
            }
            "reentrancy" => {
                let at_depth_four = request(BlockEntityTick, 12, 4);
                let at_depth_five = request(BlockEntityTick, 12, 5);
                let ops = bridge.dispatch(&mut env, &[at_depth_four]).unwrap();
                assert!(ops.eq((1..=4).map(|delay| S3BatchOp::ScheduleTick {
                    dim: at_depth_four.dim,
                    pos: at_depth_four.pos,
                    delay,
                    priority: 0,
                })));
                let error = bridge
                    .dispatch(&mut env, &[at_depth_four, at_depth_five])
                    .err()
                    .unwrap();
                assert!(
                    error.contains("IllegalStateException: S3 batch failed"),
                    "{error}"
                );
                require_rollback(&bridge);
                println!("depth 4 committed and depth 5 rolled back");
            }
            "bounds" => {
                for (kind, x, error_text) in [
                    (13, 65, "oversized S3 UTF-8 message"),
                    (14, 4097, "response capacity exceeded"),
                    (15, 0, "Invalid S3 tick delay or priority"),
                    (16, 0, "Invalid S3 container slot or stack"),
                    (17, 0, "Invalid or oversized S3 UTF-8 message"),
                    (
                        18,
                        0,
                        "Legacy native world access is unavailable during S3 dispatch",
                    ),
                ] {
                    let valid = request(BlockEntityTick, 7, 3);
                    assert_eq!(bridge.dispatch(&mut env, &[valid]).unwrap().len(), 1);
                    let error = bridge
                        .dispatch(&mut env, &[valid, request(BlockEntityTick, kind, x)])
                        .err()
                        .unwrap();
                    assert!(error.contains(error_text), "{error}");
                    require_rollback(&bridge);
                }

                let requests = [
                    request(BlockEntityTick, 20, 0),
                    request(BlockEntityTick, 20, 1),
                ];
                let pointer = bridge.request.bytes.as_mut_ptr();
                let encode = |out: &mut [u8]| encode_s3_request_batch(out, 9, &requests).unwrap();
                let len = encode(unsafe { std::slice::from_raw_parts_mut(pointer, S3_CAPACITY) });
                for (len, error_text) in [
                    (len - 1, "count or size"),
                    (len + 1, "count or size"),
                    (S3_CAPACITY + 1, "buffer or length"),
                ] {
                    let error = bridge.invoke(&mut env, len as jint).unwrap_err();
                    assert!(error.contains(error_text), "{error}");
                    require_rollback(&bridge);
                }
                for (offset, bytes, error_text) in [
                    (2, &[2][..], "request header"),
                    (44, &[0xff][..], "activation identity"),
                    (45, &[3][..], "activation identity"),
                    (52, &[0; 4][..], "activation identity"),
                ] {
                    let out = unsafe { std::slice::from_raw_parts_mut(pointer, S3_CAPACITY) };
                    encode(out);
                    out[offset..offset + bytes.len()].copy_from_slice(bytes);
                    let error = bridge.invoke(&mut env, len as jint).unwrap_err();
                    assert!(error.contains(error_text), "{error}");
                    require_rollback(&bridge);
                }
                println!("malformed input and invalid or oversized output rejected");
            }
            _ => panic!("unknown S3 fixture mode {mode}"),
        }
        assert_eq!(bridge.dispatch(&mut env, &[]).unwrap().len(), 0);
        assert_eq!(
            bridge
                .dispatch(&mut env, &[request(BlockEntityTick, 7, 3)])
                .unwrap()
                .len(),
            1
        );
        env.call_static_method("net/mcspine/neoforge/Bootstrap", "close", "()V", &[])
            .unwrap();
        let error = bridge.dispatch(&mut env, &[]).err().unwrap();
        assert!(
            error.contains("S3 dispatch requires the bootstrap owner"),
            "{error}"
        );
        require_rollback(&bridge);
    }

    #[test]
    fn s1_commit_rolls_back_cancellation_and_rechecks_each_cas() {
        let _world = super::super::world_guard();
        let (x, y, z) = (3203, 88, -3207);
        let end = super::super::DimId::End;
        let nether = super::super::DimId::Nether;
        let state = |dim| super::super::block_at_dim(dim, x, y, z);
        let end_before = state(end);
        let nether_before = state(nether);
        let first = if end_before == 1 { 2 } else { 1 };
        let final_state = 10;
        let nether_state = if nether_before == 1 { 2 } else { 1 };
        let block = |dim, new_state, expected_state| MutationOp::SetBlockIf {
            dim,
            pos: (x, y, z),
            new_state,
            expected_state,
        };
        let batch = |canceled| S1EventOutcome {
            canceled,
            break_exp: None,
            chat_message: None,
            mutations: vec![
                block(2, first, Some(end_before)),
                block(2, 11, Some(end_before)), // stale after the first write
                block(2, final_state, Some(first)),
                block(1, nether_state, None),
                MutationOp::SendSystemMessage {
                    player_eid: 42,
                    message: "updated".into(),
                },
                MutationOp::RegisterCommand {
                    literal: "greet".into(),
                },
            ],
        };

        assert_eq!(
            apply_s1_outcome(batch(true)),
            Ok(BatchCommitReport::default())
        );
        assert_eq!((state(end), state(nether)), (end_before, nether_before));

        let mut invalid = batch(false);
        invalid.mutations.push(block(3, 1, None));
        assert!(apply_s1_outcome(invalid).is_err());
        assert_eq!((state(end), state(nether)), (end_before, nether_before));

        assert_eq!(
            apply_s1_outcome(batch(false)),
            Ok(BatchCommitReport {
                applied_blocks: 3,
                skipped_cas_blocks: 1,
                system_messages: vec![(42, "updated".into())],
                registered_commands: vec!["greet".into()],
            })
        );
        assert_eq!((state(end), state(nether)), (final_state, nether_state));
        assert!(super::super::set_block_if_dim(
            end,
            x,
            y,
            z,
            end_before,
            Some(final_state)
        ));
        assert!(super::super::set_block_if_dim(
            nether,
            x,
            y,
            z,
            nether_before,
            Some(nether_state)
        ));
    }
}
