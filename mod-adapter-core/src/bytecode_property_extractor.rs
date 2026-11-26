// SPDX-License-Identifier: LGPL-3.0-or-later

//! Bounded static extraction of block/item/food property-builder chains from
//! JVM `.class` bytecode and in-memory JAR archives.
//!
//! The extractor reuses [`crate::bytecode_subset_vm::parse_class_methods`] for
//! the JVMS §4 structure and constant pool, then runs a constant-propagating
//! abstract interpreter over method bytecode. It never launches a JVM, never
//! writes archive members to disk, and every scan is bounded by an explicit
//! [`ExtractionBudget`].

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fmt;

use crate::bytecode_subset_vm::{
    parse_class_methods, LambdaImplementation, LdcConst, ParsedClassMethods, ParsedMethod, VmError,
};
use crate::modpack::{read_zip_member, zip_member_list};

/// Hard bounds for one extraction run. Every exceeding input is an error
/// rather than a silently partial extraction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExtractionBudget {
    /// Maximum `.class` members scanned in one archive.
    pub max_classes: usize,
    /// Maximum methods interpreted in one class.
    pub max_methods_per_class: usize,
    /// Maximum abstractly executed instructions in one method.
    pub max_instructions_per_method: usize,
    /// Maximum input archive bytes; the same cap bounds cumulative
    /// decompressed `.class` bytes.
    pub max_archive_bytes: u64,
}

impl Default for ExtractionBudget {
    fn default() -> Self {
        Self {
            max_classes: 2048,
            max_methods_per_class: 512,
            max_instructions_per_method: 20_000,
            max_archive_bytes: 256 * 1024 * 1024,
        }
    }
}

/// Why class/JAR property extraction failed.
#[derive(Clone, Debug, PartialEq)]
pub enum ExtractorError {
    /// The shared JVMS §4 parser rejected the class.
    ClassParse(VmError),
    /// A configured extraction budget was exceeded.
    BudgetExceeded {
        /// Bounded resource (`classes`, `methods`, `instructions`, or `archive_bytes`).
        resource: &'static str,
        /// Configured limit.
        limit: usize,
    },
    /// The input archive itself is larger than [`ExtractionBudget::max_archive_bytes`].
    ArchiveTooLarge {
        /// Input or cumulative decompressed size.
        size: u64,
        /// Configured cap.
        max: u64,
    },
    /// The archive reader rejected the JAR/ZIP structure or a member.
    Zip(String),
    /// Bytecode is structurally invalid in a way the class parser cannot see.
    Malformed(&'static str),
    /// A multi-byte instruction crosses the end of the code array.
    TruncatedInstruction {
        /// Byte offset of the instruction.
        offset: usize,
    },
    /// Verified Java bytecode cannot underflow, so this means malformed input.
    StackUnderflow {
        /// Byte offset of the instruction.
        offset: usize,
    },
}

impl fmt::Display for ExtractorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ClassParse(error) => write!(f, "class parse failed: {error}"),
            Self::BudgetExceeded { resource, limit } => {
                write!(
                    f,
                    "extraction budget exceeded for {resource} (limit {limit})"
                )
            }
            Self::ArchiveTooLarge { size, max } => {
                write!(f, "archive byte budget exceeded: {size} > {max}")
            }
            Self::Zip(message) => f.write_str(message),
            Self::Malformed(what) => write!(f, "malformed bytecode: {what}"),
            Self::TruncatedInstruction { offset } => {
                write!(f, "instruction at offset {offset} is truncated")
            }
            Self::StackUnderflow { offset } => {
                write!(f, "operand stack underflow at offset {offset}")
            }
        }
    }
}

impl std::error::Error for ExtractorError {}

impl From<VmError> for ExtractorError {
    fn from(error: VmError) -> Self {
        Self::ClassParse(error)
    }
}

/// Extracted `BlockBehaviour.Properties` / `AbstractBlock.Settings` values.
/// `None` means the bytecode did not statically establish that property.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ExtractedBlockProperties {
    /// `destroyTime`/`hardness` (the first argument of two-argument `strength`).
    pub destroy_time: Option<f32>,
    /// `explosionResistance`/`resistance` (the second argument of `strength`).
    pub explosion_resistance: Option<f32>,
    /// Constant light emission, including constant-return light lambdas.
    pub light_level: Option<i32>,
    /// `requiresCorrectToolForDrops`/`requiresTool`.
    pub requires_correct_tool: Option<bool>,
    /// `noOcclusion`/`nonOpaque`.
    pub no_occlusion: Option<bool>,
    /// `noCollission`/`noCollision`.
    pub no_collision: Option<bool>,
    /// `friction`/`slipperiness`.
    pub friction: Option<f32>,
    /// `instabreak` was invoked; `destroy_time` is also set to zero when seen.
    pub instabreak: Option<bool>,
    /// `air` was invoked.
    pub air: Option<bool>,
}

/// Extracted `FoodProperties`/`FoodComponent` values.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ExtractedFoodProperties {
    /// `nutrition`/`hunger`.
    pub nutrition: Option<i32>,
    /// `saturationModifier`/`saturation`.
    pub saturation_modifier: Option<f32>,
    /// `alwaysEdible`/`alwaysEat`.
    pub always_edible: Option<bool>,
    /// `fast`/`snack`.
    pub fast: Option<bool>,
}

/// Extracted `Item.Properties`/`Item.Settings` values.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ExtractedItemProperties {
    /// `stacksTo`/`maxCount`.
    pub stacks_to: Option<i32>,
    /// `durability`/`maxDamage`.
    pub durability: Option<i32>,
    /// `fireResistant`/`fireproof`.
    pub fire_resistant: Option<bool>,
    /// Nested food properties passed to `food(...)`.
    pub food: Option<ExtractedFoodProperties>,
    /// `fuel`/`burnTime` when a mod or loader exposes it on the item builder.
    pub burn_time: Option<i32>,
}

/// One extracted block property record.
#[derive(Clone, Debug, PartialEq)]
pub struct BlockPropertyRecord {
    /// Stable record key: the registration id when visible, otherwise a
    /// deterministic class/method/builder key.
    pub key: String,
    /// Registration id visible in a `register...` call in the same method.
    pub registration_name: Option<String>,
    /// Method whose bytecode produced the builder.
    pub source_method: String,
    /// Extracted properties.
    pub properties: ExtractedBlockProperties,
}

/// One extracted item property record.
#[derive(Clone, Debug, PartialEq)]
pub struct ItemPropertyRecord {
    /// Stable record key: the registration id when visible, otherwise a
    /// deterministic class/method/builder key.
    pub key: String,
    /// Registration id visible in a `register...` call in the same method.
    pub registration_name: Option<String>,
    /// Method whose bytecode produced the builder.
    pub source_method: String,
    /// Extracted properties.
    pub properties: ExtractedItemProperties,
}

/// Extraction telemetry. Dynamic calls that could not be resolved are counted
/// explicitly rather than treated as successfully understood.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExtractionStats {
    /// `.class` files structurally parsed.
    pub classes_scanned: usize,
    /// Methods whose bytecode was abstractly interpreted.
    pub methods_analyzed: usize,
    /// Touched block builders emitted.
    pub blocks_extracted: usize,
    /// Touched item builders emitted.
    pub items_extracted: usize,
    /// `invokedynamic` sites or lambda bodies that could not be constant-folded.
    pub unresolved_dynamic_calls: usize,
}

impl ExtractionStats {
    fn merge(&mut self, other: &Self) {
        self.classes_scanned += other.classes_scanned;
        self.methods_analyzed += other.methods_analyzed;
        self.blocks_extracted += other.blocks_extracted;
        self.items_extracted += other.items_extracted;
        self.unresolved_dynamic_calls += other.unresolved_dynamic_calls;
    }
}

/// Property records extracted from one class.
#[derive(Clone, Debug, PartialEq)]
pub struct ClassPropertyReport {
    /// JVM internal class name.
    pub class_name: String,
    /// Extracted block builders.
    pub blocks: Vec<BlockPropertyRecord>,
    /// Extracted item builders.
    pub items: Vec<ItemPropertyRecord>,
    /// Per-class telemetry.
    pub stats: ExtractionStats,
}

/// Property records extracted from every class in one in-memory JAR.
#[derive(Clone, Debug, PartialEq)]
pub struct JarPropertyReport {
    /// Per-class reports in deterministic archive central-directory order.
    pub classes: Vec<ClassPropertyReport>,
    /// Aggregate telemetry.
    pub stats: ExtractionStats,
}

#[derive(Clone, Debug, PartialEq)]
struct BlockBuilderValue {
    id: u32,
    properties: ExtractedBlockProperties,
    touched: bool,
}

#[derive(Clone, Debug, PartialEq)]
struct ItemBuilderValue {
    id: u32,
    properties: ExtractedItemProperties,
    touched: bool,
}

#[derive(Clone, Debug, PartialEq)]
struct FoodBuilderValue {
    id: u32,
    properties: ExtractedFoodProperties,
    touched: bool,
}

#[derive(Clone, Debug, PartialEq)]
struct ObjectValue {
    class_name: String,
    id: u32,
}

#[derive(Clone, Debug, PartialEq)]
enum AbstractValue {
    Int(i32),
    Float(f32),
    Bool(bool),
    Str(String),
    BlockPropertiesBuilder(BlockBuilderValue),
    ItemPropertiesBuilder(ItemBuilderValue),
    FoodPropertiesBuilder(FoodBuilderValue),
    Lambda {
        implementation: LambdaImplementation,
    },
    Object(ObjectValue),
    Unknown,
}

#[derive(Clone, Debug)]
enum FinalBuilder {
    Block(BlockBuilderValue),
    Item(ItemBuilderValue),
}

#[derive(Clone)]
struct AbstractState {
    stack: Vec<AbstractValue>,
    locals: BTreeMap<usize, AbstractValue>,
}

impl AbstractState {
    fn new() -> Self {
        Self {
            stack: Vec::new(),
            locals: BTreeMap::new(),
        }
    }

    fn pop(&mut self, offset: usize) -> Result<AbstractValue, ExtractorError> {
        self.stack
            .pop()
            .ok_or(ExtractorError::StackUnderflow { offset })
    }

    fn push(&mut self, value: AbstractValue) {
        self.stack.push(value);
    }
}

#[derive(Default)]
struct MethodAnalysis {
    final_builders: BTreeMap<u32, FinalBuilder>,
    registration_names: Vec<String>,
    unresolved_dynamic_calls: usize,
}

struct MethodInterpreter<'a> {
    class: &'a ParsedClassMethods,
    method: &'a ParsedMethod,
    budget: &'a ExtractionBudget,
    analysis: MethodAnalysis,
    next_builder_id: u32,
    next_object_id: u32,
    steps: usize,
}

impl<'a> MethodInterpreter<'a> {
    fn new(
        class: &'a ParsedClassMethods,
        method: &'a ParsedMethod,
        budget: &'a ExtractionBudget,
    ) -> Self {
        Self {
            class,
            method,
            budget,
            analysis: MethodAnalysis::default(),
            next_builder_id: 1,
            next_object_id: 1,
            steps: 0,
        }
    }

    fn new_object(&mut self, class_name: String) -> AbstractValue {
        let id = self.next_object_id;
        self.next_object_id = self.next_object_id.saturating_add(1);
        AbstractValue::Object(ObjectValue { class_name, id })
    }

    fn new_block_builder(&mut self) -> AbstractValue {
        let id = self.next_builder_id;
        self.next_builder_id = self.next_builder_id.saturating_add(1);
        let builder = BlockBuilderValue {
            id,
            properties: ExtractedBlockProperties::default(),
            touched: false,
        };
        self.analysis
            .final_builders
            .insert(id, FinalBuilder::Block(builder.clone()));
        AbstractValue::BlockPropertiesBuilder(builder)
    }

    fn new_item_builder(&mut self) -> AbstractValue {
        let id = self.next_builder_id;
        self.next_builder_id = self.next_builder_id.saturating_add(1);
        let builder = ItemBuilderValue {
            id,
            properties: ExtractedItemProperties::default(),
            touched: false,
        };
        self.analysis
            .final_builders
            .insert(id, FinalBuilder::Item(builder.clone()));
        AbstractValue::ItemPropertiesBuilder(builder)
    }

    fn new_food_builder(&mut self) -> AbstractValue {
        let id = self.next_builder_id;
        self.next_builder_id = self.next_builder_id.saturating_add(1);
        let builder = FoodBuilderValue {
            id,
            properties: ExtractedFoodProperties::default(),
            touched: false,
        };
        AbstractValue::FoodPropertiesBuilder(builder)
    }

    fn builder_for_owner(&mut self, owner: &str) -> Option<AbstractValue> {
        if is_block_properties_owner(owner) {
            Some(self.new_block_builder())
        } else if is_item_properties_owner(owner) {
            Some(self.new_item_builder())
        } else if is_food_builder_owner(owner) {
            Some(self.new_food_builder())
        } else {
            None
        }
    }

    fn record_builder(&mut self, value: &AbstractValue) {
        let (id, final_value) = match value {
            AbstractValue::BlockPropertiesBuilder(builder) => {
                (builder.id, FinalBuilder::Block(builder.clone()))
            }
            AbstractValue::ItemPropertiesBuilder(builder) => {
                (builder.id, FinalBuilder::Item(builder.clone()))
            }
            _ => return,
        };
        self.analysis.final_builders.insert(id, final_value);
    }

    fn propagate_builder(&self, state: &mut AbstractState, replacement: &AbstractValue) {
        let replacement_id = builder_id(replacement);
        for value in state
            .stack
            .iter_mut()
            .chain(state.locals.values_mut())
        {
            if builder_id(value) == replacement_id && same_builder_kind(value, replacement) {
                *value = replacement.clone();
            }
        }
    }

    fn replace_object(
        &self,
        state: &mut AbstractState,
        object_id: u32,
        replacement: AbstractValue,
    ) {
        for value in state
            .stack
            .iter_mut()
            .chain(state.locals.values_mut())
        {
            if matches!(value, AbstractValue::Object(object) if object.id == object_id) {
                *value = replacement.clone();
            }
        }
    }

    fn note_registration_strings(&mut self, args: &[AbstractValue]) {
        for arg in args {
            let AbstractValue::Str(candidate) = arg else {
                continue;
            };
            if is_registration_name(candidate)
                && !self.analysis.registration_names.contains(candidate)
            {
                self.analysis.registration_names.push(candidate.clone());
            }
        }
    }

    fn constant_lambda_int(&self, implementation: &LambdaImplementation) -> Option<i32> {
        let method = self.class.methods.iter().find(|method| {
            method.name == implementation.name && method.descriptor == implementation.descriptor
        })?;
        constant_int_return(self.class, method)
    }

    fn mutate_block_builder(
        &mut self,
        mut builder: BlockBuilderValue,
        name: &str,
        args: &[AbstractValue],
    ) -> BlockBuilderValue {
        let lower = name.to_ascii_lowercase();
        match lower.as_str() {
            "strength" => {
                let first = args.first().and_then(as_f32);
                if let Some(first) = first {
                    builder.properties.destroy_time = Some(first);
                    if let Some(second) = args.get(1).and_then(as_f32) {
                        builder.properties.explosion_resistance = Some(second);
                    } else if args.len() == 1 {
                        builder.properties.explosion_resistance = Some(first);
                    }
                    builder.touched = true;
                }
            }
            "hardness" | "destroytime" => {
                if let Some(value) = args.first().and_then(as_f32) {
                    builder.properties.destroy_time = Some(value);
                    builder.touched = true;
                }
            }
            "explosionresistance" | "resistance" => {
                if let Some(value) = args.first().and_then(as_f32) {
                    builder.properties.explosion_resistance = Some(value);
                    builder.touched = true;
                }
            }
            "requirescorrecttoolfordrops" | "requirestool" => {
                builder.properties.requires_correct_tool = Some(true);
                builder.touched = true;
            }
            "lightlevel" | "luminance" => {
                if let Some(arg) = args.first() {
                    match arg {
                        AbstractValue::Int(value) => {
                            builder.properties.light_level = Some(*value);
                            builder.touched = true;
                        }
                        AbstractValue::Float(value) if value.fract() == 0.0 => {
                            builder.properties.light_level = Some(*value as i32);
                            builder.touched = true;
                        }
                        AbstractValue::Lambda { implementation } => {
                            if let Some(value) = self.constant_lambda_int(implementation) {
                                builder.properties.light_level = Some(value);
                                builder.touched = true;
                            } else {
                                self.analysis.unresolved_dynamic_calls += 1;
                            }
                        }
                        _ => {}
                    }
                }
            }
            "noocclusion" | "nonopaque" => {
                builder.properties.no_occlusion = Some(true);
                builder.touched = true;
            }
            "nocollission" | "nocollision" => {
                builder.properties.no_collision = Some(true);
                builder.touched = true;
            }
            "friction" | "slipperiness" => {
                if let Some(value) = args.first().and_then(as_f32) {
                    builder.properties.friction = Some(value);
                    builder.touched = true;
                }
            }
            "instabreak" => {
                builder.properties.instabreak = Some(true);
                builder.properties.destroy_time = Some(0.0);
                builder.touched = true;
            }
            "air" => {
                builder.properties.air = Some(true);
                builder.touched = true;
            }
            _ => {}
        }
        builder
    }

    fn mutate_food_builder(
        &self,
        mut builder: FoodBuilderValue,
        name: &str,
        args: &[AbstractValue],
    ) -> FoodBuilderValue {
        let lower = name.to_ascii_lowercase();
        match lower.as_str() {
            "nutrition" | "hunger" => {
                if let Some(value) = args.first().and_then(as_i32) {
                    builder.properties.nutrition = Some(value);
                    builder.touched = true;
                }
            }
            "saturationmodifier" | "saturation" => {
                if let Some(value) = args.first().and_then(as_f32) {
                    builder.properties.saturation_modifier = Some(value);
                    builder.touched = true;
                }
            }
            "alwaysedible" | "alwayseat" => {
                builder.properties.always_edible = Some(true);
                builder.touched = true;
            }
            "fast" | "snack" => {
                builder.properties.fast = Some(true);
                builder.touched = true;
            }
            _ => {}
        }
        builder
    }

    fn mutate_item_builder(
        &self,
        mut builder: ItemBuilderValue,
        name: &str,
        args: &[AbstractValue],
    ) -> ItemBuilderValue {
        let lower = name.to_ascii_lowercase();
        match lower.as_str() {
            "stacksto" | "maxcount" => {
                if let Some(value) = args.first().and_then(as_i32) {
                    builder.properties.stacks_to = Some(value);
                    builder.touched = true;
                }
            }
            "durability" | "maxdamage" => {
                if let Some(value) = args.first().and_then(as_i32) {
                    builder.properties.durability = Some(value);
                    builder.touched = true;
                }
            }
            "fireresistant" | "fireproof" => {
                builder.properties.fire_resistant = Some(true);
                builder.touched = true;
            }
            "food" => {
                if let Some(AbstractValue::FoodPropertiesBuilder(food)) = args.first() {
                    builder.properties.food = Some(food.properties.clone());
                    builder.touched = true;
                }
            }
            "fuel" | "burntime" | "burn_time" => {
                if let Some(value) = args.first().and_then(as_i32) {
                    builder.properties.burn_time = Some(value);
                    builder.touched = true;
                }
            }
            _ => {}
        }
        builder
    }

    fn execute_invoke(
        &mut self,
        state: &mut AbstractState,
        offset: usize,
        kind: InvokeKind,
        index: u16,
    ) -> Result<(), ExtractorError> {
        let (owner, name, descriptor) = self.class.resolve_method_reference(index)?;
        let (argument_count, returns_value) = parse_method_descriptor(&descriptor)
            .ok_or(ExtractorError::Malformed("bad method descriptor"))?;
        if state.stack.len() < argument_count {
            return Err(ExtractorError::StackUnderflow { offset });
        }
        let args = state.stack.split_off(state.stack.len() - argument_count);
        let receiver = if kind == InvokeKind::Static {
            None
        } else {
            Some(state.pop(offset)?)
        };

        let lower_name = name.to_ascii_lowercase();
        if lower_name == "register" || lower_name.starts_with("register") {
            self.note_registration_strings(&args);
        }

        if name == "<init>" {
            if let Some(replacement) = self.builder_for_owner(&owner) {
                if let Some(AbstractValue::Object(object)) = receiver {
                    self.record_builder(&replacement);
                    self.replace_object(state, object.id, replacement);
                }
            }
            return Ok(());
        }

        if kind == InvokeKind::Static
            && matches!(name.as_str(), "of" | "create")
            && descriptor.starts_with("()")
        {
            if let Some(builder) = self.builder_for_owner(&owner) {
                state.push(builder);
                return Ok(());
            }
        }

        if let Some(receiver) = receiver {
            let mut updated: Option<AbstractValue> = None;
            match receiver {
                AbstractValue::BlockPropertiesBuilder(builder) => {
                    let builder = self.mutate_block_builder(builder, &name, &args);
                    updated = Some(AbstractValue::BlockPropertiesBuilder(builder));
                }
                AbstractValue::ItemPropertiesBuilder(builder) => {
                    let builder = self.mutate_item_builder(builder, &name, &args);
                    updated = Some(AbstractValue::ItemPropertiesBuilder(builder));
                }
                AbstractValue::FoodPropertiesBuilder(builder) => {
                    let builder = self.mutate_food_builder(builder, &name, &args);
                    updated = Some(AbstractValue::FoodPropertiesBuilder(builder));
                }
                _ => {}
            }
            if let Some(value) = updated {
                self.record_builder(&value);
                self.propagate_builder(state, &value);
                if returns_value {
                    state.push(value);
                }
                return Ok(());
            }
        }

        if returns_value {
            state.push(AbstractValue::Unknown);
        }
        Ok(())
    }

    fn execute_invokedynamic(
        &mut self,
        state: &mut AbstractState,
        offset: usize,
        index: u16,
    ) -> Result<(), ExtractorError> {
        let reference = self.class.resolve_invoke_dynamic(index)?;
        let (argument_count, _) = parse_method_descriptor(&reference.descriptor)
            .ok_or(ExtractorError::Malformed("bad invokedynamic descriptor"))?;
        if state.stack.len() < argument_count {
            return Err(ExtractorError::StackUnderflow { offset });
        }
        state.stack.truncate(state.stack.len() - argument_count);
        if let Some(implementation) = reference.lambda_implementation {
            state.push(AbstractValue::Lambda { implementation });
        } else {
            self.analysis.unresolved_dynamic_calls += 1;
            state.push(AbstractValue::Unknown);
        }
        Ok(())
    }

    fn interpret(mut self) -> Result<MethodAnalysis, ExtractorError> {
        let code = &self.method.code;
        let mut worklist = VecDeque::new();
        worklist.push_back((0usize, AbstractState::new()));
        let mut visits: HashMap<usize, u8> = HashMap::new();

        while let Some((pc, mut state)) = worklist.pop_front() {
            if pc >= code.len() {
                continue;
            }
            let visit_count = visits.entry(pc).or_insert(0);
            if *visit_count >= 4 {
                continue;
            }
            *visit_count += 1;
            if self.steps >= self.budget.max_instructions_per_method {
                return Err(ExtractorError::BudgetExceeded {
                    resource: "instructions",
                    limit: self.budget.max_instructions_per_method,
                });
            }
            self.steps += 1;

            let instruction = decode_instruction(code, pc)?;
            let width = instruction_width(&instruction);
            let next_pc = pc
                .checked_add(width)
                .ok_or(ExtractorError::Malformed("pc overflow"))?;
            match instruction {
                RawInstruction::ConstInt { value, .. } => state.push(match value {
                    0 => AbstractValue::Bool(false),
                    1 => AbstractValue::Bool(true),
                    _ => AbstractValue::Int(value),
                }),
                RawInstruction::ConstFloat(value) => state.push(AbstractValue::Float(value)),
                RawInstruction::Ldc { index, .. } => {
                    let value = match self.class.resolve_ldc_constant(index)? {
                        LdcConst::Int(value) => AbstractValue::Int(value),
                        LdcConst::Float(value) => AbstractValue::Float(value),
                        LdcConst::Str(value) => AbstractValue::Str(value),
                    };
                    state.push(value);
                }
                RawInstruction::Load { slot, .. } => {
                    if slot >= usize::from(self.method.max_locals) {
                        return Err(ExtractorError::Malformed("local load out of bounds"));
                    }
                    let value = state
                        .locals
                        .get(&slot)
                        .cloned()
                        .unwrap_or(AbstractValue::Unknown);
                    state.push(value);
                }
                RawInstruction::Store { slot, .. } => {
                    if slot >= usize::from(self.method.max_locals) {
                        return Err(ExtractorError::Malformed("local store out of bounds"));
                    }
                    let value = state.pop(pc)?;
                    state.locals.insert(slot, value);
                }
                RawInstruction::Pop => {
                    state.pop(pc)?;
                }
                RawInstruction::Pop2 => {
                    state.pop(pc)?;
                    if !state.stack.is_empty() {
                        state.pop(pc)?;
                    }
                }
                RawInstruction::Dup => {
                    let value = state
                        .stack
                        .last()
                        .cloned()
                        .ok_or(ExtractorError::StackUnderflow { offset: pc })?;
                    state.push(value);
                }
                RawInstruction::DupX1 => {
                    if state.stack.len() < 2 {
                        return Err(ExtractorError::StackUnderflow { offset: pc });
                    }
                    let top = state.stack[state.stack.len() - 1].clone();
                    state.stack.insert(state.stack.len() - 2, top);
                }
                RawInstruction::DupX2 => {
                    if state.stack.len() < 3 {
                        return Err(ExtractorError::StackUnderflow { offset: pc });
                    }
                    let top = state.stack[state.stack.len() - 1].clone();
                    state.stack.insert(state.stack.len() - 3, top);
                }
                RawInstruction::Swap => {
                    if state.stack.len() < 2 {
                        return Err(ExtractorError::StackUnderflow { offset: pc });
                    }
                    let len = state.stack.len();
                    state.stack.swap(len - 1, len - 2);
                }
                RawInstruction::I2F => {
                    let value = state.pop(pc)?;
                    state.push(match value {
                        AbstractValue::Int(value) => AbstractValue::Float(value as f32),
                        AbstractValue::Bool(value) => {
                            AbstractValue::Float(if value { 1.0 } else { 0.0 })
                        }
                        _ => AbstractValue::Unknown,
                    });
                }
                RawInstruction::F2I => {
                    let value = state.pop(pc)?;
                    state.push(match value {
                        AbstractValue::Float(value) => AbstractValue::Int(value as i32),
                        _ => AbstractValue::Unknown,
                    });
                }
                RawInstruction::New(index) => {
                    let class_name = self.class.resolve_class_name(index)?;
                    state.push(self.new_object(class_name));
                }
                RawInstruction::Invoke { kind, index } => {
                    self.execute_invoke(&mut state, pc, kind, index)?;
                }
                RawInstruction::InvokeDynamic { index } => {
                    self.execute_invokedynamic(&mut state, pc, index)?;
                }
                RawInstruction::Branch {
                    target,
                    pops,
                    unconditional,
                    ..
                } => {
                    for _ in 0..pops {
                        state.pop(pc)?;
                    }
                    let target = branch_target(pc, target)?;
                    if target >= code.len() {
                        return Err(ExtractorError::Malformed("branch target outside code"));
                    }
                    if !unconditional && next_pc < code.len() {
                        worklist.push_back((next_pc, state.clone()));
                    }
                    worklist.push_back((target, state));
                    continue;
                }
                RawInstruction::Switch { default, targets } => {
                    state.pop(pc)?;
                    for target in targets.into_iter().chain(std::iter::once(default)) {
                        let target = branch_target(pc, target)?;
                        if target >= code.len() {
                            return Err(ExtractorError::Malformed("switch target outside code"));
                        }
                        worklist.push_back((target, state.clone()));
                    }
                    continue;
                }
                RawInstruction::Return => continue,
                RawInstruction::Unknown { pops, pushes, .. } => {
                    for _ in 0..pops {
                        state.pop(pc)?;
                    }
                    for _ in 0..pushes {
                        state.push(AbstractValue::Unknown);
                    }
                }
            }
            if next_pc < code.len() {
                worklist.push_back((next_pc, state));
            }
        }

        Ok(self.analysis)
    }
}

fn builder_id(value: &AbstractValue) -> Option<u32> {
    match value {
        AbstractValue::BlockPropertiesBuilder(builder) => Some(builder.id),
        AbstractValue::ItemPropertiesBuilder(builder) => Some(builder.id),
        AbstractValue::FoodPropertiesBuilder(builder) => Some(builder.id),
        _ => None,
    }
}

fn same_builder_kind(a: &AbstractValue, b: &AbstractValue) -> bool {
    matches!(
        (a, b),
        (
            AbstractValue::BlockPropertiesBuilder(_),
            AbstractValue::BlockPropertiesBuilder(_)
        ) | (
            AbstractValue::ItemPropertiesBuilder(_),
            AbstractValue::ItemPropertiesBuilder(_)
        ) | (
            AbstractValue::FoodPropertiesBuilder(_),
            AbstractValue::FoodPropertiesBuilder(_)
        )
    )
}

fn as_i32(value: &AbstractValue) -> Option<i32> {
    match value {
        AbstractValue::Int(value) => Some(*value),
        AbstractValue::Bool(value) => Some(i32::from(*value)),
        AbstractValue::Float(value) if value.fract() == 0.0 => Some(*value as i32),
        _ => None,
    }
}

fn as_f32(value: &AbstractValue) -> Option<f32> {
    match value {
        AbstractValue::Float(value) => Some(*value),
        AbstractValue::Int(value) => Some(*value as f32),
        AbstractValue::Bool(value) => Some(if *value { 1.0 } else { 0.0 }),
        _ => None,
    }
}

fn is_block_properties_owner(owner: &str) -> bool {
    owner.ends_with("BlockBehaviour$Properties")
        || owner.ends_with("AbstractBlock$Settings")
        || owner.ends_with("BlockSettings")
        || owner.ends_with("class_4970$class_2251")
}

fn is_item_properties_owner(owner: &str) -> bool {
    owner.ends_with("Item$Properties")
        || owner.ends_with("Item$Settings")
        || owner.ends_with("class_1792$class_1793")
}

fn is_food_builder_owner(owner: &str) -> bool {
    owner.ends_with("FoodProperties$Builder")
        || owner.ends_with("FoodComponent$Builder")
        || owner.ends_with("class_4174$class_4175")
}

fn is_registration_name(candidate: &str) -> bool {
    !candidate.is_empty()
        && candidate.len() <= 128
        && candidate.bytes().all(|byte| {
            byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || matches!(byte, b'_' | b'-' | b'.' | b'/' | b':')
        })
        && candidate.bytes().any(|byte| byte.is_ascii_lowercase())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InvokeKind {
    Virtual,
    Special,
    Static,
    Interface,
}

#[derive(Clone, Debug)]
enum RawInstruction {
    ConstInt {
        value: i32,
        width: usize,
    },
    ConstFloat(f32),
    Ldc {
        index: u16,
        width: usize,
    },
    Load {
        slot: usize,
        width: usize,
    },
    Store {
        slot: usize,
        width: usize,
    },
    Pop,
    Pop2,
    Dup,
    DupX1,
    DupX2,
    Swap,
    I2F,
    F2I,
    New(u16),
    Invoke {
        kind: InvokeKind,
        index: u16,
    },
    InvokeDynamic {
        index: u16,
    },
    Branch {
        target: i32,
        pops: usize,
        unconditional: bool,
        width: usize,
    },
    Switch {
        default: i32,
        targets: Vec<i32>,
    },
    Return,
    Unknown {
        width: usize,
        pops: usize,
        pushes: usize,
    },
}

fn instruction_width(instruction: &RawInstruction) -> usize {
    match instruction {
        RawInstruction::ConstInt { width, .. }
        | RawInstruction::Ldc { width, .. }
        | RawInstruction::Load { width, .. }
        | RawInstruction::Store { width, .. } => *width,
        RawInstruction::Invoke {
            kind: InvokeKind::Interface,
            ..
        }
        | RawInstruction::InvokeDynamic { .. } => 5,
        RawInstruction::Invoke { .. } | RawInstruction::New(_) => 3,
        RawInstruction::Unknown { width, .. } => *width,
        RawInstruction::Branch { width, .. } => *width,
        RawInstruction::Switch { .. } => 0,
        _ => 1,
    }
}

fn operand_u1(code: &[u8], offset: usize, index: usize) -> Result<u8, ExtractorError> {
    code.get(offset + 1 + index)
        .copied()
        .ok_or(ExtractorError::TruncatedInstruction { offset })
}

fn operand_u2(code: &[u8], offset: usize) -> Result<u16, ExtractorError> {
    let hi = operand_u1(code, offset, 0)?;
    let lo = operand_u1(code, offset, 1)?;
    Ok(u16::from_be_bytes([hi, lo]))
}

fn operand_i2(code: &[u8], offset: usize) -> Result<i16, ExtractorError> {
    Ok(operand_u2(code, offset)? as i16)
}

fn be_i4(bytes: &[u8]) -> i32 {
    i32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

fn decode_instruction(code: &[u8], offset: usize) -> Result<RawInstruction, ExtractorError> {
    let opcode = code[offset];
    let indexed_load_store = |slot: usize, load: bool, width: usize| {
        if load {
            RawInstruction::Load { slot, width }
        } else {
            RawInstruction::Store { slot, width }
        }
    };
    Ok(match opcode {
        0x01 => RawInstruction::Unknown {
            width: 1,
            pops: 0,
            pushes: 1,
        },
        0x02..=0x08 => RawInstruction::ConstInt {
            value: i32::from(opcode) - 3,
            width: 1,
        },
        0x09..=0x0A => RawInstruction::Unknown {
            width: 1,
            pops: 0,
            pushes: 1,
        },
        0x0B..=0x0D => RawInstruction::ConstFloat(f32::from(opcode - 0x0B)),
        0x0E..=0x0F => RawInstruction::Unknown {
            width: 1,
            pops: 0,
            pushes: 1,
        },
        0x10 => RawInstruction::ConstInt {
            value: i32::from(operand_u1(code, offset, 0)? as i8),
            width: 2,
        },
        0x11 => RawInstruction::ConstInt {
            value: i32::from(operand_i2(code, offset)?),
            width: 3,
        },
        0x12 => RawInstruction::Ldc {
            index: u16::from(operand_u1(code, offset, 0)?),
            width: 2,
        },
        0x13 => RawInstruction::Ldc {
            index: operand_u2(code, offset)?,
            width: 3,
        },
        0x14 => RawInstruction::Unknown {
            width: 3,
            pops: 0,
            pushes: 1,
        },
        0x15..=0x19 => indexed_load_store(usize::from(operand_u1(code, offset, 0)?), true, 2),
        0x1A..=0x1D => RawInstruction::Load {
            slot: usize::from(opcode) - 0x1A,
            width: 1,
        },
        0x1E..=0x21 => RawInstruction::Load {
            slot: usize::from(opcode) - 0x1E,
            width: 1,
        },
        0x22..=0x25 => RawInstruction::Load {
            slot: usize::from(opcode) - 0x22,
            width: 1,
        },
        0x26..=0x29 => RawInstruction::Load {
            slot: usize::from(opcode) - 0x26,
            width: 1,
        },
        0x2A..=0x2D => RawInstruction::Load {
            slot: usize::from(opcode) - 0x2A,
            width: 1,
        },
        0x36..=0x3A => indexed_load_store(usize::from(operand_u1(code, offset, 0)?), false, 2),
        0x3B..=0x3E => RawInstruction::Store {
            slot: usize::from(opcode) - 0x3B,
            width: 1,
        },
        0x3F..=0x42 => RawInstruction::Store {
            slot: usize::from(opcode) - 0x3F,
            width: 1,
        },
        0x43..=0x46 => RawInstruction::Store {
            slot: usize::from(opcode) - 0x43,
            width: 1,
        },
        0x47..=0x4A => RawInstruction::Store {
            slot: usize::from(opcode) - 0x47,
            width: 1,
        },
        0x4B..=0x4E => RawInstruction::Store {
            slot: usize::from(opcode) - 0x4B,
            width: 1,
        },
        0x57 => RawInstruction::Pop,
        0x58 => RawInstruction::Pop2,
        0x59 => RawInstruction::Dup,
        0x5A => RawInstruction::DupX1,
        0x5B => RawInstruction::DupX2,
        0x5F => RawInstruction::Swap,
        0x86 => RawInstruction::I2F,
        0x8B => RawInstruction::F2I,
        0x99..=0x9E => RawInstruction::Branch {
            target: i32::from(operand_i2(code, offset)?),
            pops: 1,
            unconditional: false,
            width: 3,
        },
        0x9F..=0xA6 => RawInstruction::Branch {
            target: i32::from(operand_i2(code, offset)?),
            pops: 2,
            unconditional: false,
            width: 3,
        },
        0xA7 => RawInstruction::Branch {
            target: i32::from(operand_i2(code, offset)?),
            pops: 0,
            unconditional: true,
            width: 3,
        },
        0xA8 => RawInstruction::Unknown {
            width: 3,
            pops: 0,
            pushes: 1,
        },
        0xA9 => RawInstruction::Unknown {
            width: 2,
            pops: 0,
            pushes: 0,
        },
        0xAA => return decode_tableswitch(code, offset),
        0xAB => return decode_lookupswitch(code, offset),
        0xAC..=0xB1 => RawInstruction::Return,
        0xB6 => RawInstruction::Invoke {
            kind: InvokeKind::Virtual,
            index: operand_u2(code, offset)?,
        },
        0xB7 => RawInstruction::Invoke {
            kind: InvokeKind::Special,
            index: operand_u2(code, offset)?,
        },
        0xB8 => RawInstruction::Invoke {
            kind: InvokeKind::Static,
            index: operand_u2(code, offset)?,
        },
        0xB9 => {
            ensure_width(code, offset, 5)?;
            RawInstruction::Invoke {
                kind: InvokeKind::Interface,
                index: operand_u2(code, offset)?,
            }
        }
        0xBA => {
            ensure_width(code, offset, 5)?;
            RawInstruction::InvokeDynamic {
                index: operand_u2(code, offset)?,
            }
        }
        0xBB => RawInstruction::New(operand_u2(code, offset)?),
        0xC4 => return decode_wide(code, offset),
        0xC6 | 0xC7 => RawInstruction::Branch {
            target: i32::from(operand_i2(code, offset)?),
            pops: 1,
            unconditional: false,
            width: 3,
        },
        0xC8 | 0xC9 => {
            let bytes = (0..4)
                .map(|i| operand_u1(code, offset, i))
                .collect::<Result<Vec<_>, _>>()?;
            RawInstruction::Branch {
                target: be_i4(&bytes),
                pops: 0,
                unconditional: opcode == 0xC8,
                width: 5,
            }
        }
        0xCA | 0xFE | 0xFF => return Err(ExtractorError::Malformed("reserved opcode")),
        _ => {
            let width = fixed_opcode_width(opcode)
                .ok_or(ExtractorError::Malformed("unknown opcode width"))?;
            ensure_width(code, offset, width)?;
            let (pops, pushes) = generic_stack_effect(opcode);
            RawInstruction::Unknown {
                width,
                pops,
                pushes,
            }
        }
    })
}

fn ensure_width(code: &[u8], offset: usize, width: usize) -> Result<(), ExtractorError> {
    if offset
        .checked_add(width)
        .is_some_and(|end| end <= code.len())
    {
        Ok(())
    } else {
        Err(ExtractorError::TruncatedInstruction { offset })
    }
}

fn decode_tableswitch(code: &[u8], offset: usize) -> Result<RawInstruction, ExtractorError> {
    let padding = (4 - ((offset + 1) % 4)) % 4;
    let base = offset + 1 + padding;
    if base.checked_add(12).is_none_or(|end| end > code.len()) {
        return Err(ExtractorError::TruncatedInstruction { offset });
    }
    let default = be_i4(&code[base..base + 4]);
    let low = be_i4(&code[base + 4..base + 8]);
    let high = be_i4(&code[base + 8..base + 12]);
    let count = high
        .checked_sub(low)
        .and_then(|delta| delta.checked_add(1))
        .filter(|count| *count >= 0 && *count <= 4096)
        .ok_or(ExtractorError::Malformed("bad tableswitch range"))? as usize;
    let targets_start = base + 12;
    let end = targets_start
        .checked_add(count.saturating_mul(4))
        .ok_or(ExtractorError::Malformed("tableswitch length overflow"))?;
    if end > code.len() {
        return Err(ExtractorError::TruncatedInstruction { offset });
    }
    let mut targets = Vec::with_capacity(count);
    for index in 0..count {
        let at = targets_start + index * 4;
        targets.push(be_i4(&code[at..at + 4]));
    }
    Ok(RawInstruction::Switch { default, targets })
}

fn decode_lookupswitch(code: &[u8], offset: usize) -> Result<RawInstruction, ExtractorError> {
    let padding = (4 - ((offset + 1) % 4)) % 4;
    let base = offset + 1 + padding;
    if base.checked_add(8).is_none_or(|end| end > code.len()) {
        return Err(ExtractorError::TruncatedInstruction { offset });
    }
    let default = be_i4(&code[base..base + 4]);
    let pairs = be_i4(&code[base + 4..base + 8]);
    if !(0..=4096).contains(&pairs) {
        return Err(ExtractorError::Malformed("bad lookupswitch pair count"));
    }
    let pairs = pairs as usize;
    let targets_start = base + 8;
    let end = targets_start
        .checked_add(pairs.saturating_mul(8))
        .ok_or(ExtractorError::Malformed("lookupswitch length overflow"))?;
    if end > code.len() {
        return Err(ExtractorError::TruncatedInstruction { offset });
    }
    let mut targets = Vec::with_capacity(pairs);
    for index in 0..pairs {
        let at = targets_start + index * 8 + 4;
        targets.push(be_i4(&code[at..at + 4]));
    }
    Ok(RawInstruction::Switch { default, targets })
}

fn decode_wide(code: &[u8], offset: usize) -> Result<RawInstruction, ExtractorError> {
    let widened = operand_u1(code, offset, 0)?;
    let width = if widened == 0x84 { 6 } else { 4 };
    ensure_width(code, offset, width)?;
    Ok(RawInstruction::Unknown {
        width,
        pops: usize::from(matches!(widened, 0x36..=0x3A)),
        pushes: usize::from(matches!(widened, 0x15..=0x19)),
    })
}

fn fixed_opcode_width(opcode: u8) -> Option<usize> {
    Some(match opcode {
        0x00..=0x0F
        | 0x1A..=0x35
        | 0x3B..=0x56
        | 0x57..=0x83
        | 0x85..=0x98
        | 0xAC..=0xB1
        | 0xBE..=0xBF
        | 0xC2..=0xC3 => 1,
        0x10 | 0x12 | 0x15..=0x19 | 0x36..=0x3A | 0xA9 => 2,
        0x11
        | 0x13..=0x14
        | 0x84
        | 0x99..=0xA8
        | 0xB2..=0xB8
        | 0xBB..=0xBD
        | 0xC0..=0xC1
        | 0xC6..=0xC7 => 3,
        0xB9..=0xBA | 0xC8..=0xC9 => 5,
        0xC5 => 4,
        _ => return None,
    })
}

fn generic_stack_effect(opcode: u8) -> (usize, usize) {
    match opcode {
        0x2E..=0x35 => (2, 1),
        0x4F..=0x56 => (3, 0),
        0x60..=0x73 | 0x78..=0x83 | 0x94..=0x98 => (2, 1),
        0x74..=0x77 | 0x85..=0x93 | 0xBE | 0xC1 => (1, 1),
        0xB2 => (0, 1),
        0xB3 | 0xC2 | 0xC3 => (1, 0),
        0xB4 | 0xBC | 0xBD => (1, 1),
        0xB5 => (2, 0),
        0xC0 => (0, 0),
        _ => (0, 0),
    }
}

fn branch_target(offset: usize, relative: i32) -> Result<usize, ExtractorError> {
    let target = if relative.is_negative() {
        offset.checked_sub(relative.unsigned_abs() as usize)
    } else {
        offset.checked_add(relative as usize)
    };
    target.ok_or(ExtractorError::Malformed("branch target overflow"))
}

fn parse_method_descriptor(descriptor: &str) -> Option<(usize, bool)> {
    let bytes = descriptor.as_bytes();
    if bytes.first() != Some(&b'(') {
        return None;
    }
    let mut pos = 1;
    let mut count = 0usize;
    while *bytes.get(pos)? != b')' {
        pos = skip_field_type(bytes, pos)?;
        count = count.checked_add(1)?;
    }
    pos = pos.checked_add(1)?;
    if pos >= bytes.len() {
        return None;
    }
    let returns_value = if bytes[pos] == b'V' {
        pos = pos.checked_add(1)?;
        false
    } else {
        pos = skip_field_type(bytes, pos)?;
        true
    };
    if pos == bytes.len() {
        Some((count, returns_value))
    } else {
        None
    }
}

fn skip_field_type(bytes: &[u8], mut pos: usize) -> Option<usize> {
    while *bytes.get(pos)? == b'[' {
        pos = pos.checked_add(1)?;
    }
    match *bytes.get(pos)? {
        b'B' | b'C' | b'D' | b'F' | b'I' | b'J' | b'S' | b'Z' => pos.checked_add(1),
        b'L' => {
            let end = bytes[pos..].iter().position(|byte| *byte == b';')? + pos;
            end.checked_add(1)
        }
        _ => None,
    }
}

fn constant_int_return(class: &ParsedClassMethods, method: &ParsedMethod) -> Option<i32> {
    let mut pc = 0usize;
    let mut value = None;
    let mut push_constant = |constant: i32| {
        if value.replace(constant).is_some() {
            return None;
        }
        Some(())
    };
    while pc < method.code.len() {
        let opcode = method.code[pc];
        match opcode {
            0x02..=0x08 => {
                push_constant(i32::from(opcode) - 3)?;
                pc += 1;
            }
            0x10 => {
                push_constant(i32::from(*method.code.get(pc + 1)? as i8))?;
                pc += 2;
            }
            0x11 => {
                let hi = *method.code.get(pc + 1)?;
                let lo = *method.code.get(pc + 2)?;
                push_constant(i32::from(i16::from_be_bytes([hi, lo])))?;
                pc += 3;
            }
            0x12 => {
                let index = u16::from(*method.code.get(pc + 1)?);
                match class.resolve_ldc_constant(index).ok()? {
                    LdcConst::Int(constant) => push_constant(constant)?,
                    _ => return None,
                }
                pc += 2;
            }
            0x13 => {
                let hi = *method.code.get(pc + 1)?;
                let lo = *method.code.get(pc + 2)?;
                let index = u16::from_be_bytes([hi, lo]);
                match class.resolve_ldc_constant(index).ok()? {
                    LdcConst::Int(constant) => push_constant(constant)?,
                    _ => return None,
                }
                pc += 3;
            }
            0xAC => {
                return if pc + 1 == method.code.len() {
                    value
                } else {
                    None
                };
            }
            _ => return None,
        }
    }
    None
}

fn finalize_method_records(
    class_name: &str,
    method_name: &str,
    analysis: MethodAnalysis,
) -> (Vec<BlockPropertyRecord>, Vec<ItemPropertyRecord>) {
    let mut registrations = analysis.registration_names.iter();
    let mut blocks = Vec::new();
    let mut items = Vec::new();
    for (id, final_builder) in analysis.final_builders {
        match final_builder {
            FinalBuilder::Block(builder) if builder.touched => {
                let registration_name = registrations.next().cloned();
                let key = registration_name
                    .clone()
                    .unwrap_or_else(|| format!("{class_name}#{method_name}#block:{id}"));
                blocks.push(BlockPropertyRecord {
                    key,
                    registration_name,
                    source_method: method_name.to_string(),
                    properties: builder.properties,
                });
            }
            FinalBuilder::Item(builder) if builder.touched => {
                let registration_name = registrations.next().cloned();
                let key = registration_name
                    .clone()
                    .unwrap_or_else(|| format!("{class_name}#{method_name}#item:{id}"));
                items.push(ItemPropertyRecord {
                    key,
                    registration_name,
                    source_method: method_name.to_string(),
                    properties: builder.properties,
                });
            }
            _ => {}
        }
    }
    (blocks, items)
}

/// Extract block/item property builder chains from one `.class` byte buffer.
pub fn extract_class_properties(
    class_bytes: &[u8],
    budget: &ExtractionBudget,
) -> Result<ClassPropertyReport, ExtractorError> {
    let class = parse_class_methods(class_bytes)?;
    if class.methods.len() > budget.max_methods_per_class {
        return Err(ExtractorError::BudgetExceeded {
            resource: "methods",
            limit: budget.max_methods_per_class,
        });
    }

    let mut blocks = Vec::new();
    let mut items = Vec::new();
    let mut stats = ExtractionStats {
        classes_scanned: 1,
        ..ExtractionStats::default()
    };
    for method in &class.methods {
        let analysis = MethodInterpreter::new(&class, method, budget).interpret()?;
        stats.methods_analyzed += 1;
        stats.unresolved_dynamic_calls += analysis.unresolved_dynamic_calls;
        let (method_blocks, method_items) =
            finalize_method_records(&class.class_name, &method.name, analysis);
        blocks.extend(method_blocks);
        items.extend(method_items);
    }
    stats.blocks_extracted = blocks.len();
    stats.items_extracted = items.len();
    Ok(ClassPropertyReport {
        class_name: class.class_name,
        blocks,
        items,
        stats,
    })
}

/// Extract block/item property builder chains from every `.class` member of
/// one in-memory JAR/ZIP archive. Members are decompressed and verified in
/// memory; nothing is written to disk.
pub fn extract_jar_properties(
    jar_bytes: &[u8],
    budget: &ExtractionBudget,
) -> Result<JarPropertyReport, ExtractorError> {
    let input_size = jar_bytes.len() as u64;
    if input_size > budget.max_archive_bytes {
        return Err(ExtractorError::ArchiveTooLarge {
            size: input_size,
            max: budget.max_archive_bytes,
        });
    }
    let entries = zip_member_list(jar_bytes, "property extraction JAR")
        .map_err(|error| ExtractorError::Zip(error.reason().to_string()))?;
    let class_entries: Vec<_> = entries
        .iter()
        .filter(|entry| entry.name.ends_with(".class") && entry.name != "module-info.class")
        .collect();
    if class_entries.len() > budget.max_classes {
        return Err(ExtractorError::BudgetExceeded {
            resource: "classes",
            limit: budget.max_classes,
        });
    }

    let mut classes = Vec::new();
    let mut stats = ExtractionStats::default();
    let mut decompressed_bytes = 0u64;
    for entry in class_entries {
        let bytes = read_zip_member(jar_bytes, entry, "property extraction JAR")
            .map_err(|error| ExtractorError::Zip(error.reason().to_string()))?;
        decompressed_bytes = decompressed_bytes.saturating_add(bytes.len() as u64);
        if decompressed_bytes > budget.max_archive_bytes {
            return Err(ExtractorError::ArchiveTooLarge {
                size: decompressed_bytes,
                max: budget.max_archive_bytes,
            });
        }
        let report = extract_class_properties(&bytes, budget)?;
        stats.merge(&report.stats);
        classes.push(report);
    }
    Ok(JarPropertyReport { classes, stats })
}
