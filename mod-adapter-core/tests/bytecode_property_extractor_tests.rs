// SPDX-License-Identifier: LGPL-3.0-or-later

use mod_adapter_core::bytecode_property_extractor::{
    extract_class_properties, extract_jar_properties, ExtractionBudget, ExtractorError,
};

#[derive(Clone, Debug)]
enum CpEntry {
    Utf8(String),
    Integer(i32),
    Float(f32),
    Class(u16),
    String(u16),
    NameAndType(u16, u16),
    Methodref(u16, u16),
    MethodHandle(u8, u16),
    MethodType(u16),
    InvokeDynamic(u16, u16),
}

#[derive(Default)]
struct ConstantPoolBuilder {
    entries: Vec<CpEntry>,
}

impl ConstantPoolBuilder {
    fn add(&mut self, entry: CpEntry) -> u16 {
        self.entries.push(entry);
        u16::try_from(self.entries.len()).unwrap()
    }

    fn utf8(&mut self, value: &str) -> u16 {
        self.add(CpEntry::Utf8(value.to_string()))
    }

    fn integer(&mut self, value: i32) -> u16 {
        self.add(CpEntry::Integer(value))
    }

    fn float(&mut self, value: f32) -> u16 {
        self.add(CpEntry::Float(value))
    }

    fn class(&mut self, name: &str) -> u16 {
        let name_index = self.utf8(name);
        self.add(CpEntry::Class(name_index))
    }

    fn string(&mut self, value: &str) -> u16 {
        let utf8 = self.utf8(value);
        self.add(CpEntry::String(utf8))
    }

    fn name_and_type(&mut self, name: &str, descriptor: &str) -> u16 {
        let name = self.utf8(name);
        let descriptor = self.utf8(descriptor);
        self.add(CpEntry::NameAndType(name, descriptor))
    }

    fn method_ref(&mut self, owner: &str, name: &str, descriptor: &str) -> u16 {
        let class = self.class(owner);
        let nat = self.name_and_type(name, descriptor);
        self.add(CpEntry::Methodref(class, nat))
    }

    fn method_handle(&mut self, kind: u8, reference: u16) -> u16 {
        self.add(CpEntry::MethodHandle(kind, reference))
    }

    fn method_type(&mut self, descriptor: &str) -> u16 {
        let descriptor = self.utf8(descriptor);
        self.add(CpEntry::MethodType(descriptor))
    }

    fn invoke_dynamic(&mut self, bootstrap_index: u16, name: &str, descriptor: &str) -> u16 {
        let nat = self.name_and_type(name, descriptor);
        self.add(CpEntry::InvokeDynamic(bootstrap_index, nat))
    }
}

struct MethodSpec {
    access: u16,
    name: String,
    descriptor: String,
    max_stack: u16,
    max_locals: u16,
    code: Vec<u8>,
}

fn push_u2(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_be_bytes());
}

fn push_u4(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_be_bytes());
}

fn assemble_class(
    class_name: &str,
    cp: &mut ConstantPoolBuilder,
    methods: Vec<MethodSpec>,
    bootstrap_methods: Vec<(u16, Vec<u16>)>,
) -> Vec<u8> {
    let this_class = cp.class(class_name);
    let super_class = cp.class("java/lang/Object");
    let code_attribute = cp.utf8("Code");
    let bootstrap_attribute = cp.utf8("BootstrapMethods");
    let method_indexes: Vec<(u16, u16)> = methods
        .iter()
        .map(|method| (cp.utf8(&method.name), cp.utf8(&method.descriptor)))
        .collect();

    let mut out = Vec::new();
    push_u4(&mut out, 0xCAFE_BABE);
    push_u2(&mut out, 0);
    push_u2(&mut out, 61);
    push_u2(&mut out, u16::try_from(cp.entries.len() + 1).unwrap());
    for entry in &cp.entries {
        match entry {
            CpEntry::Utf8(value) => {
                out.push(1);
                push_u2(&mut out, u16::try_from(value.len()).unwrap());
                out.extend_from_slice(value.as_bytes());
            }
            CpEntry::Integer(value) => {
                out.push(3);
                push_u4(&mut out, *value as u32);
            }
            CpEntry::Float(value) => {
                out.push(4);
                push_u4(&mut out, value.to_bits());
            }
            CpEntry::Class(name_index) => {
                out.push(7);
                push_u2(&mut out, *name_index);
            }
            CpEntry::String(utf8_index) => {
                out.push(8);
                push_u2(&mut out, *utf8_index);
            }
            CpEntry::NameAndType(name, descriptor) => {
                out.push(12);
                push_u2(&mut out, *name);
                push_u2(&mut out, *descriptor);
            }
            CpEntry::Methodref(class, nat) => {
                out.push(10);
                push_u2(&mut out, *class);
                push_u2(&mut out, *nat);
            }
            CpEntry::MethodHandle(kind, reference) => {
                out.push(15);
                out.push(*kind);
                push_u2(&mut out, *reference);
            }
            CpEntry::MethodType(descriptor) => {
                out.push(16);
                push_u2(&mut out, *descriptor);
            }
            CpEntry::InvokeDynamic(bootstrap, nat) => {
                out.push(18);
                push_u2(&mut out, *bootstrap);
                push_u2(&mut out, *nat);
            }
        }
    }

    push_u2(&mut out, 0x0021);
    push_u2(&mut out, this_class);
    push_u2(&mut out, super_class);
    push_u2(&mut out, 0);
    push_u2(&mut out, 0);
    push_u2(&mut out, u16::try_from(methods.len()).unwrap());
    for (method, (name_index, descriptor_index)) in methods.iter().zip(method_indexes) {
        push_u2(&mut out, method.access);
        push_u2(&mut out, name_index);
        push_u2(&mut out, descriptor_index);
        push_u2(&mut out, 1);
        push_u2(&mut out, code_attribute);
        let code_len = 2 + 2 + 4 + method.code.len() + 2 + 2;
        push_u4(&mut out, u32::try_from(code_len).unwrap());
        push_u2(&mut out, method.max_stack);
        push_u2(&mut out, method.max_locals);
        push_u4(&mut out, u32::try_from(method.code.len()).unwrap());
        out.extend_from_slice(&method.code);
        push_u2(&mut out, 0);
        push_u2(&mut out, 0);
    }

    if bootstrap_methods.is_empty() {
        push_u2(&mut out, 0);
    } else {
        push_u2(&mut out, 1);
        push_u2(&mut out, bootstrap_attribute);
        let payload_len = 2usize
            + bootstrap_methods
                .iter()
                .map(|(_, args)| 4 + args.len() * 2)
                .sum::<usize>();
        push_u4(&mut out, u32::try_from(payload_len).unwrap());
        push_u2(&mut out, u16::try_from(bootstrap_methods.len()).unwrap());
        for (handle, args) in bootstrap_methods {
            push_u2(&mut out, handle);
            push_u2(&mut out, u16::try_from(args.len()).unwrap());
            for arg in args {
                push_u2(&mut out, arg);
            }
        }
    }
    out
}

fn cp_index(index: u16) -> [u8; 2] {
    index.to_be_bytes()
}

fn invoke_static(index: u16, code: &mut Vec<u8>) {
    code.push(0xB8);
    code.extend_from_slice(&cp_index(index));
}

fn invoke_virtual(index: u16, code: &mut Vec<u8>) {
    code.push(0xB6);
    code.extend_from_slice(&cp_index(index));
}

fn invoke_special(index: u16, code: &mut Vec<u8>) {
    code.push(0xB7);
    code.extend_from_slice(&cp_index(index));
}

fn ldc(index: u16, code: &mut Vec<u8>) {
    if index <= u16::from(u8::MAX) {
        code.push(0x12);
        code.push(u8::try_from(index).unwrap());
    } else {
        code.push(0x13);
        code.extend_from_slice(&cp_index(index));
    }
}

fn budget() -> ExtractionBudget {
    ExtractionBudget::default()
}

fn mojang_block_class() -> Vec<u8> {
    let properties = "net/minecraft/world/level/block/state/BlockBehaviour$Properties";
    let mut cp = ConstantPoolBuilder::default();
    let register = cp.method_ref("example/Registrar", "register", "(Ljava/lang/String;)V");
    let of = cp.method_ref(
        properties,
        "of",
        "()Lnet/minecraft/world/level/block/state/BlockBehaviour$Properties;",
    );
    let strength = cp.method_ref(
        properties,
        "strength",
        "(FF)Lnet/minecraft/world/level/block/state/BlockBehaviour$Properties;",
    );
    let requires = cp.method_ref(
        properties,
        "requiresCorrectToolForDrops",
        "()Lnet/minecraft/world/level/block/state/BlockBehaviour$Properties;",
    );
    let no_occlusion = cp.method_ref(
        properties,
        "noOcclusion",
        "()Lnet/minecraft/world/level/block/state/BlockBehaviour$Properties;",
    );
    let friction = cp.method_ref(
        properties,
        "friction",
        "(F)Lnet/minecraft/world/level/block/state/BlockBehaviour$Properties;",
    );
    let name = cp.string("example_block");
    let destroy_time = cp.float(3.5);
    let friction_value = cp.float(0.98);

    let mut code = Vec::new();
    ldc(name, &mut code);
    invoke_static(register, &mut code);
    invoke_static(of, &mut code);
    code.push(0x4B);
    ldc(destroy_time, &mut code);
    code.push(0x44);
    code.push(0x2A);
    code.push(0x23);
    code.extend_from_slice(&[0x10, 6, 0x86]);
    invoke_virtual(strength, &mut code);
    invoke_virtual(requires, &mut code);
    invoke_virtual(no_occlusion, &mut code);
    ldc(friction_value, &mut code);
    invoke_virtual(friction, &mut code);
    code.push(0x57);
    code.push(0xB1);

    assemble_class(
        "example/ModBlocks",
        &mut cp,
        vec![MethodSpec {
            access: 0x0009,
            name: "register".into(),
            descriptor: "()V".into(),
            max_stack: 8,
            max_locals: 2,
            code,
        }],
        Vec::new(),
    )
}

#[test]
fn extracts_mojang_block_properties_builder_chain() {
    let report = extract_class_properties(&mojang_block_class(), &budget()).unwrap();
    assert_eq!(report.class_name, "example/ModBlocks");
    assert_eq!(report.blocks.len(), 1);
    let block = &report.blocks[0];
    assert_eq!(block.registration_name.as_deref(), Some("example_block"));
    assert_eq!(block.key, "example_block");
    assert_eq!(block.properties.destroy_time, Some(3.5));
    assert_eq!(block.properties.explosion_resistance, Some(6.0));
    assert_eq!(block.properties.requires_correct_tool, Some(true));
    assert_eq!(block.properties.no_occlusion, Some(true));
    assert_eq!(block.properties.friction, Some(0.98));
    assert_eq!(report.stats.blocks_extracted, 1);
    assert_eq!(report.stats.unresolved_dynamic_calls, 0);
}

fn fabric_lambda_block_class() -> Vec<u8> {
    fabric_lambda_class(true)
}

fn fabric_lambda_class(include_lambda_bootstrap: bool) -> Vec<u8> {
    let class_name = "example/FabricBlocks";
    let settings = "net/minecraft/block/AbstractBlock$Settings";
    let lambda_name = "lambda$register$0";
    let lambda_descriptor = "(Lnet/minecraft/block/BlockState;)I";
    let mut cp = ConstantPoolBuilder::default();
    let create = cp.method_ref(
        settings,
        "create",
        "()Lnet/minecraft/block/AbstractBlock$Settings;",
    );
    let strength = cp.method_ref(
        settings,
        "strength",
        "(F)Lnet/minecraft/block/AbstractBlock$Settings;",
    );
    let luminance = cp.method_ref(
        settings,
        "luminance",
        "(Ljava/util/function/ToIntFunction;)Lnet/minecraft/block/AbstractBlock$Settings;",
    );
    let lambda_ref = cp.method_ref(class_name, lambda_name, lambda_descriptor);
    let lambda_handle = cp.method_handle(6, lambda_ref);
    let metafactory_ref = cp.method_ref(
        "java/lang/invoke/LambdaMetafactory",
        "metafactory",
        "(Ljava/lang/invoke/MethodHandles$Lookup;Ljava/lang/String;Ljava/lang/invoke/MethodType;Ljava/lang/invoke/MethodType;Ljava/lang/invoke/MethodHandle;Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/CallSite;",
    );
    let metafactory_handle = cp.method_handle(6, metafactory_ref);
    let sam_type = cp.method_type("(Lnet/minecraft/block/BlockState;)I");
    let instantiated_type = cp.method_type("(Lnet/minecraft/block/BlockState;)I");
    let indy = cp.invoke_dynamic(0, "applyAsInt", "()Ljava/util/function/ToIntFunction;");
    let strength_value = cp.float(2.0);
    let fifteen = cp.integer(15);

    let mut code = Vec::new();
    invoke_static(create, &mut code);
    ldc(strength_value, &mut code);
    invoke_virtual(strength, &mut code);
    code.push(0xBA);
    code.extend_from_slice(&cp_index(indy));
    code.extend_from_slice(&[0, 0]);
    invoke_virtual(luminance, &mut code);
    code.push(0x57);
    code.push(0xB1);
    let mut lambda_code = Vec::new();
    ldc(fifteen, &mut lambda_code);
    lambda_code.push(0xAC);

    assemble_class(
        class_name,
        &mut cp,
        if include_lambda_bootstrap {
            vec![
                MethodSpec {
                    access: 0x0009,
                    name: "register".into(),
                    descriptor: "()V".into(),
                    max_stack: 8,
                    max_locals: 0,
                    code,
                },
                MethodSpec {
                    access: 0x100A,
                    name: lambda_name.into(),
                    descriptor: lambda_descriptor.into(),
                    max_stack: 1,
                    max_locals: 1,
                    code: lambda_code,
                },
            ]
        } else {
            vec![MethodSpec {
                access: 0x0009,
                name: "register".into(),
                descriptor: "()V".into(),
                max_stack: 8,
                max_locals: 0,
                code,
            }]
        },
        if include_lambda_bootstrap {
            vec![(
                metafactory_handle,
                vec![sam_type, lambda_handle, instantiated_type],
            )]
        } else {
            Vec::new()
        },
    )
}

#[test]
fn extracts_fabric_settings_and_constant_return_light_lambda() {
    let report = extract_class_properties(&fabric_lambda_block_class(), &budget()).unwrap();
    assert_eq!(report.blocks.len(), 1);
    let block = &report.blocks[0];
    assert_eq!(block.properties.destroy_time, Some(2.0));
    assert_eq!(block.properties.explosion_resistance, Some(2.0));
    assert_eq!(block.properties.light_level, Some(15));
    assert_eq!(report.stats.unresolved_dynamic_calls, 0);
}

#[test]
fn unresolved_invokedynamic_is_counted_not_treated_as_constant() {
    let report = extract_class_properties(&fabric_lambda_class(false), &budget()).unwrap();
    assert_eq!(report.blocks.len(), 1);
    assert_eq!(report.blocks[0].properties.light_level, None);
    assert_eq!(report.stats.unresolved_dynamic_calls, 1);
}

fn item_food_class() -> Vec<u8> {
    let item_properties = "net/minecraft/world/item/Item$Properties";
    let food_builder = "net/minecraft/world/food/FoodProperties$Builder";
    let mut cp = ConstantPoolBuilder::default();
    let item_class = cp.class(item_properties);
    let item_init = cp.method_ref(item_properties, "<init>", "()V");
    let stacks_to = cp.method_ref(
        item_properties,
        "stacksTo",
        "(I)Lnet/minecraft/world/item/Item$Properties;",
    );
    let fire_resistant = cp.method_ref(
        item_properties,
        "fireResistant",
        "()Lnet/minecraft/world/item/Item$Properties;",
    );
    let durability = cp.method_ref(
        item_properties,
        "durability",
        "(I)Lnet/minecraft/world/item/Item$Properties;",
    );
    let burn_time = cp.method_ref(
        item_properties,
        "burnTime",
        "(I)Lnet/minecraft/world/item/Item$Properties;",
    );
    let food = cp.method_ref(
        item_properties,
        "food",
        "(Lnet/minecraft/world/food/FoodProperties;)Lnet/minecraft/world/item/Item$Properties;",
    );
    let food_class = cp.class(food_builder);
    let food_init = cp.method_ref(food_builder, "<init>", "()V");
    let nutrition = cp.method_ref(
        food_builder,
        "nutrition",
        "(I)Lnet/minecraft/world/food/FoodProperties$Builder;",
    );
    let saturation = cp.method_ref(
        food_builder,
        "saturationModifier",
        "(F)Lnet/minecraft/world/food/FoodProperties$Builder;",
    );
    let build = cp.method_ref(
        food_builder,
        "build",
        "()Lnet/minecraft/world/food/FoodProperties;",
    );
    let saturation_value = cp.float(0.8);
    let sixteen = cp.float(16.0);

    let mut code = Vec::new();
    ldc(sixteen, &mut code);
    code.extend_from_slice(&[0x8B, 0x3C]);
    code.push(0xBB);
    code.extend_from_slice(&cp_index(item_class));
    code.push(0x59);
    invoke_special(item_init, &mut code);
    code.push(0x1B);
    invoke_virtual(stacks_to, &mut code);
    invoke_virtual(fire_resistant, &mut code);
    code.extend_from_slice(&[0x10, 64]);
    invoke_virtual(durability, &mut code);
    code.extend_from_slice(&[0x11, 0x00, 0xC8]);
    invoke_virtual(burn_time, &mut code);
    code.push(0xBB);
    code.extend_from_slice(&cp_index(food_class));
    code.push(0x59);
    invoke_special(food_init, &mut code);
    code.extend_from_slice(&[0x10, 6]);
    invoke_virtual(nutrition, &mut code);
    ldc(saturation_value, &mut code);
    invoke_virtual(saturation, &mut code);
    invoke_virtual(build, &mut code);
    invoke_virtual(food, &mut code);
    code.push(0x57);
    code.push(0xB1);

    assemble_class(
        "example/ModItems",
        &mut cp,
        vec![MethodSpec {
            access: 0x0009,
            name: "register".into(),
            descriptor: "()V".into(),
            max_stack: 8,
            max_locals: 2,
            code,
        }],
        Vec::new(),
    )
}

#[test]
fn extracts_item_and_nested_food_properties() {
    let report = extract_class_properties(&item_food_class(), &budget()).unwrap();
    assert_eq!(report.items.len(), 1);
    let item = &report.items[0];
    assert_eq!(item.properties.stacks_to, Some(16));
    assert_eq!(item.properties.fire_resistant, Some(true));
    assert_eq!(item.properties.durability, Some(64));
    assert_eq!(item.properties.burn_time, Some(200));
    let food = item.properties.food.as_ref().expect("food properties");
    assert_eq!(food.nutrition, Some(6));
    assert_eq!(food.saturation_modifier, Some(0.8));
    assert_eq!(report.stats.items_extracted, 1);
}

fn simple_nop_class(instructions: usize) -> Vec<u8> {
    let mut cp = ConstantPoolBuilder::default();
    let mut code = vec![0x00; instructions];
    code.push(0xB1);
    assemble_class(
        "example/Nops",
        &mut cp,
        vec![MethodSpec {
            access: 0x0009,
            name: "run".into(),
            descriptor: "()V".into(),
            max_stack: 1,
            max_locals: 0,
            code,
        }],
        Vec::new(),
    )
}

#[test]
fn corrupted_class_and_budget_excess_fail_closed_without_panics() {
    assert!(extract_class_properties(&[0xCA, 0xFE, 0xBA, 0xBE], &budget()).is_err());

    let tight = ExtractionBudget {
        max_instructions_per_method: 2,
        ..ExtractionBudget::default()
    };
    let error = extract_class_properties(&simple_nop_class(8), &tight).unwrap_err();
    assert!(matches!(
        error,
        ExtractorError::BudgetExceeded {
            resource: "instructions",
            ..
        }
    ));
}

fn push_u2le(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn push_u4le(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn zip_stored(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut central = Vec::new();
    for (name, payload) in entries {
        let offset = u32::try_from(out.len()).unwrap();
        let crc = megu_core::hash::crc32_ieee(payload);
        let size = u32::try_from(payload.len()).unwrap();
        out.extend_from_slice(b"PK\x03\x04");
        push_u2le(&mut out, 20);
        push_u2le(&mut out, 0);
        push_u2le(&mut out, 0);
        push_u2le(&mut out, 0);
        push_u2le(&mut out, 0);
        push_u4le(&mut out, crc);
        push_u4le(&mut out, size);
        push_u4le(&mut out, size);
        push_u2le(&mut out, u16::try_from(name.len()).unwrap());
        push_u2le(&mut out, 0);
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(payload);

        central.extend_from_slice(b"PK\x01\x02");
        push_u2le(&mut central, 20);
        push_u2le(&mut central, 20);
        push_u2le(&mut central, 0);
        push_u2le(&mut central, 0);
        push_u2le(&mut central, 0);
        push_u2le(&mut central, 0);
        push_u4le(&mut central, crc);
        push_u4le(&mut central, size);
        push_u4le(&mut central, size);
        push_u2le(&mut central, u16::try_from(name.len()).unwrap());
        push_u2le(&mut central, 0);
        push_u2le(&mut central, 0);
        push_u2le(&mut central, 0);
        push_u2le(&mut central, 0);
        push_u4le(&mut central, 0);
        push_u4le(&mut central, offset);
        central.extend_from_slice(name.as_bytes());
    }
    let central_offset = u32::try_from(out.len()).unwrap();
    let central_size = u32::try_from(central.len()).unwrap();
    out.extend_from_slice(&central);
    out.extend_from_slice(b"PK\x05\x06");
    push_u2le(&mut out, 0);
    push_u2le(&mut out, 0);
    push_u2le(&mut out, u16::try_from(entries.len()).unwrap());
    push_u2le(&mut out, u16::try_from(entries.len()).unwrap());
    push_u4le(&mut out, central_size);
    push_u4le(&mut out, central_offset);
    push_u2le(&mut out, 0);
    out
}

#[test]
fn extracts_properties_from_synthetic_in_memory_jar() {
    let class = mojang_block_class();
    let jar = zip_stored(&[
        (
            "META-INF/MANIFEST.MF",
            b"Manifest-Version: 1.0\n".as_slice(),
        ),
        ("example/ModBlocks.class", class.as_slice()),
    ]);
    let report = extract_jar_properties(&jar, &budget()).unwrap();
    assert_eq!(report.stats.classes_scanned, 1);
    assert_eq!(report.stats.blocks_extracted, 1);
    assert_eq!(report.classes.len(), 1);
    assert_eq!(
        report.classes[0].blocks[0].properties.destroy_time,
        Some(3.5)
    );
}

// ---------------------------------------------------------------------------
// Wave 7A identity-binding regressions
// ---------------------------------------------------------------------------

fn fabric_copyof_identifier_identity_class() -> Vec<u8> {
    let class_name = "example/FabricIdentityBlocks";
    let settings = "net/fabricmc/fabric/api/object/builder/v1/block/FabricBlockSettings";
    let block = "net/minecraft/block/Block";
    let identifier = "net/minecraft/util/Identifier";
    let mut cp = ConstantPoolBuilder::default();
    let identifier_of = cp.method_ref(
        identifier,
        "of",
        "(Ljava/lang/String;Ljava/lang/String;)Lnet/minecraft/util/Identifier;",
    );
    let settings_of = cp.method_ref(
        settings,
        "of",
        "(Lnet/minecraft/block/Material;)Lnet/fabricmc/fabric/api/object/builder/v1/block/FabricBlockSettings;",
    );
    let strength = cp.method_ref(
        settings,
        "strength",
        "(F)Lnet/fabricmc/fabric/api/object/builder/v1/block/FabricBlockSettings;",
    );
    let break_instantly = cp.method_ref(
        settings,
        "breakInstantly",
        "()Lnet/fabricmc/fabric/api/object/builder/v1/block/FabricBlockSettings;",
    );
    let copy_of = cp.method_ref(
        settings,
        "copyOf",
        "(Lnet/fabricmc/fabric/api/object/builder/v1/block/FabricBlockSettings;)Lnet/fabricmc/fabric/api/object/builder/v1/block/FabricBlockSettings;",
    );
    let block_init = cp.method_ref(
        block,
        "<init>",
        "(Lnet/fabricmc/fabric/api/object/builder/v1/block/FabricBlockSettings;)V",
    );
    let register = cp.method_ref(
        "net/minecraft/registry/Registry",
        "register",
        "(Lnet/minecraft/registry/Registry;Lnet/minecraft/util/Identifier;Ljava/lang/Object;)Ljava/lang/Object;",
    );
    let block_class = cp.class(block);
    let namespace = cp.string("example");
    let path = cp.string("copy_block");
    let hardness = cp.float(4.25);

    let mut code = Vec::new();
    // Registry.register(null, Identifier.of("example", "copy_block"), new Block(...))
    code.push(0x01); // aconst_null registry object
    ldc(namespace, &mut code);
    ldc(path, &mut code);
    invoke_static(identifier_of, &mut code);
    code.push(0xBB); // new Block
    code.extend_from_slice(&cp_index(block_class));
    code.push(0x59); // dup
    code.push(0x01); // Material argument (unknown but not a scalar property)
    invoke_static(settings_of, &mut code);
    ldc(hardness, &mut code);
    invoke_virtual(strength, &mut code);
    invoke_virtual(break_instantly, &mut code);
    invoke_static(copy_of, &mut code);
    invoke_special(block_init, &mut code);
    invoke_static(register, &mut code);
    code.push(0x57); // pop returned registry object
    code.push(0xB1);

    assemble_class(
        class_name,
        &mut cp,
        vec![MethodSpec {
            access: 0x0009,
            name: "register".into(),
            descriptor: "()V".into(),
            max_stack: 8,
            max_locals: 0,
            code,
        }],
        Vec::new(),
    )
}

#[test]
fn fabric_copyof_of_material_identifier_of_and_break_instantly_bind_by_object_identity() {
    let report = extract_class_properties(&fabric_copyof_identifier_identity_class(), &budget()).unwrap();
    assert_eq!(report.blocks.len(), 1, "{report:#?}");
    let block = &report.blocks[0];
    assert_eq!(block.registration_name.as_deref(), Some("example:copy_block"));
    assert_eq!(block.key, "example:copy_block");
    assert_eq!(block.properties.destroy_time, Some(0.0));
    assert_eq!(block.properties.explosion_resistance, Some(4.25));
    assert_eq!(block.properties.instabreak, Some(true));
}

fn deferred_register_lambda_identity_class() -> Vec<u8> {
    let class_name = "example/DeferredIdentityBlocks";
    let settings = "net/minecraft/world/level/block/state/BlockBehaviour$Properties";
    let block = "net/minecraft/world/level/block/Block";
    let lambda_name = "lambda$register$0";
    let lambda_descriptor = "()Ljava/lang/Object;";
    let mut cp = ConstantPoolBuilder::default();
    let create = cp.method_ref(
        settings,
        "of",
        "()Lnet/minecraft/world/level/block/state/BlockBehaviour$Properties;",
    );
    let strength = cp.method_ref(
        settings,
        "strength",
        "(F)Lnet/minecraft/world/level/block/state/BlockBehaviour$Properties;",
    );
    let block_init = cp.method_ref(
        block,
        "<init>",
        "(Lnet/minecraft/world/level/block/state/BlockBehaviour$Properties;)V",
    );
    let register = cp.method_ref(
        "net/neoforged/neoforge/registries/DeferredRegister",
        "register",
        "(Ljava/lang/String;Ljava/util/function/Supplier;)Lnet/neoforged/neoforge/registries/DeferredHolder;",
    );
    let block_class = cp.class(block);
    let lambda_ref = cp.method_ref(class_name, lambda_name, lambda_descriptor);
    let lambda_handle = cp.method_handle(6, lambda_ref);
    let metafactory_ref = cp.method_ref(
        "java/lang/invoke/LambdaMetafactory",
        "metafactory",
        "(Ljava/lang/invoke/MethodHandles$Lookup;Ljava/lang/String;Ljava/lang/invoke/MethodType;Ljava/lang/invoke/MethodType;Ljava/lang/invoke/MethodHandle;Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/CallSite;",
    );
    let metafactory_handle = cp.method_handle(6, metafactory_ref);
    let sam_type = cp.method_type("()Ljava/lang/Object;");
    let instantiated_type = cp.method_type("()Ljava/lang/Object;");
    let indy = cp.invoke_dynamic(0, "get", "()Ljava/util/function/Supplier;");
    let registration = cp.string("deferred_block");
    let hardness = cp.float(7.5);

    let mut code = Vec::new();
    code.push(0x01); // DeferredRegister receiver (identity irrelevant here)
    ldc(registration, &mut code);
    code.push(0xBA);
    code.extend_from_slice(&cp_index(indy));
    code.extend_from_slice(&[0, 0]);
    invoke_virtual(register, &mut code);
    code.push(0x57);
    code.push(0xB1);

    let mut lambda_code = Vec::new();
    lambda_code.push(0xBB);
    lambda_code.extend_from_slice(&cp_index(block_class));
    lambda_code.push(0x59);
    invoke_static(create, &mut lambda_code);
    ldc(hardness, &mut lambda_code);
    invoke_virtual(strength, &mut lambda_code);
    invoke_special(block_init, &mut lambda_code);
    lambda_code.push(0xB0); // areturn

    assemble_class(
        class_name,
        &mut cp,
        vec![
            MethodSpec {
                access: 0x0009,
                name: "register".into(),
                descriptor: "()V".into(),
                max_stack: 6,
                max_locals: 0,
                code,
            },
            MethodSpec {
                access: 0x100A,
                name: lambda_name.into(),
                descriptor: lambda_descriptor.into(),
                max_stack: 5,
                max_locals: 0,
                code: lambda_code,
            },
        ],
        vec![(metafactory_handle, vec![sam_type, lambda_handle, instantiated_type])],
    )
}

#[test]
fn deferred_register_binds_supplier_lambda_identity_not_positional_order() {
    let report = extract_class_properties(&deferred_register_lambda_identity_class(), &budget()).unwrap();
    assert_eq!(report.blocks.len(), 1, "{report:#?}");
    let block = &report.blocks[0];
    assert_eq!(block.registration_name.as_deref(), Some("deferred_block"));
    assert_eq!(block.key, "deferred_block");
    assert_eq!(block.properties.destroy_time, Some(7.5));
    assert_eq!(block.source_method, "lambda$register$0");
}
