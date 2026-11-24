// SPDX-License-Identifier: LGPL-3.0-or-later

//! Clean-room Tiny v2 (`*.tiny`) runtime mapping loader and canonical
//! Intermediary→Mojmap name resolver.
//!
//! Fabric mods run against Fabric *Intermediary* names
//! (`net/minecraft/class_1309` + `method_5643`), while NeoForge mods run
//! against Mojmap names (`net/minecraft/world/entity/LivingEntity` +
//! `hurt`). [`TinyV2MappingTable`] parses a Tiny v2 mapping document —
//! the format Loom/Minecraft ships for Intermediary↔named translation —
//! purely with `std`, and resolves an Intermediary class, method, or
//! field name to its canonical `named` (Mojmap/Yarn-style) counterpart.
//! [`crate::mixin_lifter::lift_mixin_sites_with_mappings`] consumes the
//! table so a Fabric Mixin written against Intermediary names resolves
//! to the *exact same* [`crate::mixin_lifter::StandardEventHook`] as the
//! equivalent NeoForge Mojmap Mixin.
//!
//! When no `.tiny` document is available, the built-in
//! [`BUILTIN_CLASS_ANCHORS`]/[`BUILTIN_METHOD_ANCHORS`] table covers the
//! ~15 standard vanilla hook classes and lifecycle methods that
//! [`crate::mixin_lifter`] already lifts — the canonical (class, method)
//! pair each Intermediary alias stands for. Anchors only *normalize
//! names*; whether a normalized pair binds to a hook stays decided by
//! the lifter's fail-closed `STANDARD_TARGETS` table, so an anchor can
//! never make an unmapped target liftable.
//!
//! Fail-closed parsing rules: the input is bounded at
//! [`MAX_TINY_V2_BYTES`], the header must be `tiny<TAB>2<TAB>0` followed
//! by at least two namespace names, every row must carry exactly one
//! name column per namespace, member rows must follow a class row, and
//! unknown row kinds are an error. A v1 header (`v1`-style), a corrupt
//! column count, or an out-of-place member is a hard parse error — never
//! a silently partial table.
//!
//! Clean-room note: like `mixin_lifter`'s target table, this module
//! carries factual mapping data (class/method name correspondences)
//! only — no code, comments, or logic from Minecraft, Fabric, Yarn,
//! Intermediary, Tiny, or any mod is copied, linked, bundled, or
//! derived from here.

use std::borrow::Cow;
use std::collections::BTreeMap;

/// Largest Tiny v2 document the loader accepts — far past the few MiB a
/// real mod-shipped `mappings/*.tiny` refmap needs, small enough that a
/// hostile member cannot exhaust the scan budget.
pub const MAX_TINY_V2_BYTES: usize = 16 * 1024 * 1024;

/// Most namespace columns a Tiny v2 header may declare.
const MAX_TINY_NAMESPACES: usize = 8;

/// One Intermediary→canonical hook-anchor pair for the built-in table.
struct BuiltinAnchor {
    /// Intermediary name (`net/minecraft/class_1309`, `method_5643`).
    intermediary: &'static str,
    /// Canonical Mojmap-style internal name the anchor stands for.
    canonical: &'static str,
}

/// Clean-room canonical class anchors covering every vanilla hook class
/// `mixin_lifter`'s `STANDARD_TARGETS` accepts plus the standard classes
/// Fabric Mixins most often name (`Level`, `Player`, `ServerPlayer`,
/// `FarmBlock`, `ItemStack`, `Item`). Intermediary → Mojmap internal
/// name.
const BUILTIN_CLASS_ANCHORS: &[BuiltinAnchor] = &[
    BuiltinAnchor {
        intermediary: "net/minecraft/class_1297",
        canonical: "net/minecraft/world/entity/Entity",
    },
    BuiltinAnchor {
        intermediary: "net/minecraft/class_1309",
        canonical: "net/minecraft/world/entity/LivingEntity",
    },
    BuiltinAnchor {
        intermediary: "net/minecraft/class_1657",
        canonical: "net/minecraft/world/entity/player/Player",
    },
    BuiltinAnchor {
        intermediary: "net/minecraft/class_1747",
        canonical: "net/minecraft/world/item/BlockItem",
    },
    BuiltinAnchor {
        intermediary: "net/minecraft/class_1792",
        canonical: "net/minecraft/world/item/Item",
    },
    BuiltinAnchor {
        intermediary: "net/minecraft/class_1799",
        canonical: "net/minecraft/world/item/ItemStack",
    },
    BuiltinAnchor {
        intermediary: "net/minecraft/class_1937",
        canonical: "net/minecraft/world/level/Level",
    },
    BuiltinAnchor {
        intermediary: "net/minecraft/class_2344",
        canonical: "net/minecraft/world/level/block/FarmBlock",
    },
    BuiltinAnchor {
        intermediary: "net/minecraft/class_2680",
        canonical: "net/minecraft/world/level/block/state/BlockState",
    },
    BuiltinAnchor {
        intermediary: "net/minecraft/class_2792",
        canonical: "net/minecraft/network/protocol/game/ServerGamePacketListener",
    },
    BuiltinAnchor {
        intermediary: "net/minecraft/class_3218",
        canonical: "net/minecraft/server/level/ServerLevel",
    },
    BuiltinAnchor {
        intermediary: "net/minecraft/class_3222",
        canonical: "net/minecraft/server/level/ServerPlayer",
    },
    BuiltinAnchor {
        intermediary: "net/minecraft/class_3225",
        canonical: "net/minecraft/server/level/ServerPlayerGameMode",
    },
    BuiltinAnchor {
        intermediary: "net/minecraft/class_3244",
        canonical: "net/minecraft/server/network/ServerGamePacketListenerImpl",
    },
    BuiltinAnchor {
        intermediary: "net/minecraft/class_3324",
        canonical: "net/minecraft/server/players/PlayerList",
    },
    BuiltinAnchor {
        intermediary: "net/minecraft/class_4970",
        canonical: "net/minecraft/world/level/block/state/BlockBehaviour",
    },
    BuiltinAnchor {
        intermediary: "net/minecraft/class_4970$class_4971",
        canonical: "net/minecraft/world/level/block/state/BlockBehaviour$BlockStateBase",
    },
    // Universal 1.21.1 hook/accessor/invoker classes.
    BuiltinAnchor {
        intermediary: "net/minecraft/class_22",
        canonical: "net/minecraft/world/level/saveddata/maps/MapItemSavedData",
    },
    BuiltinAnchor {
        intermediary: "net/minecraft/class_1293",
        canonical: "net/minecraft/world/effect/MobEffectInstance",
    },
    BuiltinAnchor {
        intermediary: "net/minecraft/class_1533",
        canonical: "net/minecraft/world/entity/decoration/ItemFrame",
    },
    BuiltinAnchor {
        intermediary: "net/minecraft/class_1542",
        canonical: "net/minecraft/world/entity/item/ItemEntity",
    },
    BuiltinAnchor {
        intermediary: "net/minecraft/class_1696",
        canonical: "net/minecraft/world/entity/vehicle/MinecartFurnace",
    },
    BuiltinAnchor {
        intermediary: "net/minecraft/class_1735",
        canonical: "net/minecraft/world/inventory/Slot",
    },
    BuiltinAnchor {
        intermediary: "net/minecraft/class_1838",
        canonical: "net/minecraft/world/item/context/UseOnContext",
    },
    BuiltinAnchor {
        intermediary: "net/minecraft/class_1845",
        canonical: "net/minecraft/world/item/alchemy/PotionBrewing",
    },
    BuiltinAnchor {
        intermediary: "net/minecraft/class_1869",
        canonical: "net/minecraft/world/item/crafting/ShapedRecipe",
    },
    BuiltinAnchor {
        intermediary: "net/minecraft/class_2292",
        canonical: "net/minecraft/world/level/block/ConcretePowderBlock",
    },
    BuiltinAnchor {
        intermediary: "net/minecraft/class_2302",
        canonical: "net/minecraft/world/level/block/CropBlock",
    },
    BuiltinAnchor {
        intermediary: "net/minecraft/class_2315",
        canonical: "net/minecraft/world/level/block/DispenserBlock",
    },
    BuiltinAnchor {
        intermediary: "net/minecraft/class_2354",
        canonical: "net/minecraft/world/level/block/FenceBlock",
    },
    BuiltinAnchor {
        intermediary: "net/minecraft/class_2370",
        canonical: "net/minecraft/core/MappedRegistry",
    },
    BuiltinAnchor {
        intermediary: "net/minecraft/class_2505",
        canonical: "net/minecraft/nbt/NbtAccounter",
    },
    BuiltinAnchor {
        intermediary: "net/minecraft/class_2513",
        canonical: "net/minecraft/world/level/block/StemBlock",
    },
    BuiltinAnchor {
        intermediary: "net/minecraft/class_2965",
        canonical: "net/minecraft/core/dispenser/ProjectileDispenseBehavior",
    },
    BuiltinAnchor {
        intermediary: "net/minecraft/class_2995",
        canonical: "net/minecraft/server/ServerScoreboard",
    },
    BuiltinAnchor {
        intermediary: "net/minecraft/class_3031",
        canonical: "net/minecraft/world/level/levelgen/feature/Feature",
    },
    BuiltinAnchor {
        intermediary: "net/minecraft/class_3609",
        canonical: "net/minecraft/world/level/material/FlowingFluid",
    },
    BuiltinAnchor {
        intermediary: "net/minecraft/class_3781",
        canonical: "net/minecraft/world/level/levelgen/structure/pools/SinglePoolElement",
    },
    BuiltinAnchor {
        intermediary: "net/minecraft/class_3785",
        canonical: "net/minecraft/world/level/levelgen/structure/pools/StructureTemplatePool",
    },
    BuiltinAnchor {
        intermediary: "net/minecraft/class_3898",
        canonical: "net/minecraft/server/level/ChunkMap",
    },
    BuiltinAnchor {
        intermediary: "net/minecraft/class_3898$class_3208",
        canonical: "net/minecraft/server/level/ChunkMap$TrackedEntity",
    },
    BuiltinAnchor {
        intermediary: "net/minecraft/class_3922",
        canonical: "net/minecraft/world/level/block/CampfireBlock",
    },
    BuiltinAnchor {
        intermediary: "net/minecraft/class_4516",
        canonical: "net/minecraft/gametest/framework/GameTestHelper",
    },
    BuiltinAnchor {
        intermediary: "net/minecraft/class_4915",
        canonical: "net/minecraft/data/models/ItemModelGenerators",
    },
    BuiltinAnchor {
        intermediary: "net/minecraft/class_5141",
        canonical: "net/minecraft/world/level/levelgen/feature/trunkplacers/TrunkPlacer",
    },
    BuiltinAnchor {
        intermediary: "net/minecraft/class_5350",
        canonical: "net/minecraft/server/ReloadableServerResources",
    },
    BuiltinAnchor {
        intermediary: "net/minecraft/class_7788",
        canonical: "net/minecraft/data/loot/BlockLootSubProvider",
    },
];

/// Clean-room canonical method anchors covering every lifecycle method
/// `mixin_lifter`'s `STANDARD_TARGETS` lifts — including `FarmBlock`
/// trample (`turnToDirt`, yarn `setToDirt`) — plus the verified
/// pure/read-only `@Invoker` query methods.
/// Intermediary `method_NNNN` → canonical Mojmap name. Intermediary
/// method numbers are globally unique and hierarchy-shared, so a flat
/// name map is exact.
const BUILTIN_METHOD_ANCHORS: &[BuiltinAnchor] = &[
    // Entity/LivingEntity hurt.
    BuiltinAnchor {
        intermediary: "method_5643",
        canonical: "hurt",
    },
    // ServerPlayerGameMode destroyBlock (yarn `tryBreakBlock`).
    BuiltinAnchor {
        intermediary: "method_14266",
        canonical: "destroyBlock",
    },
    // BlockItem place / placeBlock.
    BuiltinAnchor {
        intermediary: "method_7712",
        canonical: "place",
    },
    BuiltinAnchor {
        intermediary: "method_7708",
        canonical: "placeBlock",
    },
    // Entity hierarchy tick (Player/ServerPlayer inherit the number).
    BuiltinAnchor {
        intermediary: "method_5773",
        canonical: "tick",
    },
    // ServerLevel tick.
    BuiltinAnchor {
        intermediary: "method_18765",
        canonical: "tick",
    },
    // ServerPlayer doTick (yarn `playerTick`).
    BuiltinAnchor {
        intermediary: "method_14226",
        canonical: "doTick",
    },
    // ServerGamePacketListener(Impl) handleChat (yarn `onChatMessage`).
    BuiltinAnchor {
        intermediary: "method_12048",
        canonical: "handleChat",
    },
    // PlayerList placeNewPlayer (yarn `onPlayerConnect`) / remove.
    BuiltinAnchor {
        intermediary: "method_14570",
        canonical: "placeNewPlayer",
    },
    BuiltinAnchor {
        intermediary: "method_14611",
        canonical: "remove",
    },
    // BlockBehaviour(+BlockStateBase) useWithoutItem / useItemOn
    // (yarn `onUse` / `onUseWithItem`).
    BuiltinAnchor {
        intermediary: "method_55766",
        canonical: "useWithoutItem",
    },
    BuiltinAnchor {
        intermediary: "method_55765",
        canonical: "useItemOn",
    },
    BuiltinAnchor {
        intermediary: "method_55781",
        canonical: "useWithoutItem",
    },
    BuiltinAnchor {
        intermediary: "method_55780",
        canonical: "useItemOn",
    },
    // FarmBlock trample — `turnToDirt` (yarn `setToDirt`; Mojmap 1.21.1
    // static `FarmBlock.turnToDirt`). `fallOn` is the separate
    // `method_9554`/`onLandedUpon` below.
    BuiltinAnchor {
        intermediary: "method_10125",
        canonical: "turnToDirt",
    },
    // FarmBlock/BlockBehaviour `fallOn` (yarn `onLandedUpon`) — named for
    // completeness; no standard hook binds it today.
    BuiltinAnchor {
        intermediary: "method_9554",
        canonical: "fallOn",
    },
    // Entity load/save (yarn `readNbt`/`writeNbt` — Mojmap `load`/`save`).
    BuiltinAnchor {
        intermediary: "method_5651",
        canonical: "load",
    },
    BuiltinAnchor {
        intermediary: "method_5647",
        canonical: "save",
    },
    // Player canEat (yarn `canConsume`).
    BuiltinAnchor {
        intermediary: "method_7332",
        canonical: "canEat",
    },
    // Item useOn (yarn `useOnBlock`).
    BuiltinAnchor {
        intermediary: "method_7884",
        canonical: "useOn",
    },
    // FenceBlock connectsTo / isSameFence (yarn `canConnect` /
    // `canConnectToFence`).
    BuiltinAnchor {
        intermediary: "method_10184",
        canonical: "connectsTo",
    },
    BuiltinAnchor {
        intermediary: "method_26375",
        canonical: "isSameFence",
    },
    // CampfireBlock isSmokeSource (yarn `isLitCampfire` — the hay-bale
    // smoke-height query).
    BuiltinAnchor {
        intermediary: "method_23896",
        canonical: "isSmokeSource",
    },
    // CropBlock/StemBlock mayPlaceOn (yarn `canPlantOnTop`, declared on
    // the shared BushBlock parent).
    BuiltinAnchor {
        intermediary: "method_9695",
        canonical: "mayPlaceOn",
    },
    // FlowingFluid canPassThrough (yarn `canFlowThrough`).
    BuiltinAnchor {
        intermediary: "method_15746",
        canonical: "canPassThrough",
    },
    // Scoreboard team change, inherited by ServerScoreboard (yarn
    // `addScoreHolderToTeam` / `removeScoreHolderFromTeam` on `Scoreboard`
    // class_269 — numbers are hierarchy-shared).
    BuiltinAnchor {
        intermediary: "method_1172",
        canonical: "addPlayerToTeam",
    },
    BuiltinAnchor {
        intermediary: "method_1157",
        canonical: "removePlayerFromTeam",
    },
    // MinecraftServer reloadResources / ReloadableServerResources
    // loadResources (yarn `DataPackContents.reload`).
    BuiltinAnchor {
        intermediary: "method_29439",
        canonical: "reloadResources",
    },
    BuiltinAnchor {
        intermediary: "method_29466",
        canonical: "loadResources",
    },
    // MapItemSavedData load / save (yarn `MapState.fromNbt`;
    // `PersistentState.save` is inherited).
    BuiltinAnchor {
        intermediary: "method_32371",
        canonical: "load",
    },
    BuiltinAnchor {
        intermediary: "method_17919",
        canonical: "save",
    },
    // Feature isGrassOrDirt / TrunkPlacer setDirtAt (yarn `isSoil`
    // overloads / `setToDirt`).
    BuiltinAnchor {
        intermediary: "method_23396",
        canonical: "isGrassOrDirt",
    },
    BuiltinAnchor {
        intermediary: "method_27368",
        canonical: "isGrassOrDirt",
    },
    BuiltinAnchor {
        intermediary: "method_27400",
        canonical: "setDirtAt",
    },
    // Verified pure/read-only `@Invoker` query methods (yarn names:
    // `getOutlineShape`, `getAgeProperty`, `getHeldItemStack`,
    // `isItemRecipeIngredient`, `getHitResult`, `createSilkTouchCondition`,
    // `getBehaviorForItem`, `getUpdatedState`).
    BuiltinAnchor {
        intermediary: "method_9530",
        canonical: "getShape",
    },
    BuiltinAnchor {
        intermediary: "method_9824",
        canonical: "getAgeProperty",
    },
    BuiltinAnchor {
        intermediary: "method_6940",
        canonical: "getFrameItemStack",
    },
    BuiltinAnchor {
        intermediary: "method_8079",
        canonical: "isContainer",
    },
    BuiltinAnchor {
        intermediary: "method_30344",
        canonical: "getHitResult",
    },
    BuiltinAnchor {
        intermediary: "method_60390",
        canonical: "hasSilkTouch",
    },
    BuiltinAnchor {
        intermediary: "method_10011",
        canonical: "getDispenseMethod",
    },
    BuiltinAnchor {
        intermediary: "method_15727",
        canonical: "getNewLiquid",
    },
];

/// A parsed Tiny v2 mapping table plus the built-in canonical hook
/// anchors: resolves Intermediary class, method, and field names to
/// their canonical `named` counterparts.
///
/// Resolvers consult the parsed table first (a supplied document is
/// authoritative for the names it covers), then the built-in anchor
/// table, and finally return the input normalized — an unknown name is
/// passed through unchanged so the lifter's fail-closed target table
/// decides the outcome, never a guessed name.
#[derive(Clone, Debug, Default)]
pub struct TinyV2MappingTable {
    /// Intermediary class internal name → canonical internal name.
    classes: BTreeMap<String, String>,
    /// Intermediary method name → canonical method name.
    methods: BTreeMap<String, String>,
    /// Intermediary field name → canonical field name.
    fields: BTreeMap<String, String>,
}

/// Strip an optional `L...;` descriptor envelope and translate `.` to
/// `/`, so dot, slash, and descriptor spellings of an internal class
/// name all address the same table key.
fn normalize_name(name: &str) -> Cow<'_, str> {
    let trimmed = name.trim();
    let inner = trimmed
        .strip_prefix('L')
        .and_then(|s| s.strip_suffix(';'))
        .unwrap_or(trimmed);
    if inner.contains('.') {
        Cow::Owned(inner.replace('.', "/"))
    } else {
        Cow::Borrowed(inner)
    }
}

fn anchor_lookup<'a>(anchors: &[BuiltinAnchor], name: &str) -> Option<&'a str> {
    anchors
        .iter()
        .find(|a| a.intermediary == name)
        .map(|a| a.canonical)
}

impl TinyV2MappingTable {
    /// Entries parsed from `.tiny` documents (built-in anchors not
    /// counted).
    pub fn entry_count(&self) -> usize {
        self.classes.len() + self.methods.len() + self.fields.len()
    }

    /// `true` when no `.tiny` document contributed entries — resolvers
    /// still answer from the built-in anchors.
    pub fn is_empty(&self) -> bool {
        self.entry_count() == 0
    }

    /// Merge another parsed table into this one — the later document
    /// wins per name (JAR members merge in deterministic archive order).
    pub fn merge_from(&mut self, other: Self) {
        self.classes.extend(other.classes);
        self.methods.extend(other.methods);
        self.fields.extend(other.fields);
    }

    /// Resolve a class name to its canonical internal (`a/b/C`) form.
    /// Accepts dot, slash, `L...;` descriptor, and Intermediary
    /// spellings; unmapped names return normalized-but-unchanged.
    pub fn canonical_class<'a>(&'a self, name: &'a str) -> Cow<'a, str> {
        let key = normalize_name(name);
        if let Some(canonical) = self.classes.get(key.as_ref()) {
            return Cow::Borrowed(canonical.as_str());
        }
        if let Some(canonical) = anchor_lookup(BUILTIN_CLASS_ANCHORS, key.as_ref()) {
            return Cow::Borrowed(canonical);
        }
        key
    }

    /// Resolve a method name to its canonical name. Only exact
    /// `method_NNNN`-style keys map — a name carrying a descriptor
    /// suffix (`tick(Lnet/...;)V`) never matches, so overloads keep
    /// their exact identity and stay fail-closed downstream.
    pub fn canonical_method<'a>(&'a self, name: &'a str) -> &'a str {
        if let Some(canonical) = self.methods.get(name) {
            return canonical.as_str();
        }
        if let Some(canonical) = anchor_lookup(BUILTIN_METHOD_ANCHORS, name) {
            return canonical;
        }
        name
    }

    /// Resolve a field name to its canonical name (same exact-match
    /// rule as [`Self::canonical_method`]).
    pub fn canonical_field<'a>(&'a self, name: &'a str) -> &'a str {
        if let Some(canonical) = self.fields.get(name) {
            return canonical.as_str();
        }
        name
    }

    /// Normalize a `(target_class, target_method)` pair to canonical
    /// form — the pair [`crate::mixin_lifter::lookup_target`] semantics
    /// judge against the standard-hook table.
    pub fn canonical_target<'a>(
        &'a self,
        class: &'a str,
        method: &'a str,
    ) -> (Cow<'a, str>, &'a str) {
        (self.canonical_class(class), self.canonical_method(method))
    }
}

/// Parse one Tiny v2 mapping document into a [`TinyV2MappingTable`].
///
/// The document must open with `tiny<TAB>2<TAB>0<TAB><namespace...>` and
/// declare at least two namespaces. The Intermediary source column is
/// the namespace literally named `intermediary` when present, else the
/// first; the canonical destination is `named`, else `official`, else
/// the last namespace distinct from the source. Rows are `c` (class:
/// one name per namespace), `m` (method: descriptor + names), and `f`
/// (field: descriptor + names); member rows must be tab-indented under a
/// class row. Empty name columns, wrong column counts, stray row kinds,
/// member rows before any class, a non-v2 header, and input over
/// [`MAX_TINY_V2_BYTES`] are all hard errors — never a partial table.
pub fn parse_tiny_v2(text: &str) -> Result<TinyV2MappingTable, String> {
    if text.len() > MAX_TINY_V2_BYTES {
        return Err(format!(
            "tiny v2 mapping exceeds the {} byte limit",
            MAX_TINY_V2_BYTES
        ));
    }
    let mut lines = text.lines();
    let header = lines
        .next()
        .ok_or_else(|| "tiny v2 mapping is empty".to_string())?;
    let columns: Vec<&str> = header.split('\t').collect();
    if columns.len() < 3 || columns[0] != "tiny" || columns[1] != "2" || columns[2] != "0" {
        return Err(
            "not a tiny v2 mapping (expected header `tiny<TAB>2<TAB>0<TAB><namespaces...>`)"
                .to_string(),
        );
    }
    let namespaces = &columns[3..];
    if namespaces.len() < 2 {
        return Err(format!(
            "tiny v2 header declares {} namespace(s); need at least source + target",
            namespaces.len()
        ));
    }
    if namespaces.len() > MAX_TINY_NAMESPACES {
        return Err(format!(
            "tiny v2 header declares {} namespaces (max {MAX_TINY_NAMESPACES})",
            namespaces.len()
        ));
    }
    let src = namespaces
        .iter()
        .position(|n| *n == "intermediary")
        .unwrap_or(0);
    let dst = namespaces
        .iter()
        .position(|n| *n == "named")
        .or_else(|| namespaces.iter().position(|n| *n == "official"))
        .or_else(|| namespaces.iter().rposition(|n| *n != namespaces[src]))
        .unwrap_or(src);
    if namespaces[dst] == namespaces[src] {
        return Err("tiny v2 namespaces give no distinct target column".to_string());
    }

    let mut table = TinyV2MappingTable::default();
    let mut in_class = false;
    for (index, line) in lines.enumerate() {
        let line_no = index + 2;
        if line.trim().is_empty() {
            continue;
        }
        let depth = line.bytes().take_while(|b| *b == b'\t').count();
        let columns: Vec<&str> = line.split('\t').collect();
        let kind = columns.get(depth).copied().unwrap_or_default();
        match kind {
            "c" => {
                let names = &columns[depth + 1..];
                if names.len() != namespaces.len() {
                    return Err(format!(
                        "line {line_no}: class row has {} name column(s), expected {}",
                        names.len(),
                        namespaces.len()
                    ));
                }
                if names[src].is_empty() || names[dst].is_empty() {
                    return Err(format!("line {line_no}: empty class name column"));
                }
                table
                    .classes
                    .insert(names[src].to_string(), names[dst].to_string());
                in_class = true;
            }
            "m" | "f" => {
                if !in_class || depth == 0 {
                    return Err(format!("line {line_no}: member row outside any class"));
                }
                let names = &columns[depth + 2..];
                if names.len() != namespaces.len() {
                    return Err(format!(
                        "line {line_no}: member row has {} column(s), expected descriptor + {}",
                        columns.len() - depth - 1,
                        namespaces.len()
                    ));
                }
                if names[src].is_empty() || names[dst].is_empty() {
                    return Err(format!("line {line_no}: empty member name column"));
                }
                let (src_name, dst_name) = (names[src].to_string(), names[dst].to_string());
                if kind == "m" {
                    table.methods.insert(src_name, dst_name);
                } else {
                    table.fields.insert(src_name, dst_name);
                }
            }
            other => {
                return Err(format!(
                    "line {line_no}: unknown tiny v2 row kind `{other}`"
                ));
            }
        }
    }
    Ok(table)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "tiny\t2\t0\tintermediary\tnamed\n\
        c\tnet/minecraft/class_1309\tnet/minecraft/world/entity/LivingEntity\n\
        \tm\t(Lnet/minecraft/class_1282;F)Z\tmethod_5643\thurt\n\
        \tf\tF\tfield_6273\thealth\n";

    #[test]
    fn parses_class_method_and_field_rows() {
        let table = parse_tiny_v2(SAMPLE).expect("valid tiny v2 parses");
        assert_eq!(table.classes.len(), 1);
        assert_eq!(table.methods.len(), 1);
        assert_eq!(table.fields.len(), 1);
        assert_eq!(
            table.canonical_class("net/minecraft/class_1309"),
            "net/minecraft/world/entity/LivingEntity"
        );
        assert_eq!(table.canonical_method("method_5643"), "hurt");
        assert_eq!(table.canonical_field("field_6273"), "health");
    }

    #[test]
    fn canonical_class_accepts_dot_slash_and_descriptor_forms() {
        let table = parse_tiny_v2(SAMPLE).unwrap();
        for name in [
            "net.minecraft.class_1309",
            "Lnet/minecraft/class_1309;",
            "net/minecraft/class_1309",
        ] {
            assert_eq!(
                table.canonical_class(name),
                "net/minecraft/world/entity/LivingEntity",
                "{name}"
            );
        }
    }

    #[test]
    fn unmapped_names_pass_through_unchanged() {
        let table = parse_tiny_v2(SAMPLE).unwrap();
        assert_eq!(table.canonical_method("method_9999"), "method_9999");
        assert_eq!(
            table.canonical_class("net/minecraft/class_9999"),
            "net/minecraft/class_9999"
        );
    }

    #[test]
    fn builtin_anchors_resolve_standard_hooks_without_a_file() {
        let table = TinyV2MappingTable::default();
        assert!(table.is_empty());
        assert_eq!(
            table.canonical_class("net/minecraft/class_3218"),
            "net/minecraft/server/level/ServerLevel"
        );
        assert_eq!(table.canonical_method("method_18765"), "tick");
        assert_eq!(table.canonical_method("method_10125"), "turnToDirt");
        assert_eq!(table.canonical_method("method_9554"), "fallOn");
    }

    #[test]
    fn parsed_table_overrides_builtin_anchors() {
        let table = parse_tiny_v2(
            "tiny\t2\t0\tintermediary\tnamed\n\
            c\tnet/minecraft/class_1309\tnet/custom/Other\n",
        )
        .unwrap();
        assert_eq!(
            table.canonical_class("net/minecraft/class_1309"),
            "net/custom/Other"
        );
    }

    #[test]
    fn malformed_inputs_all_fail_closed() {
        for bad in [
            "",
            "tiny\t1\t9\tintermediary\tnamed\n",
            "v1\tofficial\tintermediary\tnamed\n",
            "tiny\t2\t0\tintermediary\n",
            "tiny\t2\t0\tintermediary\tintermediary\n",
            "tiny\t2\t0\tintermediary\tnamed\n\tm\t()V\tmethod_1\ttick\n",
            "tiny\t2\t0\tintermediary\tnamed\nc\tnet/minecraft/class_1\n",
            "tiny\t2\t0\tintermediary\tnamed\nc\ta\tb\n\tx\t()V\tm\tn\n",
            "tiny\t2\t0\tintermediary\tnamed\nc\ta\tb\n\tm\t()V\t\tn\n",
            "tiny\t2\t0\tintermediary\tnamed\nc\ta\tb\n\tf\tI\tfield_1\n",
        ] {
            assert!(parse_tiny_v2(bad).is_err(), "{bad:?}");
        }
        let mut oversized = "tiny\t2\t0\tintermediary\tnamed\nc\ta\tb\n".to_string();
        oversized.push_str(&"x".repeat(MAX_TINY_V2_BYTES));
        assert!(parse_tiny_v2(&oversized).is_err());
    }

    #[test]
    fn member_rows_after_multiple_classes_and_inner_classes_parse() {
        let table = parse_tiny_v2(
            "tiny\t2\t0\tintermediary\tnamed\n\
            c\ta/Outer\tpkg/Outer\n\
            \tc\ta/Outer$Inner\tpkg/Outer$Inner\n\
            \t\tm\t()V\tmethod_1\trun\n\
            c\tb/Second\tpkg/Second\n\
            \tm\t()V\tmethod_2\tstop\n",
        )
        .expect("nested class rows parse");
        assert_eq!(table.canonical_method("method_1"), "run");
        assert_eq!(table.canonical_method("method_2"), "stop");
        assert_eq!(table.canonical_class("a/Outer$Inner"), "pkg/Outer$Inner");
    }
}
