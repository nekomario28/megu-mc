//! One JVM owner and guarded Rust world ABI for pinned Fabric and NeoForge operations.
use crate::landing_types::{
    decode_mutation_batch, Loader, MutationOp, Request, S1EventOutcome, Trample,
    FIRST_MOD_BLOCK_STATE_ID,
};
#[cfg(feature = "neoforge-events")]
use crate::landing_types::{
    decode_s3_response_batch, encode_s3_request_batch, S3BatchOp, S3TickRequest, S3_CAPACITY,
};
#[cfg(feature = "neoforge-events")]
use crate::landing_types::{
    decode_s4_payload_batch, encode_s4_payload_batch, CustomPayloadEnvelope, ModRegistrySnapshot,
    S4ContainerOp, S4ContainerOutcome, S4ContainerSnapshot, S4_MAX_CHANNEL, S4_MAX_PAYLOAD,
    S4_PAYLOAD_CAPACITY,
};
#[cfg(feature = "neoforge-events")]
use jni::{
    objects::{AutoLocal, JByteArray, JByteBuffer, JIntArray, JStaticMethodID},
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

#[cfg(all(unix, feature = "neoforge-events"))]
use std::os::unix::fs::DirBuilderExt;

static TRAMPLE: OnceLock<bool> = OnceLock::new();
static OWNER: OnceLock<ThreadId> = OnceLock::new();
static SERVER: OnceLock<bool> = OnceLock::new();
static COMMITTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static ACTIVE: std::sync::Mutex<Option<Trample>> = std::sync::Mutex::new(None);

const S1_MAX_FRAME_BYTES: usize = 256 * 1024;
const S1_MAX_MUTATIONS: usize = 4096;
const S1_MAX_MESSAGE_BYTES: usize = 4096;

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct BatchCommitReport {
    pub(crate) applied_blocks: usize,
    pub(crate) skipped_cas_blocks: usize,
    pub(crate) system_messages: Vec<(i32, String)>,
    pub(crate) registered_commands: Vec<String>,
}

fn validate_s1_state(state: u32) -> Result<(), String> {
    if state < FIRST_MOD_BLOCK_STATE_ID {
        if super::breg().is_some_and(|registry| (state as usize) < registry.state_count()) {
            return Ok(());
        }
    } else {
        #[cfg(feature = "neoforge-events")]
        if NEOFORGE.get().is_some_and(|runtime| {
            runtime.registry.entries().iter().any(|entry| {
                entry.registry == crate::landing_types::ModRegistry::Block
                    && entry.runtime_id == state
            })
        }) {
            return Ok(());
        }
    }
    Err(format!("invalid S1 block state {state}"))
}

fn validate_s1_outcome(outcome: &S1EventOutcome) -> Result<(), String> {
    if outcome.mutations.len() > S1_MAX_MUTATIONS {
        return Err("S1 mutation count exceeds limit".into());
    }
    if outcome.canceled
        && (!outcome.mutations.is_empty()
            || outcome.break_exp.is_some()
            || outcome.chat_message.is_some())
    {
        return Err("canceled S1 event has staged changes".into());
    }
    let mut bytes = outcome.chat_message.as_ref().map_or(0, String::len);
    if bytes > S1_MAX_MESSAGE_BYTES {
        return Err("S1 chat message exceeds limit".into());
    }
    for mutation in &outcome.mutations {
        match mutation {
            MutationOp::SetBlockIf {
                dim,
                pos,
                new_state,
                expected_state,
            } => {
                let dim = super::DimId::ALL
                    .get(usize::from(*dim))
                    .ok_or_else(|| format!("invalid S1 dimension slot {dim}"))?;
                super::check_modification_bounds(*dim, *pos)?;
                validate_s1_state(*new_state)?;
                if let Some(expected) = expected_state {
                    validate_s1_state(*expected)?;
                }
            }
            MutationOp::SendSystemMessage { message, .. } => {
                if message.len() > S1_MAX_MESSAGE_BYTES {
                    return Err("S1 system message exceeds limit".into());
                }
                bytes = bytes.saturating_add(message.len());
            }
            MutationOp::RegisterCommand { literal } => {
                if literal.len() > 32767 {
                    return Err("S1 command literal exceeds limit".into());
                }
                bytes = bytes.saturating_add(literal.len());
            }
        }
        if bytes > S1_MAX_FRAME_BYTES {
            return Err("S1 staged payload exceeds limit".into());
        }
    }
    Ok(())
}

/// Validate the entire ordered CAS sequence before committing any world edit.
/// The save guard encloses both phases, including cross-dimension writes.
/// The caller delivers side effects only after a successful commit.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn apply_s1_outcome(outcome: S1EventOutcome) -> Result<BatchCommitReport, String> {
    validate_s1_outcome(&outcome)?;
    if outcome.canceled {
        return Ok(BatchCommitReport::default());
    }
    super::with_save_guard(|| {
        let mut staged = std::collections::HashMap::new();
        for mutation in &outcome.mutations {
            if let MutationOp::SetBlockIf {
                dim,
                pos: (x, y, z),
                new_state,
                expected_state,
            } = mutation
            {
                let dimension = super::DimId::ALL[usize::from(*dim)];
                let position = (dimension, *x, *y, *z);
                let current = *staged
                    .entry(position)
                    .or_insert_with(|| super::block_at_dim(dimension, *x, *y, *z));
                if expected_state.is_some_and(|expected| expected != current) {
                    return Err(format!(
                        "S1 CAS precondition failed at {dimension:?} ({x}, {y}, {z})"
                    ));
                }
                staged.insert(position, *new_state);
            }
        }
        let mut report = BatchCommitReport::default();
        for mutation in outcome.mutations {
            match mutation {
                MutationOp::SetBlockIf {
                    dim,
                    pos: (x, y, z),
                    new_state,
                    expected_state: _,
                } => {
                    let dim = super::DimId::ALL[usize::from(dim)];
                    // Every ordered CAS has already passed preflight under the
                    // save guard. Block callbacks may edit a later CAS target,
                    // so rechecking mid-commit could leave a partial batch.
                    // All validated positions are writable with no second CAS.
                    super::set_block_if_dim(dim, x, y, z, new_state, None);
                    super::note_block_update_dim(dim, x, y, z, super::block_at_dim(dim, x, y, z));
                    report.applied_blocks += 1;
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
        Ok(report)
    })
}

#[cfg_attr(not(feature = "neoforge-events"), allow(dead_code))]
pub(crate) enum S1Event<T> {
    BlockBreak {
        dim: u8,
        pos: (i32, i32, i32),
        state: u32,
        player: (i32, T),
        exp_to_drop: i32,
    },
    BlockPlace {
        dim: u8,
        pos: (i32, i32, i32),
        replaced: u32,
        placed: u32,
        against: u32,
        player: (i32, T),
    },
    PlayerLogin(i32, T),
    PlayerLogout(i32, T),
    ServerChat {
        player: (i32, T),
        raw_text: T,
    },
    Command {
        dim: u8,
        pos: (i32, i32, i32),
        name: T,
        literal: T,
    },
    RegisterCommands,
}

/// Dispatch a NeoForge S1 event on the JVM owner. This is staging only. A
/// Java exception or an invalid wire batch returns no outcome to commit.
#[cfg_attr(not(feature = "neoforge-events"), allow(dead_code))]
pub(crate) fn dispatch_s1_event(
    env: &mut JNIEnv,
    hooks: &JClass,
    event: S1Event<impl AsRef<str>>,
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
                let name = env.new_string(name.as_ref())?;
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
                let name = env.new_string(name.as_ref())?;
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
            S1Event::PlayerLogin(eid, ref name) | S1Event::PlayerLogout(eid, ref name) => {
                let name = env.new_string(name.as_ref())?;
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
                let name = env.new_string(name.as_ref())?;
                let raw_text = env.new_string(raw_text.as_ref())?;
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
            S1Event::Command {
                dim,
                pos: (x, y, z),
                name,
                literal,
            } => {
                let name = env.new_string(name.as_ref())?;
                let literal = env.new_string(literal.as_ref())?;
                env.call_static_method(
                    hooks,
                    "dispatchCommandEncoded",
                    "(IIIILjava/lang/String;Ljava/lang/String;)Ljava/lang/String;",
                    &[
                        JValue::Int(i32::from(dim)),
                        JValue::Int(x),
                        JValue::Int(y),
                        JValue::Int(z),
                        JValue::Object(&name),
                        JValue::Object(&literal),
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
    if wire.len() > S1_MAX_FRAME_BYTES {
        return Err("S1 wire frame exceeds limit".into());
    }
    let outcome = decode_mutation_batch(&wire)?;
    validate_s1_outcome(&outcome)?;
    Ok(outcome)
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

#[cfg(feature = "neoforge-events")]
fn require_s4_owner() -> Result<(), String> {
    if OWNER.get() != Some(&thread::current().id()) {
        return Err("S4 requires the JVM owner thread".into());
    }
    Ok(())
}

/// Dispatch one C2S payload and return its entire S2C batch in one Java invocation.
#[cfg(feature = "neoforge-events")]
pub(crate) fn dispatch_s4_payload(
    env: &mut JNIEnv,
    player_eid: i32,
    player_name: &str,
    inbound: &CustomPayloadEnvelope,
) -> Result<Vec<CustomPayloadEnvelope>, String> {
    if inbound.target_eid.is_some() {
        return Err("S4 C2S payload has a target".into());
    }
    let frame = encode_s4_payload_batch(std::slice::from_ref(inbound))?;
    dispatch_s4_payload_frame(env, player_eid, player_name, &frame)
}

#[cfg(feature = "neoforge-events")]
fn dispatch_s4_payload_frame(
    env: &mut JNIEnv,
    player_eid: i32,
    player_name: &str,
    frame: &[u8],
) -> Result<Vec<CustomPayloadEnvelope>, String> {
    require_s4_owner()?;
    validate_s4_inbound_frame(frame)?;
    let result = env
        .with_local_frame(8, |env| -> jni::errors::Result<_> {
            let input = env.byte_array_from_slice(frame)?;
            let name = env.new_string(player_name)?;
            let out = env
                .call_static_method(
                    "net/mcspine/neoforge/S4NetworkBridge",
                    "dispatch",
                    "([BILjava/lang/String;)[B",
                    &[
                        JValue::Object(&input),
                        JValue::Int(player_eid),
                        JValue::Object(&name),
                    ],
                )?
                .l()?;
            let out = JByteArray::from(out);
            if env.get_array_length(&out)? as usize > S4_PAYLOAD_CAPACITY {
                return Ok(None);
            }
            Ok(Some(env.convert_byte_array(&out)?))
        })
        .map_err(|error| java_error(env, error))?;
    let wire = result.ok_or("oversized S4 outbound frame")?;
    decode_s4_payload_batch(&wire).map_err(str::to_owned)
}

#[cfg(feature = "neoforge-events")]
fn validate_s4_inbound_frame(frame: &[u8]) -> Result<(), String> {
    if frame.len() > 13 + S4_MAX_CHANNEL + S4_MAX_PAYLOAD {
        return Err("oversized S4 inbound frame".into());
    }
    let inbound = decode_s4_payload_batch(frame).map_err(|error| match error {
        // Keep the existing host diagnostic when a frame advertises a
        // payload beyond the S4 wire limit.
        "oversized S4 payload" => "oversized S4 payload frame".to_owned(),
        _ => error.to_owned(),
    })?;
    if inbound.len() != 1 || inbound[0].target_eid.is_some() {
        return Err("S4 C2S must contain exactly one targetless payload".into());
    }
    Ok(())
}

#[cfg(feature = "neoforge-events")]
#[allow(dead_code)] // Available to the host owner.
pub(crate) fn query_s4_block_items(
    env: &mut JNIEnv,
    snapshot: &S4ContainerSnapshot,
    ops: &[S4ContainerOp],
) -> Result<Option<S4ContainerOutcome>, String> {
    require_s4_owner()?;
    let input = snapshot.encode_query(ops)?;
    let expected = snapshot.response_len(ops);
    let result = env
        .with_local_frame(8, |env| -> jni::errors::Result<_> {
            let frame = env.new_int_array(input.len() as i32)?;
            env.set_int_array_region(&frame, 0, &input)?;
            let out = env
                .call_static_method(
                    "net/mcspine/neoforge/S4CapabilityBridge",
                    "query",
                    "([I)[I",
                    &[JValue::Object(&frame)],
                )?
                .l()?;
            let out = JIntArray::from(out);
            let len = env.get_array_length(&out)? as usize;
            if len == 0 {
                return Ok(Some(Vec::new()));
            }
            if len != expected {
                return Ok(None);
            }
            let mut wire = vec![0; len];
            env.get_int_array_region(&out, 0, &mut wire)?;
            Ok(Some(wire))
        })
        .map_err(|error| java_error(env, error))?;
    let wire = result.ok_or("invalid S4 capability response size")?;
    snapshot.decode_response(ops, &wire).map_err(str::to_owned)
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
        let active = *ACTIVE.lock().unwrap_or_else(|e| e.into_inner());
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
        let active = *ACTIVE.lock().unwrap_or_else(|e| e.into_inner());
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

#[cfg_attr(not(feature = "fabric-probe"), allow(dead_code))]
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
        #[cfg(feature = "neoforge-events")]
        let staged = match loader {
            Loader::NeoForge => StagedMods::from_env()?,
            Loader::Fabric => None,
        };
        let setup = (|| -> Result<jni::objects::GlobalRef, Box<dyn std::error::Error>> {
            let bootstrap = match loader {
                Loader::Fabric => {
                    initialize_fabric(&mut env, &classpath, true)?;
                    "net/mcspine/probe/Bootstrap"
                }
                Loader::NeoForge => {
                    let version = env
                        .call_static_method(
                            "java/lang/Runtime",
                            "version",
                            "()Ljava/lang/Runtime$Version;",
                            &[],
                        )?
                        .l()?;
                    if env.call_method(&version, "feature", "()I", &[])?.i()? != 21 {
                        return Err("NeoForge requires Java 21".into());
                    }
                    let directory = std::env::current_dir()?;
                    #[cfg(feature = "neoforge-events")]
                    let directory = staged.as_ref().map_or(directory, |staged| staged.0.clone());
                    let directory = JObject::from(
                        env.new_string(
                            directory
                                .to_str()
                                .ok_or("Invalid NeoForge server directory")?,
                        )?,
                    );
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
        #[cfg(feature = "neoforge-events")]
        if matches!(loader, Loader::NeoForge) {
            return serve_neoforge(
                &mut env,
                <&JClass>::from(hooks.as_obj()),
                receive,
                &ready,
                staged,
            );
        }
        ready.send(Ok(())).map_err(|_| "Server startup abandoned")?;
        for request in receive {
            if !serve_trample(&mut env, <&JClass>::from(hooks.as_obj()), request) {
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

fn serve_trample(env: &mut JNIEnv, hooks: &JClass, request: Request) -> bool {
    let input = request.trample;
    COMMITTED.store(false, super::Ordering::Relaxed);
    *ACTIVE.lock().unwrap_or_else(|e| e.into_inner()) = Some(input);
    let (x, y, z) = input.position;
    let outcome = env.with_local_frame(16, |env| {
        env.call_static_method(
            hooks,
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
    *ACTIVE.lock().unwrap_or_else(|e| e.into_inner()) = None;
    let result = outcome
        .map(|()| COMMITTED.load(super::Ordering::Relaxed))
        .map_err(|error| java_error(env, error));
    let succeeded = result.is_ok();
    let _ = request.reply.send(result);
    succeeded
}

#[cfg(feature = "neoforge-events")]
struct StagedMods(std::path::PathBuf);

#[cfg(feature = "neoforge-events")]
impl StagedMods {
    fn from_env() -> Result<Option<Self>, Box<dyn std::error::Error>> {
        let Some(paths) = std::env::var_os("MC_SPINE_NEOFORGE_MODS") else {
            return Ok(None);
        };
        let directory = std::env::temp_dir().join(format!("mc-spine-mods-{}", std::process::id()));
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        builder.mode(0o700);
        builder.create(&directory)?;
        let staged = Self(directory);
        let mods = staged.0.join("mods");
        std::fs::create_dir(&mods)?;
        if !paths.is_empty() {
            for path in std::env::split_paths(&paths) {
                let name = path.file_name().ok_or("Missing NeoForge mod JAR name")?;
                if path.extension().and_then(|ext| ext.to_str()) != Some("jar") || !path.is_file() {
                    return Err(format!("Invalid NeoForge mod JAR: {}", path.display()).into());
                }
                let mut source = std::fs::File::open(&path)?;
                let mut destination = std::fs::File::create_new(mods.join(name))?;
                std::io::copy(&mut source, &mut destination)?;
            }
        }
        Ok(Some(staged))
    }
}

#[cfg(feature = "neoforge-events")]
impl Drop for StagedMods {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_dir_all(&self.0) {
            eprintln!("[mod-compatibility] Could not remove staged NeoForge JARs: {error}");
        }
    }
}

#[cfg(feature = "neoforge-events")]
type NeoForgeJob = Box<
    dyn for<'local> FnOnce(&mut JNIEnv<'local>, &JClass, &mut S3BatchBridge<'local>) -> bool + Send,
>;

#[cfg(feature = "neoforge-events")]
pub(crate) struct NeoForgeRuntime {
    pub(crate) registry: ModRegistrySnapshot,
    pub(crate) commands: Vec<String>,
    jobs: std::sync::mpsc::SyncSender<NeoForgeJob>,
    staged: std::sync::Mutex<Option<StagedMods>>,
}

#[cfg(feature = "neoforge-events")]
pub(crate) static NEOFORGE: OnceLock<NeoForgeRuntime> = OnceLock::new();

#[cfg(feature = "neoforge-events")]
impl NeoForgeRuntime {
    // Callers wait without holding world or socket locks. Java only stages
    // outcomes here, so normal Rust commits and packet writes stay on callers.
    fn call<T: Send + 'static>(
        &self,
        dispatch: impl for<'local> FnOnce(
                &mut JNIEnv<'local>,
                &JClass,
                &mut S3BatchBridge<'local>,
            ) -> Result<T, String>
            + Send
            + 'static,
    ) -> Result<T, String> {
        let (send, receive) = std::sync::mpsc::channel();
        self.jobs
            .send(Box::new(move |env, hooks, batch| {
                let _ = send.send(dispatch(env, hooks, batch));
                true
            }))
            .map_err(|_| "NeoForge JVM owner stopped")?;
        receive.recv().map_err(|_| "NeoForge callback terminated")?
    }

    pub(crate) fn cleanup_staged_mods(&self) {
        self.staged.lock().unwrap_or_else(|e| e.into_inner()).take();
    }

    pub(crate) fn event(&self, event: S1Event<String>) -> Result<S1EventOutcome, String> {
        let outcome = self.call(move |env, hooks, _| dispatch_s1_event(env, hooks, event))?;
        validate_mod_block_states(&self.registry, &outcome)?;
        Ok(outcome)
    }

    /// Live player activations support guarded block writes and system messages.
    /// Scheduling and inventory proposals require a block owner and lifetime.
    pub(crate) fn player_ticks(
        &self,
        requests: Vec<S3TickRequest>,
    ) -> Result<S1EventOutcome, String> {
        let mutations = self.call(move |env, _, batch| {
            batch
                .dispatch(env, &requests)?
                .map(|op| match op {
                    S3BatchOp::SetBlockIf {
                        dim,
                        pos,
                        new_state,
                        expected_state,
                    } => Ok(MutationOp::SetBlockIf {
                        dim,
                        pos,
                        new_state,
                        expected_state: Some(expected_state),
                    }),
                    S3BatchOp::SendSystemMessage {
                        player_eid,
                        message,
                    } => Ok(MutationOp::SendSystemMessage {
                        player_eid,
                        message: message.into(),
                    }),
                    S3BatchOp::ScheduleTick { .. } | S3BatchOp::ContainerSlotDelta { .. } => Err(
                        "S3 player tick cannot schedule a block tick or write a container".into(),
                    ),
                })
                .collect::<Result<Vec<_>, String>>()
        })?;
        let outcome = S1EventOutcome {
            canceled: false,
            break_exp: None,
            chat_message: None,
            mutations,
        };
        validate_mod_block_states(&self.registry, &outcome)?;
        Ok(outcome)
    }

    pub(crate) fn payload(
        &self,
        eid: i32,
        name: String,
        payload: CustomPayloadEnvelope,
    ) -> Result<Vec<CustomPayloadEnvelope>, String> {
        self.call(move |env, _, _| dispatch_s4_payload(env, eid, &name, &payload))
    }
}

#[cfg(feature = "neoforge-events")]
fn validate_mod_block_states(
    registry: &ModRegistrySnapshot,
    outcome: &S1EventOutcome,
) -> Result<(), String> {
    use crate::landing_types::{ModRegistry, FIRST_MOD_BLOCK_STATE_ID};
    for mutation in &outcome.mutations {
        if let MutationOp::SetBlockIf { new_state, .. } = mutation {
            if *new_state >= FIRST_MOD_BLOCK_STATE_ID
                && !registry.entries().iter().any(|entry| {
                    entry.registry == ModRegistry::Block && entry.runtime_id == *new_state
                })
            {
                return Err(format!("Unknown NeoForge block state {new_state}"));
            }
        }
    }
    Ok(())
}

#[cfg(feature = "neoforge-events")]
fn serve_neoforge(
    env: &mut JNIEnv,
    hooks: &JClass,
    trample: std::sync::mpsc::Receiver<Request>,
    ready: &std::sync::mpsc::Sender<Result<(), String>>,
    staged: Option<StagedMods>,
) -> Result<(), Box<dyn std::error::Error>> {
    let registry = fetch_registry_snapshot(env)?;
    let outcome = dispatch_s1_event(env, hooks, S1Event::<&str>::RegisterCommands)?;
    validate_mod_block_states(&registry, &outcome)?;
    let mut commands = apply_s1_outcome(outcome)?.registered_commands;
    if commands.iter().any(|literal| {
        literal.is_empty()
            || literal.starts_with('/')
            || literal
                .chars()
                .any(|ch| ch.is_whitespace() || ch.is_control())
            || literal.len() > 32767
    }) {
        return Err("Invalid S1 command literal".into());
    }
    super::exclude_native_neoforge_literals(&mut commands);
    commands.sort_unstable();
    commands.dedup();
    let mut batch = S3BatchBridge::bind(env)?;
    let (send, receive) = std::sync::mpsc::sync_channel::<NeoForgeJob>(64);

    // landing::initialize owns the legacy typed channel. Its relay joins the
    // same queue as Play callbacks so only this thread enters JNI.
    let trample_jobs = send.clone();
    thread::Builder::new()
        .name("neoforge-landing-relay".into())
        .spawn(move || {
            for request in trample {
                if trample_jobs
                    .send(Box::new(move |env, hooks, _| {
                        serve_trample(env, hooks, request)
                    }))
                    .is_err()
                {
                    break;
                }
            }
        })?;
    NEOFORGE
        .set(NeoForgeRuntime {
            registry,
            commands,
            jobs: send,
            staged: std::sync::Mutex::new(staged),
        })
        .map_err(|_| "NeoForge runtime already initialized")?;
    ready.send(Ok(())).map_err(|_| "Server startup abandoned")?;
    for job in receive {
        if !job(env, hooks, &mut batch) {
            return Err("Java callback failed; owner stopped".into());
        }
    }
    Ok(())
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
    fn s4_jni_fixture() {
        use crate::landing_types::build_config_registry_sync_packets;
        use crate::landing_types::{S4BlockSide, S4ItemStack, S4SlotMutation, S4SlotResult};

        let Some(directory) = std::env::var_os("MC_SPINE_S4_FIXTURE") else {
            return;
        };
        let directory = Path::new(&directory);
        let classpath = std::fs::read_to_string(directory.join("classpath.txt")).unwrap();
        let jvm = create_jvm(&classpath).unwrap();
        let mut env = jvm.attach_current_thread().unwrap();
        let path = env.new_string(directory.to_str().unwrap()).unwrap();
        let initialized = env.call_static_method(
            "net/mcspine/neoforge/Bootstrap",
            "initialize",
            "(Ljava/lang/String;)Ljava/lang/Class;",
            &[JValue::Object(&path)],
        );
        initialized.unwrap_or_else(|error| panic!("{}", java_error(&mut env, error)));

        let envelope = |channel: &str, bytes: Vec<u8>| CustomPayloadEnvelope {
            channel: format!("nf5_fixture:{channel}"),
            target_eid: None,
            bytes,
        };
        let dispatch = |env: &mut JNIEnv, channel, bytes| {
            dispatch_s4_payload(env, -1025, "Rust owner", &envelope(channel, bytes))
        };
        let expected = [
            CustomPayloadEnvelope {
                target_eid: Some(-1025),
                ..envelope("echo", vec![42, 17, 0])
            },
            envelope("out", vec![43, 17, 0]),
        ];
        assert_eq!(dispatch(&mut env, "in", vec![0, 17, 0]).unwrap(), expected);

        // An exactly 32 KiB packet can traverse both directions. The final
        // field has a three-byte VarInt prefix for its 32763-byte content.
        let mut max = vec![5, 0, 0xfb, 0xff, 0x01];
        max.resize(S4_MAX_PAYLOAD, 0x9b);
        assert_eq!(
            dispatch(&mut env, "echo", max.clone()).unwrap(),
            [CustomPayloadEnvelope {
                target_eid: Some(-1025),
                ..envelope("out", max)
            }]
        );
        println!("S4 targeted and broadcast payloads round-tripped");

        assert_eq!(dispatch(&mut env, "echo", vec![1, 4, 0]).unwrap().len(), 4);
        for (channel, bytes, reason) in [
            ("unknown", vec![0, 17, 0], "Unregistered S4 C2S"),
            ("out", vec![0, 17, 0], "Unregistered S4 C2S"),
            ("echo", vec![0, 17], "Truncated S4 payload"),
            ("echo", vec![0, 17, 0, 99], "Trailing S4 payload"),
            ("echo", vec![0; S4_MAX_PAYLOAD + 1], "oversized S4 payload"),
            ("echo", vec![1, 5, 0], "S4 payload dispatch failed"),
            ("echo", vec![2, 0, 0], "S4 payload dispatch failed"),
            ("echo", vec![3, 0, 0], "Oversized S4 payload"),
            ("echo", vec![4, 0, 0], "batch record limit"),
            ("echo", vec![6, 0, 0], "Unregistered S4 S2C"),
            ("echo", vec![7, 0, 0], "batch capacity"),
        ] {
            let error = dispatch(&mut env, channel, bytes).unwrap_err();
            assert!(
                error.contains(reason),
                "{channel} expected {reason:?}: {error}"
            );
            assert_eq!(
                dispatch(&mut env, "echo", vec![0, 17, 0]).unwrap(),
                expected
            );
        }

        let frame = encode_s4_payload_batch(&[envelope("echo", vec![0, 17, 0])]).unwrap();
        for len in 0..frame.len() {
            assert!(dispatch_s4_payload_frame(&mut env, 1, "Rust owner", &frame[..len]).is_err());
        }
        let mut oversized =
            encode_s4_payload_batch(&[envelope("echo", vec![0; S4_MAX_PAYLOAD])]).unwrap();
        let header = oversized.len() - S4_MAX_PAYLOAD;
        oversized[header - 4..header].copy_from_slice(&((S4_MAX_PAYLOAD + 1) as u32).to_le_bytes());
        oversized.push(0);
        assert!(
            dispatch_s4_payload_frame(&mut env, 1, "Rust owner", &oversized)
                .unwrap_err()
                .contains("oversized S4 payload frame")
        );
        println!("S4 invalid, oversized and depth-five payloads rejected");

        let item = |count| S4ItemStack {
            item_id: 1333,
            count,
        };
        let vanilla = S4ItemStack {
            item_id: 1,
            count: 3,
        };
        let mut snapshot = S4ContainerSnapshot {
            dim: 2,
            pos: (-28, 65, 42),
            state: 26685,
            side: Some(S4BlockSide::Up),
            slot_limit: 16,
            slots: vec![item(12), vanilla, S4ItemStack::EMPTY],
        };
        let read = S4ContainerOp::Read { slot: 0 };
        let simulate = [
            S4ContainerOp::Insert {
                slot: 0,
                stack: item(7),
                simulate: true,
            },
            S4ContainerOp::Extract {
                slot: 0,
                amount: 8,
                simulate: true,
            },
            read,
        ];
        let result = query_s4_block_items(&mut env, &snapshot, &simulate)
            .unwrap()
            .unwrap();
        assert_eq!(result.handler_slots, 3);
        assert_eq!(
            result.results,
            [item(3), item(8), item(12)].map(|stack| S4SlotResult { limit: 16, stack })
        );
        assert!(result.mutations.is_empty());

        let ops = [
            S4ContainerOp::Insert {
                slot: 0,
                stack: item(7),
                simulate: false,
            },
            S4ContainerOp::Insert {
                slot: 1,
                stack: item(5),
                simulate: false,
            },
            S4ContainerOp::Extract {
                slot: 0,
                amount: 6,
                simulate: false,
            },
            read,
            S4ContainerOp::Read { slot: 2 },
            S4ContainerOp::Extract {
                slot: 1,
                amount: 100,
                simulate: false,
            },
        ];
        let result = query_s4_block_items(&mut env, &snapshot, &ops)
            .unwrap()
            .unwrap();
        assert_eq!(
            result.results,
            [
                item(3),
                item(5),
                item(6),
                item(10),
                S4ItemStack::EMPTY,
                vanilla
            ]
            .map(|stack| S4SlotResult { limit: 16, stack })
        );
        assert_eq!(
            result.mutations,
            [
                S4SlotMutation {
                    dim: 2,
                    pos: snapshot.pos,
                    slot: 0,
                    expected_state: 26685,
                    expected: item(12),
                    replacement: item(10)
                },
                S4SlotMutation {
                    dim: 2,
                    pos: snapshot.pos,
                    slot: 1,
                    expected_state: 26685,
                    expected: vanilla,
                    replacement: S4ItemStack::EMPTY
                },
            ]
        );
        // A caller applies the proposals separately. Reusing the pre-call
        // snapshot must return the original stack, including after a failure.
        let mut invalid = ops.to_vec();
        invalid.push(S4ContainerOp::Read { slot: 99 });
        assert!(query_s4_block_items(&mut env, &snapshot, &invalid).is_err());
        let result = query_s4_block_items(&mut env, &snapshot, &[read])
            .unwrap()
            .unwrap();
        assert_eq!(result.results[0].stack, item(12));
        snapshot.side = Some(S4BlockSide::Down);
        assert!(query_s4_block_items(&mut env, &snapshot, &[read])
            .unwrap()
            .is_none());
        snapshot.side = None;
        snapshot.state = 26684;
        assert!(query_s4_block_items(&mut env, &snapshot, &[read])
            .unwrap()
            .is_none());
        println!("S4 capability results and conditional mutations verified");

        let registry = fetch_registry_snapshot(&mut env).unwrap();
        let packets = build_config_registry_sync_packets(&registry).unwrap();
        assert_eq!(packets, [
            b"\x07\x0fminecraft:block\x02\x13nf5_fixture:a_other\x00\x17nf5_fixture:z_container\x00".to_vec(),
            b"\x07\x0eminecraft:item\x02\x12nf5_fixture:a_item\x00\x12nf5_fixture:z_item\x00".to_vec(),
        ]);
        assert_eq!(
            packets,
            build_config_registry_sync_packets(&fetch_registry_snapshot(&mut env).unwrap())
                .unwrap()
        );
        println!("S4 deterministic Registry Data packet bodies verified");
    }

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

        let mut canceled = batch(true);
        canceled.mutations.clear();
        assert_eq!(apply_s1_outcome(canceled), Ok(BatchCommitReport::default()));
        assert_eq!((state(end), state(nether)), (end_before, nether_before));

        let mut invalid = batch(false);
        invalid.mutations.push(block(3, 1, None));
        assert!(apply_s1_outcome(invalid).is_err());
        assert_eq!((state(end), state(nether)), (end_before, nether_before));

        assert!(apply_s1_outcome(batch(false))
            .unwrap_err()
            .contains("CAS precondition"));
        assert_eq!((state(end), state(nether)), (end_before, nether_before));

        let mut valid = batch(false);
        valid.mutations.remove(1);
        assert_eq!(
            apply_s1_outcome(valid),
            Ok(BatchCommitReport {
                applied_blocks: 3,
                skipped_cas_blocks: 0,
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

    #[test]
    fn s1_cross_dimension_failed_last_cas_is_atomic() {
        let _world = super::super::world_guard();
        let pos = (3209, 85, -3206);
        let dim = [
            super::super::DimId::Overworld,
            super::super::DimId::Nether,
            super::super::DimId::End,
        ];
        let previous = dim.map(|d| super::super::block_at_dim(d, pos.0, pos.1, pos.2));
        let mutations = dim
            .iter()
            .enumerate()
            .map(|(i, _)| MutationOp::SetBlockIf {
                dim: i as u8,
                pos,
                new_state: if previous[i] == 1 { 2 } else { 1 },
                expected_state: Some(if i == 2 { u32::MAX } else { previous[i] }),
            })
            .collect();
        let failure = S1EventOutcome {
            canceled: false,
            break_exp: None,
            chat_message: None,
            mutations,
        };
        // The invalid final CAS must reject the whole batch before the
        // Overworld or Nether can acquire even a transient edit.
        assert!(apply_s1_outcome(failure).is_err());
        assert_eq!(
            dim.map(|d| super::super::block_at_dim(d, pos.0, pos.1, pos.2)),
            previous
        );

        let mutations = dim
            .iter()
            .enumerate()
            .map(|(i, _)| MutationOp::SetBlockIf {
                dim: i as u8,
                pos,
                new_state: if previous[i] == 1 { 2 } else { 1 },
                expected_state: Some(if i == 2 { 0 } else { previous[i] }),
            })
            .collect();
        // A valid but stale final expected state must also roll back all dimensions.
        let mut stale = S1EventOutcome {
            canceled: false,
            break_exp: None,
            chat_message: None,
            mutations,
        };
        let stale_state = if previous[2] == 0 { 1 } else { 0 };
        if let MutationOp::SetBlockIf { expected_state, .. } = &mut stale.mutations[2] {
            *expected_state = Some(stale_state);
        }
        assert!(apply_s1_outcome(stale)
            .unwrap_err()
            .contains("CAS precondition"));
        assert_eq!(
            dim.map(|d| super::super::block_at_dim(d, pos.0, pos.1, pos.2)),
            previous
        );
    }

    #[test]
    fn s1_invalid_bounds_states_counts_and_canceled_outcomes_fail_closed() {
        let _world = super::super::world_guard();
        let pos = (3214, 80, -3212);
        let end = super::super::DimId::End;
        let before = super::super::block_at_dim(end, pos.0, pos.1, pos.2);
        let good = || MutationOp::SetBlockIf {
            dim: 2,
            pos,
            new_state: 1,
            expected_state: Some(before),
        };
        let check = |mutation: MutationOp| {
            let outcome = S1EventOutcome {
                canceled: false,
                break_exp: None,
                chat_message: None,
                mutations: vec![good(), mutation],
            };
            assert!(apply_s1_outcome(outcome).is_err());
            assert_eq!(super::super::block_at_dim(end, pos.0, pos.1, pos.2), before);
        };
        for (dim, y) in [
            (0, -65),
            (0, 320),
            (1, -1),
            (1, 256),
            (2, -1),
            (2, 256),
            (3, 80),
            (u8::MAX, 80),
        ] {
            check(MutationOp::SetBlockIf {
                dim,
                pos: (pos.0, y, pos.2),
                new_state: 1,
                expected_state: None,
            });
        }
        for (new_state, expected_state) in [(u32::MAX, None), (1, Some(u32::MAX))] {
            check(MutationOp::SetBlockIf {
                dim: 2,
                pos,
                new_state,
                expected_state,
            });
        }
        assert!(apply_s1_outcome(S1EventOutcome {
            canceled: true,
            break_exp: None,
            chat_message: None,
            mutations: vec![good()]
        })
        .is_err());
        assert_eq!(super::super::block_at_dim(end, pos.0, pos.1, pos.2), before);
        let mut mutations = (0..S1_MAX_MUTATIONS)
            .map(|_| MutationOp::SendSystemMessage {
                player_eid: 1,
                message: "x".into(),
            })
            .collect::<Vec<_>>();
        mutations.insert(0, good());
        let outcome = S1EventOutcome {
            canceled: false,
            break_exp: None,
            chat_message: None,
            mutations,
        };
        assert!(apply_s1_outcome(outcome).is_err());
        assert_eq!(super::super::block_at_dim(end, pos.0, pos.1, pos.2), before);
    }

    #[cfg(feature = "neoforge-events")]
    #[test]
    fn s3_s4_host_rejects_oversized_and_multiple_payloads() {
        use crate::landing_types::{S3Activation, S3TickRequest, S3_MAX_RECORDS};

        let request = S3TickRequest {
            activation: S3Activation::BlockEntityTick,
            dim: 0,
            kind: 1,
            id: 1,
            generation: 1,
            pos: (0, 64, 0),
            state: 1,
        };
        let mut buf = vec![0; S3_CAPACITY];
        assert!(encode_s3_request_batch(&mut buf, 1, &vec![request; S3_MAX_RECORDS + 1]).is_err());
        let mut response = vec![b'S', b'3', 1, 1];
        response.extend_from_slice(&((S3_MAX_RECORDS + 1) as u32).to_le_bytes());
        response.extend_from_slice(&1u64.to_le_bytes());
        assert!(decode_s3_response_batch(&response, 1, &[request]).is_err());

        let envelope = CustomPayloadEnvelope {
            channel: "test:hello".into(),
            target_eid: None,
            bytes: vec![1, 2],
        };
        let valid = encode_s4_payload_batch(std::slice::from_ref(&envelope)).unwrap();
        assert!(validate_s4_inbound_frame(&valid).is_ok());
        assert!(validate_s4_inbound_frame(&valid[..valid.len() - 1]).is_err());
        let double = encode_s4_payload_batch(&[envelope.clone(), envelope.clone()]).unwrap();
        assert!(validate_s4_inbound_frame(&double).is_err());
        let targeted = encode_s4_payload_batch(&[CustomPayloadEnvelope {
            target_eid: Some(42),
            ..envelope.clone()
        }])
        .unwrap();
        assert!(validate_s4_inbound_frame(&targeted).is_err());
        let mut oversized_header = valid.clone();
        let payload_offset = oversized_header.len() - envelope.bytes.len();
        oversized_header[payload_offset - 4..payload_offset]
            .copy_from_slice(&((S4_MAX_PAYLOAD + 1) as u32).to_le_bytes());
        assert!(validate_s4_inbound_frame(&oversized_header)
            .unwrap_err()
            .contains("oversized S4 payload frame"));
        let too_many = vec![envelope; 65];
        assert!(encode_s4_payload_batch(&too_many).is_err());
        let oversized = vec![0; S4_MAX_PAYLOAD + 1];
        assert!(encode_s4_payload_batch(&[CustomPayloadEnvelope {
            channel: "test:hello".into(),
            target_eid: None,
            bytes: oversized,
        }])
        .is_err());
    }
}
