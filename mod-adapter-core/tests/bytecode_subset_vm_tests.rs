// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! End-to-end tests for `bytecode_subset_vm`: real JVMS §4 `.class`
//! binaries hand-assembled byte-by-byte (no `javac`, no JVM, no crates)
//! exercising the full pipeline — parse → static verify → bounded
//! execute — plus the hostile-bytecode rejection battery.

use mod_adapter_core::bytecode_subset_vm::testkit::{ClassAssembler, MethodSpec};
use mod_adapter_core::bytecode_subset_vm::{
    execute_verified_method, parse_class_methods, verify_subset_method, HostCallKind, SubsetVmHost,
    VmError, VmValue, DEFAULT_FUEL,
};

// ---------------------------------------------------------------------------
// Bytecode instruction helpers (JVMS §6 opcodes)
// ---------------------------------------------------------------------------

fn invokevirtual(index: u16) -> [u8; 3] {
    let [hi, lo] = index.to_be_bytes();
    [0xB6, hi, lo]
}

fn invokestatic(index: u16) -> [u8; 3] {
    let [hi, lo] = index.to_be_bytes();
    [0xB8, hi, lo]
}

fn invokeinterface(index: u16, count: u8) -> [u8; 5] {
    let [hi, lo] = index.to_be_bytes();
    [0xB9, hi, lo, count, 0]
}

fn ldc(index: u16) -> [u8; 2] {
    [0x12, index as u8]
}

fn bipush(v: i8) -> [u8; 2] {
    [0x10, v as u8]
}

fn sipush(v: i16) -> [u8; 3] {
    let [hi, lo] = v.to_be_bytes();
    [0x11, hi, lo]
}

fn iinc(slot: u8, delta: i8) -> [u8; 3] {
    [0x84, slot, delta as u8]
}

fn branch(opcode: u8, delta: i16) -> [u8; 3] {
    let [hi, lo] = delta.to_be_bytes();
    [opcode, hi, lo]
}

fn goto(delta: i16) -> [u8; 3] {
    branch(0xA7, delta)
}

fn code(parts: &[&[u8]]) -> Vec<u8> {
    parts.concat()
}

const NOP: &[u8] = &[0x00];
const ICONST_M1: &[u8] = &[0x02];
const ICONST_0: &[u8] = &[0x03];
const ICONST_2: &[u8] = &[0x05];
const ICONST_5: &[u8] = &[0x08];
const FCONST_0: &[u8] = &[0x0B];
const ALOAD_0: &[u8] = &[0x2A];
const ALOAD_1: &[u8] = &[0x2B];
const ISTORE_1: &[u8] = &[0x3C];
const POP: &[u8] = &[0x57];
const IADD: &[u8] = &[0x60];
const IDIV: &[u8] = &[0x6C];
const FADD: &[u8] = &[0x62];
const FMUL: &[u8] = &[0x6A];
const FCMPL: &[u8] = &[0x94];
const FCMPG: &[u8] = &[0x95];
const IRETURN: &[u8] = &[0xAC];
const RETURN: &[u8] = &[0xB1];

// ---------------------------------------------------------------------------
// Recording host
// ---------------------------------------------------------------------------

/// A `SubsetVmHost` that records every call and keeps a tiny fake world:
/// one player with health, one damage event, block/ item/message logs.
#[derive(Default)]
struct RecordingHost {
    player_health: f32,
    event_damage: f32,
    event_cancelled: bool,
    messages: Vec<String>,
    items_given: Vec<(String, i32)>,
    blocks: std::collections::BTreeMap<(i32, i32, i32), String>,
    calls: Vec<HostCallKind>,
}

impl RecordingHost {
    fn with_health(health: f32) -> Self {
        Self {
            player_health: health,
            ..Self::default()
        }
    }

    fn with_damage(damage: f32) -> Self {
        Self {
            event_damage: damage,
            ..Self::default()
        }
    }
}

impl SubsetVmHost for RecordingHost {
    fn call(&mut self, kind: HostCallKind, args: &[VmValue]) -> Result<Option<VmValue>, VmError> {
        self.calls.push(kind);
        match kind {
            HostCallKind::PlayerGetHealth => Ok(Some(VmValue::Float(self.player_health))),
            HostCallKind::PlayerSetHealth => {
                let VmValue::Float(v) = args[1] else {
                    panic!("setHealth arg must be Float")
                };
                self.player_health = v;
                Ok(None)
            }
            HostCallKind::PlayerSendMessage => {
                let VmValue::Str(s) = &args[1] else {
                    panic!("sendMessage arg must be Str")
                };
                self.messages.push(s.clone());
                Ok(None)
            }
            HostCallKind::PlayerGiveItem => {
                let (VmValue::Str(item), VmValue::Int(n)) = (&args[1], &args[2]) else {
                    panic!("giveItem args must be (Str, Int)")
                };
                self.items_given.push((item.clone(), *n));
                Ok(None)
            }
            HostCallKind::LevelGetBlock => {
                let (VmValue::Int(x), VmValue::Int(y), VmValue::Int(z)) =
                    (&args[1], &args[2], &args[3])
                else {
                    panic!("getBlock args must be (Int, Int, Int)")
                };
                Ok(Some(VmValue::Str(
                    self.blocks
                        .get(&(*x, *y, *z))
                        .cloned()
                        .unwrap_or_else(|| "minecraft:air".to_string()),
                )))
            }
            HostCallKind::LevelSetBlock => {
                let (VmValue::Int(x), VmValue::Int(y), VmValue::Int(z), VmValue::Str(b)) =
                    (&args[1], &args[2], &args[3], &args[4])
                else {
                    panic!("setBlock args must be (Int, Int, Int, Str)")
                };
                self.blocks.insert((*x, *y, *z), b.clone());
                Ok(None)
            }
            HostCallKind::EventGetDamage => Ok(Some(VmValue::Float(self.event_damage))),
            HostCallKind::EventSetDamage => {
                let VmValue::Float(v) = args[1] else {
                    panic!("setDamage arg must be Float")
                };
                self.event_damage = v;
                Ok(None)
            }
            HostCallKind::EventCancel => {
                self.event_cancelled = true;
                Ok(None)
            }
            HostCallKind::MathMaxFloat | HostCallKind::MathMinFloat => {
                let (VmValue::Float(a), VmValue::Float(b)) = (&args[0], &args[1]) else {
                    panic!("Math.{:?} args must be (Float, Float)", kind)
                };
                let v = if kind == HostCallKind::MathMaxFloat {
                    a.max(*b)
                } else {
                    a.min(*b)
                };
                Ok(Some(VmValue::Float(v)))
            }
            HostCallKind::MathClampFloat => {
                let (VmValue::Float(v), VmValue::Float(lo), VmValue::Float(hi)) =
                    (&args[0], &args[1], &args[2])
                else {
                    panic!("clamp args must be (Float, Float, Float)")
                };
                Ok(Some(VmValue::Float(v.clamp(*lo, *hi))))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Fixture builders
// ---------------------------------------------------------------------------

/// `onDamage(Lmcspine/api/Event;)V` implementing the classic
/// `if (event.getDamage() > 10.0f) event.setDamage(event.getDamage() * 0.5f)`
/// damage-modifier hook — symbolic `mcspine/api` host names.
fn damage_hook_class() -> Vec<u8> {
    let mut a = ClassAssembler::new("com/example/DamageHook");
    let get_damage = a.cp.method_ref("mcspine/api/Event", "getDamage", "()F");
    let set_damage = a.cp.method_ref("mcspine/api/Event", "setDamage", "(F)V");
    let ten = a.cp.float(10.0);
    let half = a.cp.float(0.5);
    //  0: aload_0
    //  1: invokevirtual getDamage   -> Float
    //  4: ldc 10.0f
    //  6: fcmpl                     (javac emits fcmpl for `>`)
    //  7: ifle +14 -> 21 (else: return)
    // 10: aload_0
    // 11: aload_0
    // 12: invokevirtual getDamage
    // 15: ldc 0.5f
    // 17: fmul
    // 18: invokevirtual setDamage
    // 21: return
    let bc = code(&[
        ALOAD_0,
        &invokevirtual(get_damage),
        &ldc(ten),
        FCMPL,
        &branch(0x9E, 14), // ifle -> offset 21
        ALOAD_0,
        ALOAD_0,
        &invokevirtual(get_damage),
        &ldc(half),
        FMUL,
        &invokevirtual(set_damage),
        RETURN,
    ]);
    assert_eq!(bc.len(), 22);
    a.finish(&[MethodSpec::new(
        "onDamage",
        "(Lmcspine/api/Event;)V",
        4,
        1,
        bc,
    )])
}

/// `onRightClick(Lnet/minecraft/world/entity/player/Player;)V` —
/// Mojang-mapped host names:
/// `player.setHealth(Math.min(20.0f, player.getHealth() + 4.0f));
///  player.sendMessage("Healed!");`
fn heal_handler_class() -> Vec<u8> {
    let mut a = ClassAssembler::new("com/example/HealHandler");
    let player = "net/minecraft/world/entity/player/Player";
    let get_health = a.cp.method_ref(player, "getHealth", "()F");
    let set_health = a.cp.method_ref(player, "setHealth", "(F)V");
    let send_msg =
        a.cp.method_ref(player, "sendMessage", "(Ljava/lang/String;)V");
    let math_min = a.cp.method_ref("java/lang/Math", "min", "(FF)F");
    let twenty = a.cp.float(20.0);
    let four = a.cp.float(4.0);
    let msg = a.cp.string("Healed!");
    //  0: aload_0                     ; receiver for setHealth
    //  1: ldc 20.0f
    //  3: aload_0                     ; receiver for getHealth
    //  4: invokevirtual getHealth
    //  7: ldc 4.0f
    //  9: fadd
    // 10: invokestatic Math.min (FF)F
    // 13: invokevirtual setHealth
    // 16: aload_0
    // 17: ldc "Healed!"
    // 19: invokevirtual sendMessage
    // 22: return
    let bc = code(&[
        ALOAD_0,
        &ldc(twenty),
        ALOAD_0,
        &invokevirtual(get_health),
        &ldc(four),
        FADD,
        &invokestatic(math_min),
        &invokevirtual(set_health),
        ALOAD_0,
        &ldc(msg),
        &invokevirtual(send_msg),
        RETURN,
    ]);
    assert_eq!(bc.len(), 23);
    a.finish(&[MethodSpec::new(
        "onRightClick",
        "(Lnet/minecraft/world/entity/player/Player;)V",
        4,
        1,
        bc,
    )])
}

/// One-method `()V` class whose code is exactly `bc` — for the hostile
/// and edge-case battery. `cp_setup` interns whatever constants the code
/// needs before `finish` serializes.
fn hostile_class(name: &str, bc: Vec<u8>, cp_setup: impl FnOnce(&mut ClassAssembler)) -> Vec<u8> {
    let mut a = ClassAssembler::new(name);
    cp_setup(&mut a);
    a.finish(&[MethodSpec::new("run", "()V", 8, 1, bc)])
}

fn verify_err(class_bytes: &[u8], name: &str, descriptor: &str) -> VmError {
    let parsed = parse_class_methods(class_bytes).expect("class bytes must parse");
    verify_subset_method(&parsed, name, descriptor).expect_err("method must fail verification")
}

// ---------------------------------------------------------------------------
// Real-behavior tests
// ---------------------------------------------------------------------------

#[test]
fn parses_class_structure() {
    let bytes = damage_hook_class();
    assert_eq!(&bytes[0..4], &[0xCA, 0xFE, 0xBA, 0xBE]);
    let parsed = parse_class_methods(&bytes).expect("fixture parses");
    assert_eq!(parsed.class_name, "com/example/DamageHook");
    assert_eq!(parsed.methods.len(), 1);
    let m = &parsed.methods[0];
    assert_eq!(m.name, "onDamage");
    assert_eq!(m.descriptor, "(Lmcspine/api/Event;)V");
    assert_eq!(m.max_stack, 4);
    assert_eq!(m.max_locals, 1);
    assert_eq!(m.code.len(), 22);
}

#[test]
fn damage_hook_halves_damage_above_threshold_zero_jvm() {
    let bytes = damage_hook_class();
    let parsed = parse_class_methods(&bytes).unwrap();
    let method = verify_subset_method(&parsed, "onDamage", "(Lmcspine/api/Event;)V")
        .expect("damage hook verifies cleanly");

    // 14.0 damage: above threshold -> halved to 7.0.
    let mut host = RecordingHost::with_damage(14.0);
    let result =
        execute_verified_method(&method, &[VmValue::Ref(1)], &mut host, DEFAULT_FUEL).unwrap();
    assert_eq!(result.return_value, None);
    assert!(result.instructions_executed > 0);
    assert_eq!(host.event_damage, 7.0);
    assert_eq!(
        host.calls,
        vec![
            HostCallKind::EventGetDamage,
            HostCallKind::EventGetDamage,
            HostCallKind::EventSetDamage
        ]
    );

    // 6.0 damage: below threshold -> untouched.
    let mut host = RecordingHost::with_damage(6.0);
    execute_verified_method(&method, &[VmValue::Ref(1)], &mut host, DEFAULT_FUEL).unwrap();
    assert_eq!(host.event_damage, 6.0);
    assert_eq!(host.calls, vec![HostCallKind::EventGetDamage]);
}

#[test]
fn heal_handler_clamps_health_and_sends_message_mojang_names() {
    let bytes = heal_handler_class();
    let parsed = parse_class_methods(&bytes).unwrap();
    let method = verify_subset_method(
        &parsed,
        "onRightClick",
        "(Lnet/minecraft/world/entity/player/Player;)V",
    )
    .expect("heal handler verifies cleanly");

    // 17.5 + 4 = 21.5 -> clamped to 20 by Math.min.
    let mut host = RecordingHost::with_health(17.5);
    execute_verified_method(&method, &[VmValue::Ref(7)], &mut host, DEFAULT_FUEL).unwrap();
    assert_eq!(host.player_health, 20.0);
    assert_eq!(host.messages, vec!["Healed!".to_string()]);
    assert_eq!(
        host.calls,
        vec![
            HostCallKind::PlayerGetHealth,
            HostCallKind::MathMinFloat,
            HostCallKind::PlayerSetHealth,
            HostCallKind::PlayerSendMessage,
        ]
    );

    // 10 + 4 = 14 -> below the cap, no clamping.
    let mut host = RecordingHost::with_health(10.0);
    execute_verified_method(&method, &[VmValue::Ref(7)], &mut host, DEFAULT_FUEL).unwrap();
    assert_eq!(host.player_health, 14.0);
}

#[test]
fn interface_invoke_and_block_access_roundtrip() {
    // Covers invokeinterface + LevelGetBlock/LevelSetBlock + giveItem:
    //   if (level.getBlockId(x,y,z).equals-ish "minecraft:air") -> place,
    //   else give item. Simplified: set a block, give a pickaxe, read a
    //   block name back via Str result.
    let mut a = ClassAssembler::new("com/example/BlockLogic");
    let level = "mcspine/api/Level";
    let player = "mcspine/api/Player";
    let get_block =
        a.cp.interface_method_ref(level, "getBlockId", "(III)Ljava/lang/String;");
    let set_block =
        a.cp.method_ref(level, "setBlock", "(IIILjava/lang/String;)V");
    let give =
        a.cp.interface_method_ref(player, "giveItem", "(Ljava/lang/String;I)V");
    let stone = a.cp.string("minecraft:stone");
    let pick = a.cp.string("minecraft:diamond_pickaxe");
    // max_locals: 1=level ref, 2=player ref (args seed 0,1)
    let bc = code(&[
        ALOAD_0,
        &bipush(3),
        &bipush(64),
        &bipush(-7),
        &ldc(stone),
        &invokevirtual(set_block), // level.setBlock(3,64,-7,"minecraft:stone")
        ALOAD_1,
        &ldc(pick),
        &bipush(1),
        &invokeinterface(give, 3), // player.giveItem("diamond_pickaxe", 1); count=3 (recv+2 args)
        ALOAD_0,
        &bipush(3),
        &bipush(64),
        &bipush(-7),
        &invokeinterface(get_block, 4), // count=4 (recv+3 args)
        POP,                            // discard the Str result
        RETURN,
    ]);
    let bytes = a.finish(&[MethodSpec::new(
        "run",
        "(Lmcspine/api/Level;Lmcspine/api/Player;)V",
        8,
        2,
        bc,
    )]);
    let parsed = parse_class_methods(&bytes).unwrap();
    let method = verify_subset_method(&parsed, "run", "(Lmcspine/api/Level;Lmcspine/api/Player;)V")
        .expect("block logic verifies");
    let mut host = RecordingHost::default();
    execute_verified_method(
        &method,
        &[VmValue::Ref(10), VmValue::Ref(11)],
        &mut host,
        DEFAULT_FUEL,
    )
    .unwrap();
    assert_eq!(
        host.blocks.get(&(3, 64, -7)),
        Some(&"minecraft:stone".to_string())
    );
    assert_eq!(
        host.items_given,
        vec![("minecraft:diamond_pickaxe".to_string(), 1)]
    );
}

#[test]
fn integer_arithmetic_wraps_like_java() {
    // (i32::MAX + 2) / -1: Java integer arithmetic never traps on
    // overflow — the add wraps to i32::MIN + 1, the divide is exact.
    let mut a = ClassAssembler::new("com/example/Overflow");
    let max_i32 = a.cp.int(i32::MAX);
    let bc = code(&[&ldc(max_i32), ICONST_2, IADD, ICONST_M1, IDIV, IRETURN]);
    let bytes = a.finish(&[MethodSpec::static_method("calc", "()I", 4, 0, bc)]);
    let parsed = parse_class_methods(&bytes).unwrap();
    let method = verify_subset_method(&parsed, "calc", "()I").unwrap();
    let mut host = RecordingHost::default();
    let result = execute_verified_method(&method, &[], &mut host, DEFAULT_FUEL).unwrap();
    assert_eq!(result.return_value, Some(VmValue::Int(i32::MAX)));

    // i32::MIN / -1 wraps back to i32::MIN (JVMS idiv overflow rule).
    let mut a = ClassAssembler::new("com/example/Overflow2");
    let min_i32 = a.cp.int(i32::MIN);
    let bc = code(&[&ldc(min_i32), ICONST_M1, IDIV, IRETURN]);
    let bytes = a.finish(&[MethodSpec::static_method("calc", "()I", 4, 0, bc)]);
    let parsed = parse_class_methods(&bytes).unwrap();
    let method = verify_subset_method(&parsed, "calc", "()I").unwrap();
    let result = execute_verified_method(&method, &[], &mut host, DEFAULT_FUEL).unwrap();
    assert_eq!(result.return_value, Some(VmValue::Int(i32::MIN)));
}

#[test]
fn fcmpl_fcmpg_nan_polarity_matches_jvms() {
    let mut a = ClassAssembler::new("com/example/Cmp");
    let nan = a.cp.float(f32::NAN);
    // fcmpg(NaN, 1.0) -> 1
    let g = code(&[&ldc(nan), &ldc(nan), FCMPG, IRETURN]);
    // fcmpl(NaN, NaN) -> -1
    let l = code(&[&ldc(nan), &ldc(nan), FCMPL, IRETURN]);
    let bytes = a.finish(&[
        MethodSpec::static_method("g", "()I", 4, 0, g),
        MethodSpec::static_method("l", "()I", 4, 0, l),
    ]);
    let parsed = parse_class_methods(&bytes).unwrap();
    let mut host = RecordingHost::default();
    let g = verify_subset_method(&parsed, "g", "()I").unwrap();
    let l = verify_subset_method(&parsed, "l", "()I").unwrap();
    assert_eq!(
        execute_verified_method(&g, &[], &mut host, DEFAULT_FUEL)
            .unwrap()
            .return_value,
        Some(VmValue::Int(1))
    );
    assert_eq!(
        execute_verified_method(&l, &[], &mut host, DEFAULT_FUEL)
            .unwrap()
            .return_value,
        Some(VmValue::Int(-1))
    );
}

#[test]
fn f2i_saturates_and_nan_becomes_zero() {
    let mut a = ClassAssembler::new("com/example/Convert");
    let nan = a.cp.float(f32::NAN);
    let big = a.cp.float(1e20);
    let bc_nan = code(&[&ldc(nan), &[0x86], IRETURN]); // f2i
    let bc_big = code(&[&ldc(big), &[0x86], IRETURN]);
    let bytes = a.finish(&[
        MethodSpec::static_method("nan", "()I", 2, 0, bc_nan),
        MethodSpec::static_method("big", "()I", 2, 0, bc_big),
    ]);
    let parsed = parse_class_methods(&bytes).unwrap();
    let mut host = RecordingHost::default();
    for (name, expected) in [("nan", 0), ("big", i32::MAX)] {
        let m = verify_subset_method(&parsed, name, "()I").unwrap();
        assert_eq!(
            execute_verified_method(&m, &[], &mut host, DEFAULT_FUEL)
                .unwrap()
                .return_value,
            Some(VmValue::Int(expected)),
            "{name}"
        );
    }
}

#[test]
fn iinc_loop_sums_to_return_value() {
    // int i=0, sum=0; while (i<5) { sum+=i; i++; } return sum;  -> 10.
    // locals: 0=i, 1=sum.
    let loop_body = [
        ICONST_0.to_vec(),         // 0: i=0
        vec![0x3B],                // 1: istore_0
        ICONST_0.to_vec(),         // 2: sum=0
        ISTORE_1.to_vec(),         // 3: istore_1
        vec![0x1A],                // 4: iload_0
        ICONST_5.to_vec(),         // 5: iconst_5
        branch(0xA2, 13).to_vec(), // 6: if_icmpge +13 -> 19 (exit)
        vec![0x1B],                // 9: iload_1
        vec![0x1A],                // 10: iload_0
        IADD.to_vec(),             // 11: iadd
        ISTORE_1.to_vec(),         // 12: istore_1
        iinc(0, 1).to_vec(),       // 13: iinc i
        goto(-12).to_vec(),        // 16: goto -12 -> 4 (loop head)
        vec![0x1B],                // 19: iload_1
        IRETURN.to_vec(),          // 20: ireturn
    ]
    .concat();
    let a = ClassAssembler::new("com/example/LoopSum");
    let bytes = a.finish(&[MethodSpec::static_method("sum", "()I", 4, 2, loop_body)]);
    let parsed = parse_class_methods(&bytes).unwrap();
    let method = verify_subset_method(&parsed, "sum", "()I").unwrap();
    let mut host = RecordingHost::default();
    let result = execute_verified_method(&method, &[], &mut host, DEFAULT_FUEL).unwrap();
    assert_eq!(result.return_value, Some(VmValue::Int(10)));
}

// ---------------------------------------------------------------------------
// Fuel / runtime error tests
// ---------------------------------------------------------------------------

#[test]
fn infinite_goto_loop_hits_out_of_fuel_after_exact_budget() {
    let bytes = hostile_class("com/evil/Infinite", vec![0xA7, 0x00, 0x00], |_| {});
    let parsed = parse_class_methods(&bytes).unwrap();
    // `goto +0` at offset 0 jumps to itself — statically legal, so it
    // verifies; the fuel cap is what makes it safe.
    let method = verify_subset_method(&parsed, "run", "()V")
        .expect("self-goto is legal bytecode and verifies");
    let mut host = RecordingHost::default();
    for fuel in [1u32, 7, 1000] {
        let err = execute_verified_method(&method, &[], &mut host, fuel)
            .expect_err("loop must hit the fuel cap");
        assert_eq!(err, VmError::OutOfFuel, "fuel {fuel}");
    }
}

#[test]
fn integer_divide_by_zero_is_clean_error() {
    let bytes = hostile_class(
        "com/example/DivZero",
        code(&[ICONST_5, ICONST_0, IDIV, IRETURN]),
        |_| {},
    );
    let parsed = parse_class_methods(&bytes).unwrap();
    // run()V with ireturn on an int — verifier allows it structurally
    // (stack-depth only), runtime errors on the divide first.
    let method = verify_subset_method(&parsed, "run", "()V").unwrap();
    let mut host = RecordingHost::default();
    let err = execute_verified_method(&method, &[], &mut host, DEFAULT_FUEL)
        .expect_err("division by zero must fail cleanly");
    assert_eq!(err, VmError::ArithmeticDivByZero);
}

#[test]
fn integer_remainder_by_zero_is_clean_error() {
    let bytes = hostile_class(
        "com/example/RemZero",
        code(&[ICONST_5, ICONST_0, &[0x70], IRETURN]), // irem
        |_| {},
    );
    let parsed = parse_class_methods(&bytes).unwrap();
    let method = verify_subset_method(&parsed, "run", "()V").unwrap();
    let mut host = RecordingHost::default();
    let err = execute_verified_method(&method, &[], &mut host, DEFAULT_FUEL)
        .expect_err("remainder by zero must fail cleanly");
    assert_eq!(err, VmError::ArithmeticDivByZero);
}

// ---------------------------------------------------------------------------
// Static verifier rejection battery — every one of these fails BEFORE a
// single instruction executes.
// ---------------------------------------------------------------------------

#[test]
fn rejects_runtime_exec_rce() {
    let mut a = ClassAssembler::new("com/evil/Rce");
    let exec = a.cp.method_ref(
        "java/lang/Runtime",
        "exec",
        "(Ljava/lang/String;)Ljava/lang/Process;",
    );
    let bc = code(&[&invokestatic(exec), POP, RETURN]);
    let bytes = a.finish(&[MethodSpec::new("run", "()V", 8, 1, bc)]);
    let err = verify_err(&bytes, "run", "()V");
    assert!(
        matches!(err, VmError::ForbiddenHostClass { ref class, .. } if class == "java/lang/Runtime"),
        "Runtime.exec must be forbidden: {err:?}"
    );
}

#[test]
fn rejects_process_builder() {
    let mut a = ClassAssembler::new("com/evil/Pb");
    let start =
        a.cp.method_ref("java/lang/ProcessBuilder", "start", "()Ljava/lang/Process;");
    let bc = code(&[ALOAD_0, &invokevirtual(start), POP, RETURN]);
    let bytes = a.finish(&[MethodSpec::new("run", "()V", 8, 1, bc)]);
    let err = verify_err(&bytes, "run", "()V");
    assert!(
        matches!(err, VmError::ForbiddenHostClass { ref class, .. } if class == "java/lang/ProcessBuilder"),
        "ProcessBuilder must be forbidden: {err:?}"
    );
}

#[test]
fn rejects_jndi_lookup_megu2shell() {
    let mut a = ClassAssembler::new("com/evil/Jndi");
    let lookup = a.cp.method_ref(
        "javax/naming/InitialContext",
        "lookup",
        "(Ljava/lang/String;)Ljava/lang/Object;",
    );
    let bc = code(&[ALOAD_0, &invokevirtual(lookup), POP, RETURN]);
    let bytes = a.finish(&[MethodSpec::new("run", "()V", 8, 1, bc)]);
    let err = verify_err(&bytes, "run", "()V");
    assert!(
        matches!(err, VmError::ForbiddenHostClass { ref class, .. } if class == "javax/naming/InitialContext"),
        "JNDI InitialContext must be forbidden: {err:?}"
    );
}

#[test]
fn rejects_reflection_io_and_network() {
    for (class, method, desc) in [
        (
            "java/lang/reflect/Method",
            "invoke",
            "(Ljava/lang/Object;[Ljava/lang/Object;)Ljava/lang/Object;",
        ),
        ("java/io/File", "delete", "()Z"),
        ("java/net/Socket", "connect", "(Ljava/net/SocketAddress;)V"),
        ("sun/misc/Unsafe", "getByte", "(J)B"),
        ("java/lang/System", "exit", "(I)V"),
    ] {
        let mut a = ClassAssembler::new("com/evil/Hostile");
        let mr = a.cp.method_ref(class, method, desc);
        let bc = code(&[&invokestatic(mr), RETURN]);
        let bytes = a.finish(&[MethodSpec::new("run", "()V", 8, 1, bc)]);
        let err = verify_err(&bytes, "run", "()V");
        assert!(
            matches!(err, VmError::ForbiddenHostClass { .. }),
            "{class}.{method} must be forbidden, got {err:?}"
        );
    }
}

#[test]
fn rejects_unknown_host_call() {
    let mut a = ClassAssembler::new("com/evil/Unknown");
    let pwn = a.cp.method_ref("com/evil/Pwn", "pwn", "()V");
    let bc = code(&[&invokestatic(pwn), RETURN]);
    let bytes = a.finish(&[MethodSpec::new("run", "()V", 8, 1, bc)]);
    let err = verify_err(&bytes, "run", "()V");
    assert!(matches!(err, VmError::UnknownHostCall { .. }), "{err:?}");
}

#[test]
fn rejects_host_signature_mismatch() {
    // Math.max exists — but the int overload (II)I is not allowlisted.
    let mut a = ClassAssembler::new("com/evil/SigMis");
    let maxi = a.cp.method_ref("java/lang/Math", "max", "(II)I");
    let bc = code(&[ICONST_2, ICONST_5, &invokestatic(maxi), POP, RETURN]);
    let bytes = a.finish(&[MethodSpec::new("run", "()V", 8, 1, bc)]);
    let err = verify_err(&bytes, "run", "()V");
    assert!(
        matches!(err, VmError::HostSignatureMismatch { .. }),
        "Math.max(II)I must be a signature mismatch: {err:?}"
    );
}

#[test]
fn rejects_disallowed_opcodes() {
    // new / invokedynamic / monitorenter / athrow / anewarray / getfield —
    // each is an opcode-level rejection, no operand required.
    for (opcode, label) in [
        (0xBBu8, "new"),
        (0xBA, "invokedynamic"),
        (0xC2, "monitorenter"),
        (0xBF, "athrow"),
        (0xBD, "anewarray"),
        (0xB4, "getfield"),
        (0xB2, "getstatic"),
        (0xB7, "invokespecial"),
        (0xAA, "tableswitch"),
        (0xC5, "multianewarray"),
    ] {
        let bytes = hostile_class("com/evil/Opcode", vec![opcode], |_| {});
        let err = verify_err(&bytes, "run", "()V");
        assert!(
            matches!(err, VmError::DisallowedOpcode { opcode: op, .. } if op == opcode),
            "opcode 0x{opcode:02X} ({label}) must be disallowed: {err:?}"
        );
    }
}

#[test]
fn rejects_branch_into_middle_of_instruction() {
    // sipush 0x1234 (3 bytes at 0..2); goto -2 at offset 3 targets
    // offset 1 — the middle of the sipush operand.
    let bytes = hostile_class(
        "com/evil/MidJump",
        code(&[&sipush(0x1234), &goto(-2)]),
        |_| {},
    );
    let err = verify_err(&bytes, "run", "()V");
    assert!(
        matches!(err, VmError::BadBranchTarget { target: 1, .. }),
        "jump into operand bytes must fail: {err:?}"
    );
}

#[test]
fn rejects_out_of_bounds_branch() {
    // goto +100 at offset 0 targets 100 — far past the 4-byte code array.
    let bytes = hostile_class("com/evil/OobJump", code(&[NOP, &goto(100)]), |_| {});
    let err = verify_err(&bytes, "run", "()V");
    assert!(
        matches!(err, VmError::BadBranchTarget { .. }),
        "out-of-bounds jump must fail: {err:?}"
    );

    // Backward jump before offset 0.
    let bytes = hostile_class(
        "com/evil/NegJump",
        code(&[NOP, NOP, NOP, &goto(-10)]),
        |_| {},
    );
    let err = verify_err(&bytes, "run", "()V");
    assert!(
        matches!(err, VmError::BadBranchTarget { .. }),
        "negative-target jump must fail: {err:?}"
    );
}

#[test]
fn rejects_merged_stack_depth_mismatch() {
    // 0: iconst_1          (depth 1)
    // 1: ifeq +4 -> 5      (exit depth 0 on both edges)
    // 4: iconst_2          (depth 1)
    // 5: ireturn           reached with depth 0 (branch) vs 1 (fallthrough)
    let bytes = hostile_class(
        "com/evil/Merge",
        code(&[&[0x04], &branch(0x99, 4), ICONST_2, IRETURN]),
        |_| {},
    );
    let err = verify_err(&bytes, "run", "()V");
    assert!(
        matches!(err, VmError::VerifyStackMerge { .. }),
        "inconsistent merge depth must fail: {err:?}"
    );
}

#[test]
fn rejects_verifier_stack_underflow_and_overflow() {
    // iadd with an empty stack — caught statically by the dataflow pass.
    let bytes = hostile_class("com/evil/Underflow", code(&[IADD, RETURN]), |_| {});
    let err = verify_err(&bytes, "run", "()V");
    assert!(
        matches!(err, VmError::VerifyStackUnderflow { .. }),
        "{err:?}"
    );

    // Declared max_stack=1 but the code pushes 2 values.
    let a = ClassAssembler::new("com/evil/Overflow");
    let bytes = a.finish(&[MethodSpec::new(
        "run",
        "()V",
        1,
        1,
        code(&[ICONST_2, ICONST_5, RETURN]),
    )]);
    let err = verify_err(&bytes, "run", "()V");
    assert!(
        matches!(err, VmError::VerifyStackOverflow { .. }),
        "{err:?}"
    );
}

#[test]
fn rejects_structural_limit_violations() {
    let a = ClassAssembler::new("com/evil/Limits");
    let tiny = code(&[RETURN]);
    let bytes = a.finish(&[MethodSpec::new("run", "()V", 65, 1, tiny.clone())]);
    let err = verify_err(&bytes, "run", "()V");
    assert!(matches!(err, VmError::StackTooDeep { .. }), "{err:?}");

    let a = ClassAssembler::new("com/evil/Limits");
    let bytes = a.finish(&[MethodSpec::new("run", "()V", 4, 65, tiny.clone())]);
    let err = verify_err(&bytes, "run", "()V");
    assert!(matches!(err, VmError::TooManyLocals { .. }), "{err:?}");

    // Local slot outside max_locals (aload 5 with max_locals=1).
    let a = ClassAssembler::new("com/evil/Limits");
    let bytes = a.finish(&[MethodSpec::new(
        "run",
        "()V",
        4,
        1,
        code(&[&[0x19, 0x05], POP, RETURN]), // aload 5
    )]);
    let err = verify_err(&bytes, "run", "()V");
    assert!(
        matches!(err, VmError::LocalIndexOutOfBounds { index: 5 }),
        "{err:?}"
    );

    // code_length over the 4096 cap.
    let a = ClassAssembler::new("com/evil/Limits");
    let big = vec![0x00u8; 4096]
        .into_iter()
        .chain([0xB1])
        .collect::<Vec<_>>();
    let bytes = a.finish(&[MethodSpec::new("run", "()V", 4, 1, big)]);
    let err = verify_err(&bytes, "run", "()V");
    assert!(
        matches!(err, VmError::CodeTooLong { length: 4097, .. }),
        "{err:?}"
    );
}

#[test]
fn rejects_truncated_instruction_and_fallthrough() {
    // bipush without its operand byte.
    let bytes = hostile_class("com/evil/Trunc", vec![0x10], |_| {});
    let err = verify_err(&bytes, "run", "()V");
    assert!(
        matches!(err, VmError::TruncatedInstruction { .. }),
        "{err:?}"
    );

    // Falling off the end: nop as the last instruction.
    let bytes = hostile_class("com/evil/FallThrough", code(&[NOP, ICONST_2, POP]), |_| {});
    let err = verify_err(&bytes, "run", "()V");
    assert!(matches!(err, VmError::VerifyFallThrough { .. }), "{err:?}");
}

#[test]
fn rejects_bad_magic_and_truncation() {
    let mut bad = damage_hook_class();
    bad[0] = 0xDE;
    assert!(matches!(parse_class_methods(&bad), Err(VmError::BadMagic)));

    let truncated = &damage_hook_class()[..12];
    assert!(
        matches!(parse_class_methods(truncated), Err(VmError::Truncated(_))),
        "truncated input must fail"
    );
}

#[test]
fn runtime_type_mismatch_is_clean_error() {
    // astore_0 on a Float — passes the depth-only verifier, fails cleanly
    // at run time.
    let bytes = hostile_class(
        "com/example/TypeMis",
        code(&[FCONST_0, &[0x4B], RETURN]), // fconst_0; astore_0
        |_| {},
    );
    let parsed = parse_class_methods(&bytes).unwrap();
    let method = verify_subset_method(&parsed, "run", "()V").unwrap();
    let mut host = RecordingHost::default();
    let err = execute_verified_method(&method, &[], &mut host, DEFAULT_FUEL)
        .expect_err("astore of Float must fail at run time");
    assert!(matches!(err, VmError::TypeMismatch(_)), "{err:?}");
}
