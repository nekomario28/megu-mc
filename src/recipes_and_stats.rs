// SPDX-License-Identifier: LGPL-3.0-or-later
// Phase P3-C core: recipe-book state + `/recipe` engine, player statistics, and
// the tab-list header/footer packet. Every wire layout below was verified
// against the 1.21.1 (protocol 767) server jar:
//
//   S2C 0x41 ClientboundRecipePacket          action VarInt (INIT/ADD/REMOVE) +
//                                             RecipeBookSettings (8 bools in
//                                             RecipeBookType order) + recipes +
//                                             toHighlight on INIT
//   S2C 0x77 ClientboundUpdateRecipesPacket   count VarInt + RecipeHolder
//                                             entries: id ResourceLocation +
//                                             serializer registry id VarInt +
//                                             serializer payload
//   C2S 0x28 ServerboundRecipeBookChangeSettingsPacket  bookType VarInt +
//                                             bookOpen bool + filter bool
//   C2S 0x29 ServerboundRecipeBookSeenRecipePacket      recipeId ResourceLocation
//   C2S 0x22 ServerboundPlaceRecipePacket     containerId i8 + recipeId + makeAll
//   S2C 0x04 ClientboundAwardStatsPacket      count VarInt + (statType, stat,
//                                             value) VarInt triples
//   S2C 0x6d ClientboundTabListPacket         header + footer, each an anonymous
//                                             NBT text component
//
// Item stacks ride the 767 `cuq.i`/`cuq.h` codecs: count VarInt, and for
// non-empty stacks the item id VarInt followed by the component patch (added
// count, removed count, entries). Component values are type-keyed blobs; the
// decoder only accepts the empty patch form vanilla ingredients/results use.

// ---------------------------------------------------------------------------
// small wire helpers (rbs_-prefixed to stay out of the other lanes' namespace)
// ---------------------------------------------------------------------------

/// Vanilla `Utf8String` caps at 32767 chars; cap raw bytes a little above the
/// worst UTF-8 expansion so a malformed length prefix cannot force a big Vec.
const RBS_MAX_STRING_BYTES: usize = 0x20000;

fn rbs_read_str(b: &mut &[u8]) -> Option<String> {
    let n = read_varint(b)?;
    if n < 0 || n as usize > RBS_MAX_STRING_BYTES || n as usize > b.len() {
        return None;
    }
    let (head, tail) = b.split_at(n as usize);
    *b = tail;
    String::from_utf8(head.to_vec()).ok()
}

fn rbs_write_str_list(out: &mut Vec<u8>, ids: &[String]) {
    write_varint(out, ids.len() as i64);
    for id in ids {
        write_str(out, id);
    }
}

#[cfg(test)]
fn rbs_read_str_list(b: &mut &[u8]) -> Option<Vec<String>> {
    let n = read_protocol_i32(b)?;
    if n < 0 {
        return None;
    }
    let mut v = Vec::with_capacity((n as usize).min(1024));
    for _ in 0..n {
        v.push(rbs_read_str(b)?);
    }
    Some(v)
}

fn rbs_read_bool(b: &mut &[u8]) -> Option<bool> {
    let (&x, tail) = b.split_first()?;
    *b = tail;
    Some(x != 0)
}

// ---------------------------------------------------------------------------
// recipe book state: PlayerRecipeBook + the 0x41 packet
// ---------------------------------------------------------------------------

/// `crj` RecipeBookType order on the wire (C2S 0x28 `bookId` and the 0x41
/// settings block iterate this order).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum RecipeBookCategory {
    Crafting = 0,
    Furnace = 1,
    BlastFurnace = 2,
    Smoker = 3,
}

impl RecipeBookCategory {
    const ALL: [Self; 4] = [
        Self::Crafting,
        Self::Furnace,
        Self::BlastFurnace,
        Self::Smoker,
    ];

    fn id(self) -> i32 {
        self as i32
    }

    fn from_id(v: i32) -> Option<Self> {
        match v {
            0 => Some(Self::Crafting),
            1 => Some(Self::Furnace),
            2 => Some(Self::BlastFurnace),
            3 => Some(Self::Smoker),
            _ => None,
        }
    }
}

/// `avt$a` — per-page recipe-book toggles.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct RecipeBookPageSettings {
    gui_open: bool,
    filtering_craftable: bool,
}

/// `avt` RecipeBookSettings: one (open, filter) pair per book page, written as
/// eight booleans in `RecipeBookCategory::ALL` order.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct RecipeBookSettings {
    pages: [RecipeBookPageSettings; 4],
}

impl RecipeBookSettings {
    #[cfg(test)]
    fn page(&self, cat: RecipeBookCategory) -> RecipeBookPageSettings {
        self.pages[cat.id() as usize]
    }

    fn set(&mut self, cat: RecipeBookCategory, gui_open: bool, filtering_craftable: bool) {
        self.pages[cat.id() as usize] = RecipeBookPageSettings {
            gui_open,
            filtering_craftable,
        };
    }

    fn encode(&self, out: &mut Vec<u8>) {
        for p in &self.pages {
            out.push(p.gui_open as u8);
            out.push(p.filtering_craftable as u8);
        }
    }

    #[cfg(test)]
    fn decode(b: &mut &[u8]) -> Option<Self> {
        let mut s = Self::default();
        for page in &mut s.pages {
            page.gui_open = rbs_read_bool(b)?;
            page.filtering_craftable = rbs_read_bool(b)?;
        }
        Some(s)
    }
}

/// A player's recipe-book state: `unlocked` is vanilla's `known` set and
/// `displayed` is the set the client highlights as new (INIT `to_highlight`),
/// cleared when the book sees the recipe.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct PlayerRecipeBook {
    unlocked: std::collections::BTreeSet<String>,
    displayed: std::collections::BTreeSet<String>,
    settings: RecipeBookSettings,
}

impl PlayerRecipeBook {
    #[cfg(test)]
    fn new() -> Self {
        Self::default()
    }

    fn is_unlocked(&self, recipe_id: &str) -> bool {
        self.unlocked.contains(recipe_id)
    }

    /// `ServerRecipeBook.add` + `addHighlight` for a freshly granted recipe:
    /// it shows up as new until the book sees it.
    fn unlock(&mut self, recipe_id: &str) -> bool {
        if !self.unlocked.insert(recipe_id.to_string()) {
            return false;
        }
        self.displayed.insert(recipe_id.to_string());
        true
    }

    fn revoke(&mut self, recipe_id: &str) -> bool {
        self.displayed.remove(recipe_id);
        self.unlocked.remove(recipe_id)
    }

    /// C2S 0x29 (`ServerboundRecipeBookSeenRecipePacket`): the client opened the
    /// recipe in a book, so it stops being highlighted.
    fn seen_recipe(&mut self, recipe_id: &str) {
        self.displayed.remove(recipe_id);
    }

    /// C2S 0x28 (`ServerboundRecipeBookChangeSettingsPacket`): per-page toggles.
    fn apply_book_settings(
        &mut self,
        category: RecipeBookCategory,
        book_open: bool,
        filter_active: bool,
    ) {
        self.settings.set(category, book_open, filter_active);
    }

    fn to_highlight(&self) -> Vec<String> {
        self.displayed.iter().cloned().collect()
    }
}

/// `aee$a` ClientboundRecipePacket.State.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RecipeBookAction {
    Init = 0,
    Add = 1,
    Remove = 2,
}

impl RecipeBookAction {
    fn id(self) -> i32 {
        self as i32
    }

    #[cfg(test)]
    fn from_id(v: i32) -> Option<Self> {
        match v {
            0 => Some(Self::Init),
            1 => Some(Self::Add),
            2 => Some(Self::Remove),
            _ => None,
        }
    }
}

/// S2C 0x41 `ClientboundRecipePacket` payload (packet id prefix not included;
/// the send path wraps it as usual).
#[derive(Clone, Debug, PartialEq, Eq)]
struct UpdateRecipesBook {
    action: RecipeBookAction,
    settings: RecipeBookSettings,
    recipes: Vec<String>,
    /// Wire-valid only for `Init` — vanilla reads the field for no other state.
    to_highlight: Vec<String>,
}

fn encode_update_recipes_book(p: &UpdateRecipesBook) -> Vec<u8> {
    let mut out = Vec::new();
    write_varint(&mut out, p.action.id() as i64);
    p.settings.encode(&mut out);
    rbs_write_str_list(&mut out, &p.recipes);
    if p.action == RecipeBookAction::Init {
        rbs_write_str_list(&mut out, &p.to_highlight);
    }
    out
}

#[cfg(test)]
fn decode_update_recipes_book(b: &mut &[u8]) -> Option<UpdateRecipesBook> {
    let action = RecipeBookAction::from_id(read_protocol_i32(b)?)?;
    let settings = RecipeBookSettings::decode(b)?;
    let recipes = rbs_read_str_list(b)?;
    let to_highlight = if action == RecipeBookAction::Init {
        rbs_read_str_list(b)?
    } else {
        Vec::new()
    };
    Some(UpdateRecipesBook {
        action,
        settings,
        recipes,
        to_highlight,
    })
}

// ---------------------------------------------------------------------------
// recipe-book C2S packets
// ---------------------------------------------------------------------------

/// C2S 0x28 `ServerboundRecipeBookChangeSettingsPacket`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RecipeBookChangeSettings {
    book_type: RecipeBookCategory,
    book_open: bool,
    filter_active: bool,
}

fn decode_recipe_book_change_settings(b: &mut &[u8]) -> Option<RecipeBookChangeSettings> {
    let s = RecipeBookChangeSettings {
        book_type: RecipeBookCategory::from_id(read_protocol_i32(b)?)?,
        book_open: rbs_read_bool(b)?,
        filter_active: rbs_read_bool(b)?,
    };
    if !b.is_empty() {
        return None;
    }
    Some(s)
}

#[cfg(test)]
fn encode_recipe_book_change_settings(s: &RecipeBookChangeSettings) -> Vec<u8> {
    let mut out = Vec::new();
    write_varint(&mut out, s.book_type.id() as i64);
    out.push(s.book_open as u8);
    out.push(s.filter_active as u8);
    out
}

/// C2S 0x29 `ServerboundRecipeBookSeenRecipePacket`: a single ResourceLocation.
fn decode_recipe_book_seen_recipe(b: &mut &[u8]) -> Option<String> {
    let id = rbs_read_str(b)?;
    if !b.is_empty() {
        return None;
    }
    Some(id)
}

#[cfg(test)]
fn encode_recipe_book_seen_recipe(recipe_id: &str) -> Vec<u8> {
    let mut out = Vec::new();
    write_str(&mut out, recipe_id);
    out
}

/// C2S 0x22 `ServerboundPlaceRecipePacket`.
#[derive(Clone, Debug, PartialEq, Eq)]
struct PlaceRecipe {
    window_id: i8,
    recipe_id: String,
    make_all: bool,
}

fn decode_place_recipe(b: &mut &[u8]) -> Option<PlaceRecipe> {
    let (&w, tail) = b.split_first()?;
    *b = tail;
    let p = PlaceRecipe {
        window_id: w as i8,
        recipe_id: rbs_read_str(b)?,
        make_all: rbs_read_bool(b)?,
    };
    if !b.is_empty() {
        return None;
    }
    Some(p)
}

// ---------------------------------------------------------------------------
// `/recipe` command engine
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RecipeCommandVerb {
    Give,
    Take,
}

/// The applied delta and the `0x41` payloads to send to the target player(s).
/// `changed` is the exact set of recipe ids whose state flipped (sorted).
#[derive(Clone, Debug, PartialEq, Eq)]
struct RecipeCommandOutcome {
    changed: Vec<String>,
    packets: Vec<Vec<u8>>,
}

/// `/recipe <give|take> <targets> <recipe|*>` applied to one target's book.
/// `universe` is every recipe id the server's recipe manager knows; `target` is
/// a single recipe id or `*`. Returns `None` when a single recipe id is not in
/// `universe` — vanilla's `/recipe` fails with `commands.recipe.failed` there.
fn exec_recipe_command(
    book: &mut PlayerRecipeBook,
    verb: RecipeCommandVerb,
    universe: &[String],
    target: &str,
) -> Option<RecipeCommandOutcome> {
    let universe_set: std::collections::BTreeSet<&str> =
        universe.iter().map(String::as_str).collect();
    let candidates: Vec<String> = if target == "*" {
        universe.to_vec()
    } else {
        if !universe_set.contains(target) {
            return None;
        }
        vec![target.to_string()]
    };
    let mut changed = Vec::new();
    for id in &candidates {
        let flipped = match verb {
            RecipeCommandVerb::Give => book.unlock(id),
            RecipeCommandVerb::Take => book.revoke(id),
        };
        if flipped {
            changed.push(id.clone());
        }
    }
    changed.sort();
    let packets = if changed.is_empty() {
        Vec::new()
    } else {
        let action = match verb {
            RecipeCommandVerb::Give => RecipeBookAction::Add,
            RecipeCommandVerb::Take => RecipeBookAction::Remove,
        };
        vec![encode_update_recipes_book(&UpdateRecipesBook {
            action,
            settings: book.settings,
            recipes: changed.clone(),
            to_highlight: Vec::new(),
        })]
    };
    Some(RecipeCommandOutcome { changed, packets })
}

// ---------------------------------------------------------------------------
// S2C 0x77 UpdateRecipes: RecipeHolder entries
// ---------------------------------------------------------------------------

/// Vanilla `minecraft:recipe_serializer` registry order for 1.21.1 — the ids the
/// 0x77 entry's second field carries (from `RecipeSerializer.<clinit>`).
#[cfg(test)]
const RBS_RECIPE_SERIALIZERS: &[&str] = &[
    "crafting_shaped",                 // 0
    "crafting_shapeless",              // 1
    "crafting_special_armordye",       // 2
    "crafting_special_bookcloning",    // 3
    "crafting_special_mapcloning",     // 4
    "crafting_special_mapextending",   // 5
    "crafting_special_firework_rocket",// 6
    "crafting_special_firework_star",  // 7
    "crafting_special_firework_star_fade", // 8
    "crafting_special_tippedarrow",    // 9
    "crafting_special_bannerduplicate",// 10
    "crafting_special_shielddecoration", // 11
    "crafting_special_shulkerboxcoloring", // 12
    "crafting_special_suspiciousstew", // 13
    "crafting_special_repairitem",     // 14
    "smelting",                        // 15
    "blasting",                        // 16
    "smoking",                         // 17
    "campfire_cooking",                // 18
    "stonecutting",                    // 19
    "smithing_transform",              // 20
    "smithing_trim",                   // 21
    "crafting_decorated_pot",          // 22
];

/// One network ItemStack (`cuq.h`/`cuq.i` in 1.21.1): `count` VarInt, then for a
/// non-empty stack `item_id` VarInt + the component patch. `added` entries carry
/// their component type id plus the already-encoded value bytes — the value
/// codec is per component type, so the wire keeps it opaque.
#[derive(Clone, Debug, PartialEq, Eq)]
struct RbsSlot {
    count: i32,
    item_id: i32,
    added: Vec<(i32, Vec<u8>)>,
    removed: Vec<i32>,
}

impl RbsSlot {
    #[cfg(test)]
    fn empty() -> Self {
        Self {
            count: 0,
            item_id: 0,
            added: Vec::new(),
            removed: Vec::new(),
        }
    }

    /// A bare stack with no component patch — the only form an `Ingredient`
    /// entry or a plain recipe result carries.
    fn simple(item_id: i32, count: i32) -> Self {
        Self {
            count,
            item_id,
            added: Vec::new(),
            removed: Vec::new(),
        }
    }
}

/// `cyw` Ingredient on the wire: a VarInt-prefixed list of ItemStacks.
type RbsIngredient = Vec<RbsSlot>;

fn rbs_write_slot(out: &mut Vec<u8>, slot: &RbsSlot) {
    write_varint(out, slot.count as i64);
    if slot.count <= 0 {
        return;
    }
    write_varint(out, slot.item_id as i64);
    write_varint(out, slot.added.len() as i64);
    write_varint(out, slot.removed.len() as i64);
    for (ty, value) in &slot.added {
        write_varint(out, *ty as i64);
        out.extend_from_slice(value);
    }
    for ty in &slot.removed {
        write_varint(out, *ty as i64);
    }
}

/// Decodes the patch counts but accepts only the empty patch: component value
/// lengths are per-type, so an arbitrary patch cannot be walked without the
/// component registry codecs.
#[cfg(test)]
fn rbs_read_slot(b: &mut &[u8]) -> Option<RbsSlot> {
    let count = read_protocol_i32(b)?;
    if count <= 0 {
        return Some(RbsSlot::empty());
    }
    let item_id = read_protocol_i32(b)?;
    let added = read_protocol_i32(b)?;
    let removed = read_protocol_i32(b)?;
    if added != 0 || removed != 0 {
        return None;
    }
    Some(RbsSlot {
        count,
        item_id,
        added: Vec::new(),
        removed: Vec::new(),
    })
}

fn rbs_write_ingredient(out: &mut Vec<u8>, ingredient: &RbsIngredient) {
    write_varint(out, ingredient.len() as i64);
    for slot in ingredient {
        rbs_write_slot(out, slot);
    }
}

/// Vanilla rejects empty ItemStacks anywhere `cuq.i` (not `cuq.h`) is used —
/// an empty slot here fails the decode the same way.
#[cfg(test)]
fn rbs_read_required_slot(b: &mut &[u8]) -> Option<RbsSlot> {
    let slot = rbs_read_slot(b)?;
    if slot.count <= 0 {
        return None;
    }
    Some(slot)
}

#[cfg(test)]
fn rbs_read_ingredient(b: &mut &[u8]) -> Option<RbsIngredient> {
    let n = read_protocol_i32(b)?;
    if !(0..=256).contains(&n) {
        return None;
    }
    let mut v = Vec::with_capacity(n as usize);
    for _ in 0..n {
        v.push(rbs_read_required_slot(b)?);
    }
    Some(v)
}

/// Cooking-family payload (`czm` SimpleCookingSerializer), shared by smelting,
/// blasting, smoking and campfire_cooking.
#[cfg(test)]
#[derive(Clone, Debug, PartialEq)]
struct RbsCookingRecipe {
    group: String,
    /// `cyn` CookingBookCategory ordinal: 0 food, 1 blocks, 2 misc.
    category: i32,
    ingredient: RbsIngredient,
    result: RbsSlot,
    experience: f32,
    cooking_time: i32,
}

/// The `type`-keyed recipe payload of a 0x77 entry. `Special` covers every
/// `czn` SimpleCraftingRecipeSerializer payload (a lone category VarInt),
/// including `crafting_decorated_pot`.
#[derive(Clone, Debug, PartialEq)]
enum RbsRecipe {
    #[cfg(test)]
    Shaped {
        group: String,
        /// `cyo` CraftingBookCategory ordinal: 0 building, 1 redstone,
        /// 2 equipment, 3 misc.
        category: i32,
        width: i32,
        height: i32,
        /// `width * height` ingredients, row-major — no count prefix.
        ingredients: Vec<RbsIngredient>,
        result: RbsSlot,
        show_notification: bool,
    },
    #[cfg(test)]
    Shapeless {
        group: String,
        category: i32,
        ingredients: Vec<RbsIngredient>,
        result: RbsSlot,
    },
    #[cfg(test)]
    Smelting(RbsCookingRecipe),
    #[cfg(test)]
    Blasting(RbsCookingRecipe),
    #[cfg(test)]
    Smoking(RbsCookingRecipe),
    #[cfg(test)]
    CampfireCooking(RbsCookingRecipe),
    Stonecutting {
        group: String,
        ingredient: RbsIngredient,
        result: RbsSlot,
    },
    #[cfg(test)]
    SmithingTransform {
        template: RbsIngredient,
        base: RbsIngredient,
        addition: RbsIngredient,
        result: RbsSlot,
    },
    #[cfg(test)]
    SmithingTrim {
        template: RbsIngredient,
        base: RbsIngredient,
        addition: RbsIngredient,
    },
    #[cfg(test)]
    Special {
        serializer_id: i32,
        category: i32,
    },
}

impl RbsRecipe {
    /// `czo` Special by serializer name, e.g. `crafting_special_bookcloning`.
    #[cfg(test)]
    fn special(serializer_name: &str, category: i32) -> Option<Self> {
        let serializer_id = RBS_RECIPE_SERIALIZERS
            .iter()
            .position(|&n| n == serializer_name)? as i32;
        Some(Self::Special {
            serializer_id,
            category,
        })
    }

    fn serializer_id(&self) -> i32 {
        match self {
            #[cfg(test)]
            Self::Shaped { .. } => 0,
            #[cfg(test)]
            Self::Shapeless { .. } => 1,
            #[cfg(test)]
            Self::Smelting(_) => 15,
            #[cfg(test)]
            Self::Blasting(_) => 16,
            #[cfg(test)]
            Self::Smoking(_) => 17,
            #[cfg(test)]
            Self::CampfireCooking(_) => 18,
            Self::Stonecutting { .. } => 19,
            #[cfg(test)]
            Self::SmithingTransform { .. } => 20,
            #[cfg(test)]
            Self::SmithingTrim { .. } => 21,
            #[cfg(test)]
            Self::Special { serializer_id, .. } => *serializer_id,
        }
    }
}

/// `czb` RecipeHolder on the wire: `id` ResourceLocation, then the serializer
/// registry id VarInt, then that serializer's payload.
#[derive(Clone, Debug, PartialEq)]
struct RbsRecipeEntry {
    id: String,
    recipe: RbsRecipe,
}

#[cfg(test)]
fn rbs_write_cooking(out: &mut Vec<u8>, r: &RbsCookingRecipe) {
    write_str(out, &r.group);
    write_varint(out, r.category as i64);
    rbs_write_ingredient(out, &r.ingredient);
    rbs_write_slot(out, &r.result);
    out.extend_from_slice(&r.experience.to_be_bytes());
    write_varint(out, r.cooking_time as i64);
}

#[cfg(test)]
fn rbs_read_cooking(b: &mut &[u8]) -> Option<RbsCookingRecipe> {
    let group = rbs_read_str(b)?;
    let category = read_protocol_i32(b)?;
    let ingredient = rbs_read_ingredient(b)?;
    let result = rbs_read_required_slot(b)?;
    if b.len() < 4 {
        return None;
    }
    let experience = f32::from_be_bytes(b[..4].try_into().ok()?);
    *b = &b[4..];
    let cooking_time = read_protocol_i32(b)?;
    Some(RbsCookingRecipe {
        group,
        category,
        ingredient,
        result,
        experience,
        cooking_time,
    })
}

fn rbs_write_recipe_entry(out: &mut Vec<u8>, entry: &RbsRecipeEntry) {
    write_str(out, &entry.id);
    write_varint(out, entry.recipe.serializer_id() as i64);
    match &entry.recipe {
        #[cfg(test)]
        RbsRecipe::Shaped {
            group,
            category,
            width,
            height,
            ingredients,
            result,
            show_notification,
        } => {
            write_str(out, group);
            write_varint(out, *category as i64);
            write_varint(out, *width as i64);
            write_varint(out, *height as i64);
            for i in ingredients {
                rbs_write_ingredient(out, i);
            }
            rbs_write_slot(out, result);
            out.push(*show_notification as u8);
        }
        #[cfg(test)]
        RbsRecipe::Shapeless {
            group,
            category,
            ingredients,
            result,
        } => {
            write_str(out, group);
            write_varint(out, *category as i64);
            write_varint(out, ingredients.len() as i64);
            for i in ingredients {
                rbs_write_ingredient(out, i);
            }
            rbs_write_slot(out, result);
        }
        #[cfg(test)]
        RbsRecipe::Smelting(r)
        | RbsRecipe::Blasting(r)
        | RbsRecipe::Smoking(r)
        | RbsRecipe::CampfireCooking(r) => rbs_write_cooking(out, r),
        RbsRecipe::Stonecutting {
            group,
            ingredient,
            result,
        } => {
            write_str(out, group);
            rbs_write_ingredient(out, ingredient);
            rbs_write_slot(out, result);
        }
        #[cfg(test)]
        RbsRecipe::SmithingTransform {
            template,
            base,
            addition,
            result,
        } => {
            rbs_write_ingredient(out, template);
            rbs_write_ingredient(out, base);
            rbs_write_ingredient(out, addition);
            rbs_write_slot(out, result);
        }
        #[cfg(test)]
        RbsRecipe::SmithingTrim {
            template,
            base,
            addition,
        } => {
            rbs_write_ingredient(out, template);
            rbs_write_ingredient(out, base);
            rbs_write_ingredient(out, addition);
        }
        #[cfg(test)]
        RbsRecipe::Special { category, .. } => write_varint(out, *category as i64),
    }
}

#[cfg(test)]
fn rbs_read_recipe_entry(b: &mut &[u8]) -> Option<RbsRecipeEntry> {
    let id = rbs_read_str(b)?;
    let serializer_id = read_protocol_i32(b)?;
    let recipe = match serializer_id {
        // crafting_shaped
        0 => {
            let group = rbs_read_str(b)?;
            let category = read_protocol_i32(b)?;
            let width = read_protocol_i32(b)?;
            let height = read_protocol_i32(b)?;
            if !(1..=8).contains(&width) || !(1..=8).contains(&height) {
                return None;
            }
            let cells = (width * height) as usize;
            let mut ingredients = Vec::with_capacity(cells);
            for _ in 0..cells {
                ingredients.push(rbs_read_ingredient(b)?);
            }
            let result = rbs_read_required_slot(b)?;
            let show_notification = rbs_read_bool(b)?;
            RbsRecipe::Shaped {
                group,
                category,
                width,
                height,
                ingredients,
                result,
                show_notification,
            }
        }
        // crafting_shapeless
        1 => {
            let group = rbs_read_str(b)?;
            let category = read_protocol_i32(b)?;
            let ingredients = rbs_read_ingredient_list(b)?;
            let result = rbs_read_required_slot(b)?;
            RbsRecipe::Shapeless {
                group,
                category,
                ingredients,
                result,
            }
        }
        // the cooking family shares one serializer payload
        15..=18 => {
            let r = rbs_read_cooking(b)?;
            match serializer_id {
                15 => RbsRecipe::Smelting(r),
                16 => RbsRecipe::Blasting(r),
                17 => RbsRecipe::Smoking(r),
                _ => RbsRecipe::CampfireCooking(r),
            }
        }
        // stonecutting
        19 => {
            let group = rbs_read_str(b)?;
            let ingredient = rbs_read_ingredient(b)?;
            let result = rbs_read_required_slot(b)?;
            RbsRecipe::Stonecutting {
                group,
                ingredient,
                result,
            }
        }
        // smithing_transform
        20 => {
            let template = rbs_read_ingredient(b)?;
            let base = rbs_read_ingredient(b)?;
            let addition = rbs_read_ingredient(b)?;
            let result = rbs_read_required_slot(b)?;
            RbsRecipe::SmithingTransform {
                template,
                base,
                addition,
                result,
            }
        }
        // smithing_trim
        21 => {
            let template = rbs_read_ingredient(b)?;
            let base = rbs_read_ingredient(b)?;
            let addition = rbs_read_ingredient(b)?;
            RbsRecipe::SmithingTrim {
                template,
                base,
                addition,
            }
        }
        // SimpleCraftingRecipeSerializer payloads: a lone category VarInt
        2..=14 | 22 => RbsRecipe::Special {
            serializer_id,
            category: read_protocol_i32(b)?,
        },
        _ => return None,
    };
    Some(RbsRecipeEntry { id, recipe })
}

/// `jv`/shapeless ingredient list: VarInt count + ingredients (distinct from
/// `RbsIngredient`, which is one ingredient's slot list).
#[cfg(test)]
fn rbs_read_ingredient_list(b: &mut &[u8]) -> Option<Vec<RbsIngredient>> {
    let n = read_protocol_i32(b)?;
    if !(0..=256).contains(&n) {
        return None;
    }
    let mut v = Vec::with_capacity(n as usize);
    for _ in 0..n {
        v.push(rbs_read_ingredient(b)?);
    }
    Some(v)
}

/// S2C 0x77 `ClientboundUpdateRecipesPacket` payload.
fn encode_update_recipes(entries: &[RbsRecipeEntry]) -> Vec<u8> {
    let mut out = Vec::new();
    write_varint(&mut out, entries.len() as i64);
    for e in entries {
        rbs_write_recipe_entry(&mut out, e);
    }
    out
}

#[cfg(test)]
fn decode_update_recipes(b: &mut &[u8]) -> Option<Vec<RbsRecipeEntry>> {
    let n = read_protocol_i32(b)?;
    if !(0..=65536).contains(&n) {
        return None;
    }
    let mut v = Vec::with_capacity((n as usize).min(4096));
    for _ in 0..n {
        v.push(rbs_read_recipe_entry(b)?);
    }
    if !b.is_empty() {
        return None;
    }
    Some(v)
}

// ---------------------------------------------------------------------------
// player statistics: engine + S2C 0x04 AwardStats
// ---------------------------------------------------------------------------

/// `minecraft:stat_type` registry order for 1.21.1 (`avz` Stats.<clinit>) — the
/// `categoryId` of each 0x04 entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum StatCategory {
    Mined = 0,
    Crafted = 1,
    Used = 2,
    Broken = 3,
    PickedUp = 4,
    Dropped = 5,
    Killed = 6,
    KilledBy = 7,
    Custom = 8,
}

impl StatCategory {
    fn id(self) -> i32 {
        self as i32
    }

    fn from_id(v: i32) -> Option<Self> {
        match v {
            0 => Some(Self::Mined),
            1 => Some(Self::Crafted),
            2 => Some(Self::Used),
            3 => Some(Self::Broken),
            4 => Some(Self::PickedUp),
            5 => Some(Self::Dropped),
            6 => Some(Self::Killed),
            7 => Some(Self::KilledBy),
            8 => Some(Self::Custom),
            _ => None,
        }
    }
}

/// `awa` StatsCounter, keyed by `(stat_type, stat_id)` registry ids. BTreeMap
/// keeps iteration deterministic for both the wire codec and persistence.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct PlayerStats {
    counts: std::collections::BTreeMap<(StatCategory, i32), i32>,
}

impl PlayerStats {
    fn new() -> Self {
        Self::default()
    }

    /// `StatsCounter.increment`/`setValue` semantics: the counters saturate at
    /// i32 bounds rather than wrapping.
    fn increment(&mut self, category: StatCategory, stat_id: i32, delta: i32) {
        let e = self.counts.entry((category, stat_id)).or_insert(0);
        *e = e.saturating_add(delta);
    }

    #[cfg(test)]
    fn get(&self, category: StatCategory, stat_id: i32) -> i32 {
        self.counts.get(&(category, stat_id)).copied().unwrap_or(0)
    }

    #[cfg(test)]
    fn reset(&mut self) {
        self.counts.clear();
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.counts.len()
    }
}

/// Save v2 snapshot codec: a VarInt entry count, then `(category, stat_id,
/// value)` VarInt triples in the map's deterministic order.
fn encode_stats(stats: &PlayerStats) -> Vec<u8> {
    let mut out = Vec::new();
    write_varint(&mut out, stats.counts.len() as i64);
    for ((cat, stat_id), value) in &stats.counts {
        write_varint(&mut out, cat.id() as i64);
        write_varint(&mut out, *stat_id as i64);
        write_varint(&mut out, *value as i64);
    }
    out
}

fn decode_stats(b: &mut &[u8]) -> Option<PlayerStats> {
    let n = read_protocol_i32(b)?;
    if !(0..=1_000_000).contains(&n) {
        return None;
    }
    let mut stats = PlayerStats::new();
    for _ in 0..n {
        let cat = StatCategory::from_id(read_protocol_i32(b)?)?;
        let stat_id = read_protocol_i32(b)?;
        let value = read_protocol_i32(b)?;
        stats.counts.insert((cat, stat_id), value);
    }
    if !b.is_empty() {
        return None;
    }
    Some(stats)
}

/// S2C 0x04 `ClientboundAwardStatsPacket` payload.
#[cfg(test)]
fn encode_award_stats(stats: &PlayerStats) -> Vec<u8> {
    encode_stats(stats)
}

#[cfg(test)]
fn decode_award_stats(b: &mut &[u8]) -> Option<PlayerStats> {
    decode_stats(b)
}

/// Answers C2S 0x09 `ServerboundClientCommandPacket` action 1
/// (`REQUEST_STATS`): the 0x04 AwardStats payload for the player.
#[cfg(test)]
fn handle_client_stats_request(stats: &PlayerStats) -> Vec<u8> {
    encode_award_stats(stats)
}

/// C2S 0x09 ClientCommand: a lone action VarInt (0 = perform respawn,
/// 1 = request stats).
#[cfg(test)]
fn decode_client_command_action(b: &mut &[u8]) -> Option<i32> {
    let action = read_protocol_i32(b)?;
    if !b.is_empty() {
        return None;
    }
    Some(action)
}

// ---------------------------------------------------------------------------
// S2C 0x6d ClientboundTabListPacket: header + footer text components
// ---------------------------------------------------------------------------

/// Header and footer as anonymous NBT text components (`{"text":"..."}` via
/// `chat_nbt`, matching `xb.d` ComponentSerialization on the wire).
fn encode_tab_list(header_text: &str, footer_text: &str) -> Vec<u8> {
    let mut out = chat_nbt(header_text);
    out.extend_from_slice(&chat_nbt(footer_text));
    out
}

/// Decodes into the two anonymous-NBT `Tag`s; trailing bytes are rejected.
#[cfg(test)]
fn decode_tab_list(b: &mut &[u8]) -> Option<(nbt::Tag, nbt::Tag)> {
    let header = nbt::read_network_nbt(b).ok()?;
    let footer = nbt::read_network_nbt(b).ok()?;
    if !b.is_empty() {
        return None;
    }
    Some((header, footer))
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod recipes_and_stats_tests {
    use super::*;

    fn book_settings(open: [bool; 4], filter: [bool; 4]) -> RecipeBookSettings {
        let mut s = RecipeBookSettings::default();
        for (i, cat) in RecipeBookCategory::ALL.iter().enumerate() {
            s.set(*cat, open[i], filter[i]);
        }
        s
    }

    #[test]
    fn recipe_book_0x41_init_add_remove_and_c2s_0x28_0x29_0x22_roundtrip() {
        // 0x41 INIT: action 0, eight booleans in RecipeBookType order, the
        // unlocked list, then the to_highlight list.
        let settings = book_settings([true, false, true, false], [false, true, false, true]);
        let init = UpdateRecipesBook {
            action: RecipeBookAction::Init,
            settings,
            recipes: vec!["minecraft:torch".into(), "minecraft:furnace".into()],
            to_highlight: vec!["minecraft:furnace".into()],
        };
        let wire = encode_update_recipes_book(&init);
        assert_eq!(wire[0], 0x00); // action INIT
        assert_eq!(
            &wire[1..9],
            &[1, 0, 0, 1, 1, 0, 0, 1],
            "open/filter pairs in crafting, furnace, blast, smoker order"
        );
        let mut rest = &wire[..];
        let decoded = decode_update_recipes_book(&mut rest).unwrap();
        assert_eq!(decoded, init);
        assert!(rest.is_empty());

        // ADD and REMOVE carry no to_highlight field.
        for action in [RecipeBookAction::Add, RecipeBookAction::Remove] {
            let p = UpdateRecipesBook {
                action,
                settings: RecipeBookSettings::default(),
                recipes: vec!["minecraft:stick".into()],
                to_highlight: vec!["minecraft:must_not_encode".into()],
            };
            let wire = encode_update_recipes_book(&p);
            let mut rest = &wire[..];
            let back = decode_update_recipes_book(&mut rest).unwrap();
            assert!(rest.is_empty());
            assert_eq!(back.action, action);
            assert_eq!(back.recipes, p.recipes);
            assert!(back.to_highlight.is_empty());
        }

        // C2S 0x28: bookId varint + two booleans, applied to the book page.
        let mut book = PlayerRecipeBook::new();
        let wire = encode_recipe_book_change_settings(&RecipeBookChangeSettings {
            book_type: RecipeBookCategory::BlastFurnace,
            book_open: true,
            filter_active: true,
        });
        assert_eq!(wire, &[0x02, 0x01, 0x01]);
        let mut rest = &wire[..];
        let s = decode_recipe_book_change_settings(&mut rest).unwrap();
        assert!(rest.is_empty());
        book.apply_book_settings(s.book_type, s.book_open, s.filter_active);
        let page = book.settings.page(RecipeBookCategory::BlastFurnace);
        assert!(page.gui_open && page.filtering_craftable);
        assert!(!book.settings.page(RecipeBookCategory::Crafting).gui_open);

        // C2S 0x29: the seen recipe leaves the displayed/highlight set only.
        book.unlock("minecraft:torch");
        assert!(book.displayed.contains("minecraft:torch"));
        let wire = encode_recipe_book_seen_recipe("minecraft:torch");
        let mut rest = &wire[..];
        let id = decode_recipe_book_seen_recipe(&mut rest).unwrap();
        assert!(rest.is_empty());
        assert_eq!(id, "minecraft:torch");
        book.seen_recipe(&id);
        assert!(!book.displayed.contains("minecraft:torch"));
        assert!(book.is_unlocked("minecraft:torch"));

        // C2S 0x22: containerId i8 + recipeId string + makeAll bool.
        let mut wire = Vec::new();
        wire.push(0x02u8); // window 2
        write_str(&mut wire, "minecraft:torch");
        wire.push(0x01);
        let mut rest = &wire[..];
        let place = decode_place_recipe(&mut rest).unwrap();
        assert!(rest.is_empty());
        assert_eq!(place.window_id, 2);
        assert_eq!(place.recipe_id, "minecraft:torch");
        assert!(place.make_all);
    }

    #[test]
    fn update_recipes_0x77_encodes_shaped_shapeless_smelting_stonecutting_and_smithing()
     {
        let stick = RbsSlot::simple(280, 4); // minecraft:stick = item id 280
        let plank = vec![RbsSlot::simple(5, 1)];
        let entries = vec![
            // crafting_shaped: group, category, w, h, w*h ingredients, result, notify
            RbsRecipeEntry {
                id: "minecraft:stick".into(),
                recipe: RbsRecipe::Shaped {
                    group: String::new(),
                    category: 3, // misc
                    width: 1,
                    height: 2,
                    ingredients: vec![plank.clone(), plank.clone()],
                    result: stick.clone(),
                    show_notification: false,
                },
            },
            RbsRecipeEntry {
                id: "minecraft:fire_charge".into(),
                recipe: RbsRecipe::Shapeless {
                    group: "fire".into(),
                    category: 3,
                    ingredients: vec![vec![RbsSlot::simple(377, 1)], vec![RbsSlot::simple(289, 1)]],
                    result: RbsSlot::simple(385, 3),
                },
            },
            RbsRecipeEntry {
                id: "minecraft:cooked_beef".into(),
                recipe: RbsRecipe::Smelting(RbsCookingRecipe {
                    group: String::new(),
                    category: 0, // food
                    ingredient: vec![RbsSlot::simple(363, 1)],
                    result: RbsSlot::simple(364, 1),
                    experience: 0.35,
                    cooking_time: 200,
                }),
            },
            RbsRecipeEntry {
                id: "minecraft:cut_sandstone".into(),
                recipe: RbsRecipe::Stonecutting {
                    group: String::new(),
                    ingredient: vec![RbsSlot::simple(24, 1)],
                    result: RbsSlot::simple(151, 1),
                },
            },
            RbsRecipeEntry {
                id: "minecraft:netherite_chestplate".into(),
                recipe: RbsRecipe::SmithingTransform {
                    template: vec![RbsSlot::simple(2266, 1)],
                    base: vec![RbsSlot::simple(311, 1)],
                    addition: vec![RbsSlot::simple(1065, 1)],
                    result: RbsSlot::simple(745, 1),
                },
            },
            RbsRecipeEntry {
                id: "minecraft:bolt_armor_trim".into(),
                recipe: RbsRecipe::SmithingTrim {
                    template: vec![RbsSlot::simple(2256, 1)],
                    base: vec![RbsSlot::simple(311, 1)],
                    addition: vec![RbsSlot::simple(1049, 1)],
                },
            },
            RbsRecipeEntry {
                id: "minecraft:armor_dye".into(),
                recipe: RbsRecipe::special("crafting_special_armordye", 3).unwrap(),
            },
        ];

        // Exact byte check on the shaped entry head: id, serializer 0, group
        // "", category 3, 1x2 grid, two identical plank ingredients, result.
        let mut one = Vec::new();
        rbs_write_recipe_entry(&mut one, &entries[0]);
        let mut expect = Vec::new();
        write_str(&mut expect, "minecraft:stick");
        write_varint(&mut expect, 0); // serializer crafting_shaped
        write_varint(&mut expect, 0); // group ""
        write_varint(&mut expect, 3); // category misc
        write_varint(&mut expect, 1); // width
        write_varint(&mut expect, 2); // height
        for _ in 0..2 {
            write_varint(&mut expect, 1); // ingredient: 1 stack
            write_varint(&mut expect, 1); // count
            write_varint(&mut expect, 5); // oak_planks item id
            write_varint(&mut expect, 0); // added components
            write_varint(&mut expect, 0); // removed components
        }
        write_varint(&mut expect, 4); // result count
        write_varint(&mut expect, 280); // stick
        write_varint(&mut expect, 0);
        write_varint(&mut expect, 0);
        expect.push(0); // show_notification false
        assert_eq!(one, expect);

        let wire = encode_update_recipes(&entries);
        assert_eq!(wire[0], 7); // count
        let mut rest = &wire[..];
        let back = decode_update_recipes(&mut rest).unwrap();
        assert!(rest.is_empty());
        assert_eq!(back, entries);
    }

    #[test]
    fn recipe_command_give_take_wildcard_and_single_updates_book() {
        let universe: Vec<String> = [
            "minecraft:torch",
            "minecraft:furnace",
            "minecraft:stick",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let mut book = PlayerRecipeBook::new();

        // give <single>: unlocks, marks displayed, emits an ADD packet.
        let out =
            exec_recipe_command(&mut book, RecipeCommandVerb::Give, &universe, "minecraft:torch")
                .unwrap();
        assert_eq!(out.changed, vec!["minecraft:torch".to_string()]);
        assert_eq!(out.packets.len(), 1);
        let mut rest = &out.packets[0][..];
        let pkt = decode_update_recipes_book(&mut rest).unwrap();
        assert_eq!(pkt.action, RecipeBookAction::Add);
        assert_eq!(pkt.recipes, out.changed);
        assert!(book.is_unlocked("minecraft:torch"));
        assert!(book.displayed.contains("minecraft:torch"));

        // Re-giving the same recipe is a no-op: no delta, no packet.
        let out =
            exec_recipe_command(&mut book, RecipeCommandVerb::Give, &universe, "minecraft:torch")
                .unwrap();
        assert!(out.changed.is_empty() && out.packets.is_empty());

        // take <single>: revokes, emits a REMOVE packet.
        let out =
            exec_recipe_command(&mut book, RecipeCommandVerb::Take, &universe, "minecraft:torch")
                .unwrap();
        assert_eq!(out.changed, vec!["minecraft:torch".to_string()]);
        let mut rest = &out.packets[0][..];
        assert_eq!(
            decode_update_recipes_book(&mut rest).unwrap().action,
            RecipeBookAction::Remove
        );
        assert!(!book.is_unlocked("minecraft:torch"));

        // Unknown single recipe -> command fails (None).
        assert!(
            exec_recipe_command(&mut book, RecipeCommandVerb::Give, &universe, "minecraft:nope")
                .is_none()
        );

        // give * unlocks every recipe in the universe in one ADD packet.
        let mut sorted_universe = universe.clone();
        sorted_universe.sort();
        let out =
            exec_recipe_command(&mut book, RecipeCommandVerb::Give, &universe, "*").unwrap();
        assert_eq!(out.changed, sorted_universe);
        assert_eq!(book.unlocked.len(), 3);
        assert_eq!(book.to_highlight(), sorted_universe);

        // take * clears the whole book.
        let out =
            exec_recipe_command(&mut book, RecipeCommandVerb::Take, &universe, "*").unwrap();
        assert_eq!(out.changed, sorted_universe);
        assert!(book.unlocked.is_empty() && book.displayed.is_empty());
    }

    #[test]
    fn player_stats_increment_persistence_and_0x04_award_stats_roundtrip() {
        let mut stats = PlayerStats::new();
        stats.increment(StatCategory::Mined, 15, 3); // mined:stone x3
        stats.increment(StatCategory::Mined, 15, 2);
        stats.increment(StatCategory::Killed, 50, 1);
        stats.increment(StatCategory::Custom, 7, i32::MAX);
        assert_eq!(stats.get(StatCategory::Mined, 15), 5);
        assert_eq!(stats.get(StatCategory::Used, 15), 0);

        // saturating counter: never wraps past i32::MAX.
        stats.increment(StatCategory::Custom, 7, 1);
        assert_eq!(stats.get(StatCategory::Custom, 7), i32::MAX);

        // deterministic snapshot codec: stable bytes, strict roundtrip.
        let snap = encode_stats(&stats);
        assert_eq!(snap, encode_stats(&stats));
        let mut rest = &snap[..];
        let back = decode_stats(&mut rest).unwrap();
        assert!(rest.is_empty());
        assert_eq!(back, stats);
        // trailing garbage is rejected.
        let mut junk = snap.clone();
        junk.push(0xff);
        assert!(decode_stats(&mut &junk[..]).is_none());

        // reset clears everything.
        let mut cleared = back.clone();
        cleared.reset();
        assert_eq!(cleared.len(), 0);

        // 0x04 AwardStats: count + (category, stat, value) VarInt triples.
        let wire = encode_award_stats(&stats);
        assert_eq!(wire[0], 3); // three counters
        let mut expect = Vec::new();
        write_varint(&mut expect, 3);
        for (cat, stat, value) in [
            (StatCategory::Mined.id(), 15, 5),
            (StatCategory::Killed.id(), 50, 1),
            (StatCategory::Custom.id(), 7, i32::MAX),
        ] {
            write_varint(&mut expect, cat as i64);
            write_varint(&mut expect, stat as i64);
            write_varint(&mut expect, value as i64);
        }
        assert_eq!(wire, expect);
        let mut rest = &wire[..];
        assert_eq!(decode_award_stats(&mut rest).unwrap(), stats);
        assert!(rest.is_empty());

        // C2S 0x09 client_command action 1 (request stats) -> the 0x04 payload.
        let mut req = Vec::new();
        write_varint(&mut req, 1);
        let mut rest = &req[..];
        assert_eq!(decode_client_command_action(&mut rest).unwrap(), 1);
        assert_eq!(handle_client_stats_request(&stats), wire);
    }

    #[test]
    fn tab_list_0x6d_header_footer_roundtrip() {
        let wire = encode_tab_list("Welcome", "megu");
        assert_eq!(wire[0], 0x0a); // anonymous TAG_Compound
        let mut rest = &wire[..];
        let (header, footer) = decode_tab_list(&mut rest).unwrap();
        assert!(rest.is_empty());
        let text = |t: &nbt::Tag| {
            t.get("text")
                .and_then(|t| t.string())
                .and_then(|s| s.to_utf8().ok())
        };
        assert_eq!(text(&header).as_deref(), Some("Welcome"));
        assert_eq!(text(&footer).as_deref(), Some("megu"));

        // The encoded form is byte-identical to two chat_nbt components.
        let mut expect = chat_nbt("Welcome");
        expect.extend_from_slice(&chat_nbt("megu"));
        assert_eq!(wire, expect);

        // Trailing bytes and a truncated footer are rejected.
        let mut bad = wire.clone();
        bad.push(0);
        assert!(decode_tab_list(&mut &bad[..]).is_none());
        assert!(decode_tab_list(&mut &wire[..wire.len() - 1]).is_none());
    }
}
