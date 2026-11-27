// Lane P5e (#216): clean, typed, persistable models for the remaining Vanilla
// 1.21.1 block entity families. Included from player_mechanisms.rs into the
// crate root (same flat scope - no `mod`), so the `block_entity_*` NBT helpers
// and `crate::nbt::{Tag, NbtString}` resolve here.
//
// Each model owns exactly the fields the task pins, writes them under the real
// vanilla `saveAdditional` key names, and decodes fail-closed: a non-compound
// root, a present-but-mistyped field, or a malformed inventory entry answers
// `None` instead of a half-loaded entity. Fields absent from saved data take
// the vanilla defaults `BaseEntityBlock`/`loadAdditional` use.
//
// These are deliberately the simple, name-keyed shapes - item ids as `String`,
// not `BlockEntityItem` - so they can be persisted and tested without the live
// store. The live-wired families in the runtime keep their richer types.

/// One occupied `(item id, count)` entry in a vanilla container `Items` list.
type BeSlotItem = (String, u8);

/// Decode a vanilla `Items` list into a fixed slot array. `Items` absent is an
/// empty inventory; `Items` present but not a list, a non-compound entry, an
/// entry without a string `id`, or a `count` that is not a positive byte fails
/// closed. Like vanilla `ContainerHelper.loadAllItems`, an out-of-range `Slot`
/// is skipped rather than fatal, and a missing `Slot`/`count` reads as 0/1.
fn be_read_items<const N: usize>(tag: &Tag) -> Option<[Option<BeSlotItem>; N]> {
    let mut slots: [Option<BeSlotItem>; N] = std::array::from_fn(|_| None);
    let Some(items) = tag.get("Items") else {
        return Some(slots);
    };
    let entries = items.list()?;
    for entry in entries {
        if !matches!(entry, Tag::Compound(_)) {
            return None;
        }
        let slot = match entry.get("Slot") {
            None => 0,
            Some(value) => usize::try_from(nbt_int_val(value)?).ok()?,
        };
        let id = block_entity_str_field(entry, "id")?;
        let count = match entry.get("count") {
            None => 1,
            Some(value) => u8::try_from(nbt_int_val(value)?).ok()?,
        };
        if count == 0 {
            return None;
        }
        if let Some(slot) = slots.get_mut(slot) {
            *slot = Some((id, count));
        }
    }
    Some(slots)
}

/// Write occupied `(item, count)` slots in vanilla `Items` form: one compound
/// per occupied slot carrying `Slot` (byte), `id` (string) and `count` (int).
fn be_write_items(fields: &mut BlockEntityNbtFields, slots: &[Option<BeSlotItem>]) {
    let values = slots
        .iter()
        .enumerate()
        .filter_map(|(slot, item)| {
            let (id, count) = item.as_ref()?;
            Some(block_entity_compound([
                ("Slot", Tag::Byte(slot as i8)),
                ("id", block_entity_string(id)),
                ("count", Tag::Int(i32::from(*count))),
            ]))
        })
        .collect();
    block_entity_put(fields, "Items", block_entity_list(10, values));
}

/// Read one `{id, count}` item compound. Absent `key` is `None`; present but
/// not a compound, without a string `id`, or with a bad `count` fails closed.
fn be_read_item(tag: &Tag, key: &str) -> Option<Option<BeSlotItem>> {
    let Some(value) = tag.get(key) else {
        return Some(None);
    };
    let id = block_entity_str_field(value, "id")?;
    let count = match value.get("count") {
        None => 1,
        Some(count) => u8::try_from(nbt_int_val(count)?).ok()?,
    };
    if count == 0 {
        return None;
    }
    Some(Some((id, count)))
}

/// Write `{id, count}` for a present item, nothing for `None`.
fn be_write_item(fields: &mut BlockEntityNbtFields, key: &str, item: Option<&BeSlotItem>) {
    if let Some((id, count)) = item {
        block_entity_put(
            fields,
            key,
            block_entity_compound([
                ("id", block_entity_string(id)),
                ("count", Tag::Int(i32::from(*count))),
            ]),
        );
    }
}

/// A numeric field that must be an NBT number when present (`nbt_int_val`
/// covers every numeric width like `CompoundTag.getInt` does). Absent reads
/// as `default`.
fn be_num(tag: &Tag, key: &str, default: i64) -> Option<i64> {
    match tag.get(key) {
        None => Some(default),
        Some(value) => nbt_int_val(value),
    }
}

/// A string field that must be an NBT string that decodes when present.
fn be_str(tag: &Tag, key: &str) -> Option<Option<String>> {
    match tag.get(key) {
        None => Some(None),
        Some(Tag::String(value)) => Some(Some(value.to_utf8().ok()?)),
        Some(_) => None,
    }
}

/// `AbstractContainerMenu.getRedstoneSignalFromContainer`: each occupied slot
/// contributes `count / min(container limit, item max stack)`, the total is
/// scaled to 0..=14, and any fill bumps the answer to at least 1. Vanilla
/// `Container.getMaxStackSize(stack)` is `min(this.getMaxStackSize(),
/// stack.getMaxStackSize())`, so `container_max` is the entity's own stack
/// limit — 64 for the brewing stand, 1 for the chiseled bookshelf.
fn be_container_signal(
    slots: &[Option<BeSlotItem>],
    container_max: u8,
    item_max_stack: impl Fn(&str) -> u8,
) -> u8 {
    let mut fill = 0.0f32;
    for (id, count) in slots.iter().flatten() {
        let limit = item_max_stack(id).max(1);
        fill += f32::from(*count) / f32::from(limit.min(container_max));
    }
    let signal = (fill / slots.len().max(1) as f32 * 14.0).floor() as u8;
    signal + u8::from(fill > 0.0)
}

// ---------- Beacon (`minecraft:beacon`) ----------

/// `BeaconBlockEntity.updateLevels` (1.21.1): the number of complete pyramid
/// layers below the beacon, 0..=4. `is_base_block(dx, dy, dz)` answers whether
/// the block at that offset counts toward the pyramid (iron, gold, diamond,
/// emerald or netherite block in vanilla). `beacon_y`/`min_y` bound the scan:
/// `updateBase` stops at the dimension floor, so a layer reaching below
/// `min_y` can never be complete.
fn beacon_pyramid_levels(
    beacon_y: i32,
    min_y: i32,
    mut is_base_block: impl FnMut(i32, i32, i32) -> bool,
) -> u8 {
    let mut levels = 0;
    for depth in 1..=4_i32 {
        if beacon_y - depth < min_y {
            break;
        }
        let complete =
            (-depth..=depth).all(|x| (-depth..=depth).all(|z| is_base_block(x, -depth, z)));
        if !complete {
            break;
        }
        levels = depth as u8;
    }
    levels
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct BeaconBlockEntity {
    /// The saved `Levels` snapshot, always in 0..=4.
    pub(crate) levels: u8,
    /// `primary_effect` mob effect id, e.g. "minecraft:haste".
    pub(crate) primary_effect: Option<String>,
    /// `secondary_effect` mob effect id; only meaningful at level 4.
    pub(crate) secondary_effect: Option<String>,
}

impl BeaconBlockEntity {
    pub(crate) fn from_nbt(tag: &Tag) -> Option<Self> {
        let Tag::Compound(_) = tag else {
            return None;
        };
        let levels = u8::try_from(be_num(tag, "Levels", 0)?).ok()?.min(4);
        Some(Self {
            levels,
            primary_effect: be_str(tag, "primary_effect")?,
            secondary_effect: be_str(tag, "secondary_effect")?,
        })
    }

    pub(crate) fn to_nbt(&self) -> Tag {
        let mut fields = BlockEntityNbtFields::new();
        block_entity_put(&mut fields, "Levels", Tag::Int(i32::from(self.levels.min(4))));
        for (key, effect) in [
            ("primary_effect", &self.primary_effect),
            ("secondary_effect", &self.secondary_effect),
        ] {
            if let Some(effect) = effect {
                block_entity_put(&mut fields, key, block_entity_string(effect));
            }
        }
        Tag::Compound(fields)
    }

    /// `BeaconBlockEntity`: effect radius is `levels * 10 + 10` blocks.
    pub(crate) fn effect_range(&self) -> i32 {
        i32::from(self.levels.min(4)) * 10 + 10
    }

    /// `BeaconBlockEntity`: effect duration is `(9 + levels * 2)` seconds, in ticks.
    pub(crate) fn effect_duration_ticks(&self) -> i32 {
        (9 + i32::from(self.levels.min(4)) * 2) * 20
    }

    /// The secondary power only applies at a full four-layer pyramid, and a
    /// secondary matching the primary upgrades it to amplifier 1 instead.
    pub(crate) fn active_effects(&self) -> (Option<String>, Option<String>) {
        if self.levels < 4 {
            (self.primary_effect.clone(), None)
        } else {
            (self.primary_effect.clone(), self.secondary_effect.clone())
        }
    }
}

// ---------- Lectern (`minecraft:lectern`) ----------

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct LecternBlockEntity {
    /// Item id of the displayed book (`writable_book` / `written_book`).
    pub(crate) book_item: Option<String>,
    /// Current `Page`, zero-based.
    pub(crate) page: i32,
    /// Number of pages in the book; this name-keyed model keeps the count
    /// rather than the page list itself, persisted as that list's length.
    pub(crate) total_pages: i32,
}

impl LecternBlockEntity {
    fn book_component_key(item: &str) -> &'static str {
        if item.strip_prefix("minecraft:").unwrap_or(item) == "writable_book" {
            "minecraft:writable_book_content"
        } else {
            "minecraft:written_book_content"
        }
    }

    /// Page-count the book's content component carries; both book components
    /// store `pages` as a list whose length is the count.
    fn pages_in(tag: &Tag) -> i32 {
        let Some(book) = tag.get("Book") else {
            return 0;
        };
        let Some(Tag::Compound(components)) = book.get("components") else {
            return 0;
        };
        [
            "minecraft:written_book_content",
            "minecraft:writable_book_content",
        ]
        .iter()
        .find_map(|key| components.get(&NbtString::text(key)))
        .and_then(|content| content.get("pages"))
        .and_then(Tag::list)
        .map_or(0, |pages| pages.len() as i32)
    }

    pub(crate) fn from_nbt(tag: &Tag) -> Option<Self> {
        let Tag::Compound(_) = tag else {
            return None;
        };
        let book_item = be_read_item(tag, "Book")?.map(|(id, _)| id);
        Some(Self {
            book_item,
            page: i32::try_from(be_num(tag, "Page", 0)?).ok()?,
            total_pages: Self::pages_in(tag),
        })
    }

    pub(crate) fn to_nbt(&self) -> Tag {
        let mut fields = BlockEntityNbtFields::new();
        if let Some(item) = &self.book_item {
            let pages = std::iter::repeat_with(|| block_entity_string(""))
                .take(self.total_pages.max(0) as usize)
                .collect();
            let mut content = BlockEntityNbtFields::new();
            block_entity_put(&mut content, "pages", block_entity_list(8, pages));
            let mut components = BlockEntityNbtFields::new();
            block_entity_put(
                &mut components,
                Self::book_component_key(item),
                Tag::Compound(content),
            );
            block_entity_put(
                &mut fields,
                "Book",
                block_entity_compound([
                    ("id", block_entity_string(item)),
                    ("count", Tag::Int(1)),
                    ("components", Tag::Compound(components)),
                ]),
            );
            block_entity_put(&mut fields, "Page", Tag::Int(self.page));
        }
        Tag::Compound(fields)
    }

    /// `LecternBlockEntity` clamps a requested page into `0..total_pages - 1`,
    /// matching the menu's `BookViewed` clamp. Returns whether the page moved.
    pub(crate) fn set_page(&mut self, page: i32) -> bool {
        let page = page.max(0).min((self.total_pages - 1).max(0));
        let changed = page != self.page;
        self.page = page;
        changed
    }

    /// `LecternBlock`'s analog output: `floor(fraction * 14) + has_book`, where
    /// the fraction spans the book. No book reads as zero.
    pub(crate) fn comparator_signal(&self) -> u8 {
        if self.book_item.is_none() {
            return 0;
        }
        let fraction = if self.total_pages > 1 {
            self.page as f32 / (self.total_pages - 1) as f32
        } else {
            1.0
        };
        (fraction * 14.0).floor() as u8 + 1
    }
}

// ---------- Jukebox (`minecraft:jukebox`) ----------

/// The 1.21.1 `jukebox_song` registry's `comparator_output` for each vanilla
/// music disc item name.
fn jukebox_comparator_output(record: &str) -> Option<u8> {
    Some(match record.strip_prefix("minecraft:").unwrap_or(record) {
        "music_disc_13" => 1,
        "music_disc_cat" => 2,
        "music_disc_blocks" => 3,
        "music_disc_chirp" => 4,
        "music_disc_far" => 5,
        "music_disc_mall" => 6,
        "music_disc_mellohi" => 7,
        "music_disc_stal" => 8,
        "music_disc_strad" => 9,
        "music_disc_ward" => 10,
        "music_disc_11" | "music_disc_creator_music_box" => 11,
        "music_disc_wait" | "music_disc_creator" => 12,
        "music_disc_pigstep" | "music_disc_precipice" => 13,
        "music_disc_otherside" | "music_disc_relic" => 14,
        "music_disc_5" => 15,
        _ => return None,
    })
}

/// The datapack-driven `jukebox_song` lookup for a record item id. Vanilla
/// 1.21.1 resolves the comparator output through the item's
/// `jukebox_playable.song` component; every vanilla `music_disc_<path>` item
/// points at `minecraft:<path>`, which the name-keyed model derives by
/// stripping the `music_disc_` prefix. Returns the registry's
/// `comparator_output` when a datapack `jukebox_song` entry exists.
fn jukebox_datapack_comparator_output(record: &str) -> Option<u8> {
    let path = record.strip_prefix("minecraft:").unwrap_or(record);
    let song = format!("minecraft:{}", path.strip_prefix("music_disc_")?);
    let registry = dynamic_registries().lock().unwrap_or_else(|e| e.into_inner());
    registry
        .jukebox_song
        .get(&song)
        .and_then(|entry| u8::try_from(entry.comparator_output).ok())
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct JukeboxBlockEntity {
    /// Item id of the loaded record, e.g. "minecraft:music_disc_cat".
    pub(crate) record_item: Option<String>,
    /// `ticks_since_song_started` - ticks since the song began; 0 when the
    /// jukebox holds no record.
    pub(crate) ticks_since_song_started: u64,
}

impl JukeboxBlockEntity {
    pub(crate) fn from_nbt(tag: &Tag) -> Option<Self> {
        let Tag::Compound(_) = tag else {
            return None;
        };
        let record_item = be_read_item(tag, "RecordItem")?.map(|(id, _)| id);
        let ticks = match tag.get("ticks_since_song_started") {
            None => 0,
            Some(value) => u64::try_from(nbt_int_val(value)?).ok()?,
        };
        Some(Self {
            record_item,
            ticks_since_song_started: ticks,
        })
    }

    pub(crate) fn to_nbt(&self) -> Tag {
        let mut fields = BlockEntityNbtFields::new();
        be_write_item(
            &mut fields,
            "RecordItem",
            self.record_item.as_ref().map(|id| (id.clone(), 1)).as_ref(),
        );
        if self.record_item.is_some() {
            block_entity_put(
                &mut fields,
                "ticks_since_song_started",
                Tag::Long(self.ticks_since_song_started as i64),
            );
        }
        Tag::Compound(fields)
    }

    /// `JukeboxBlock`'s analog output: the song's `comparator_output` while a
    /// record is loaded, zero otherwise. The loaded datapack `jukebox_song`
    /// registry wins over the built-in table so datapack overrides apply.
    pub(crate) fn comparator_signal(&self) -> u8 {
        self.record_item
            .as_deref()
            .and_then(|record| {
                jukebox_datapack_comparator_output(record)
                    .or_else(|| jukebox_comparator_output(record))
            })
            .unwrap_or(0)
    }
}

// ---------- Chiseled bookshelf (`minecraft:chiseled_bookshelf`) ----------

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ChiseledBookshelfBlockEntity {
    /// Book item ids in slots 0..=5.
    pub(crate) slots: [Option<String>; 6],
    /// `lastInteractedSlot` — vanilla keeps a plain `int` that starts at `0`,
    /// so an untouched shelf already reads slot 0 for the comparator and NBT
    /// persists `0`, never a sentinel.
    pub(crate) last_interacted_slot: i32,
}

impl Default for ChiseledBookshelfBlockEntity {
    fn default() -> Self {
        Self {
            slots: Default::default(),
            last_interacted_slot: 0,
        }
    }
}

impl ChiseledBookshelfBlockEntity {
    pub(crate) fn from_nbt(tag: &Tag) -> Option<Self> {
        let Tag::Compound(_) = tag else {
            return None;
        };
        let items = be_read_items::<6>(tag)?;
        let mut slots: [Option<String>; 6] = std::array::from_fn(|_| None);
        for (slot, item) in slots.iter_mut().zip(items) {
            *slot = item.map(|(id, _)| id);
        }
        Some(Self {
            slots,
            last_interacted_slot: i32::try_from(be_num(tag, "last_interacted_slot", 0)?).ok()?,
        })
    }

    pub(crate) fn to_nbt(&self) -> Tag {
        let mut fields = BlockEntityNbtFields::new();
        let items: [Option<BeSlotItem>; 6] =
            std::array::from_fn(|i| self.slots[i].clone().map(|id| (id, 1)));
        be_write_items(&mut fields, &items);
        block_entity_put(
            &mut fields,
            "last_interacted_slot",
            Tag::Int(self.last_interacted_slot),
        );
        Tag::Compound(fields)
    }

    /// `ChiseledBookShelfBlock.useItemOn` inserts into the first occupied slot
    /// scanning the hit book's column; the model exposes the single-slot form
    /// and records the interaction for the comparator.
    pub(crate) fn insert_book(&mut self, slot: usize, item: &str) -> bool {
        let Some(target) = self.slots.get_mut(slot) else {
            return false;
        };
        if target.is_some() {
            return false;
        }
        *target = Some(item.strip_prefix("minecraft:").unwrap_or(item).to_string());
        self.last_interacted_slot = slot as i32;
        true
    }

    pub(crate) fn remove_book(&mut self, slot: usize) -> Option<String> {
        let book = self.slots.get_mut(slot)?.take()?;
        self.last_interacted_slot = slot as i32;
        Some(book)
    }

    /// `ChiseledBookShelfBlock`'s analog output is `lastInteractedSlot + 1`
    /// (1.21.1 `dqq.j() + 1`): an untouched shelf reads `1` because the field
    /// defaults to `0`, and slot 5 reads 6.
    pub(crate) fn comparator_signal(&self) -> u8 {
        (self.last_interacted_slot + 1).clamp(0, 15) as u8
    }

    /// `ChiseledBookShelfBlockEntity.getMaxStackSize` returns `1` (1.21.1
    /// `dqq.al_()`), so the shared container comparator formula divides every
    /// stored book by `min(1, item max)` — one full slot per book. The live
    /// block comparator uses `lastInteractedSlot + 1` instead; this is the
    /// container-menu signal, exercised by the parity tests; the container
    /// comparator lane consumes it once wired.
    pub(crate) fn container_signal(&self) -> u8 {
        let slots: [Option<BeSlotItem>; 6] =
            std::array::from_fn(|i| self.slots[i].clone().map(|id| (id, 1)));
        be_container_signal(&slots, 1, |_| 64)
    }
}

const _: fn(&ChiseledBookshelfBlockEntity) -> u8 = ChiseledBookshelfBlockEntity::container_signal;

// ---------- Decorated pot (`minecraft:decorated_pot`) ----------

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct DecoratedPotBlockEntity {
    /// Sherd item ids in `PotDecorations` order: back, left, right, front.
    /// An undecorated side is `minecraft:brick`.
    pub(crate) sherds: [String; 4],
    /// Item id of the stored item, if any.
    pub(crate) item_id: Option<String>,
    /// Stack count of the stored item.
    pub(crate) item_count: u8,
}

impl Default for DecoratedPotBlockEntity {
    fn default() -> Self {
        Self {
            sherds: std::array::from_fn(|_| "minecraft:brick".to_string()),
            item_id: None,
            item_count: 0,
        }
    }
}

impl DecoratedPotBlockEntity {
    pub(crate) fn from_nbt(tag: &Tag) -> Option<Self> {
        let Tag::Compound(_) = tag else {
            return None;
        };
        let mut pot = Self::default();
        if let Some(sherds) = tag.get("sherds") {
            let entries = sherds.list()?;
            if entries.len() != 4 {
                return None;
            }
            for (slot, entry) in pot.sherds.iter_mut().zip(entries) {
                let Tag::String(id) = entry else {
                    return None;
                };
                *slot = id.to_utf8().ok()?;
            }
        }
        if let Some((id, count)) = be_read_item(tag, "item")? {
            pot.item_id = Some(id);
            pot.item_count = count;
        }
        Some(pot)
    }

    pub(crate) fn to_nbt(&self) -> Tag {
        let mut fields = BlockEntityNbtFields::new();
        block_entity_put(
            &mut fields,
            "sherds",
            block_entity_list(
                8,
                self.sherds.iter().map(|id| block_entity_string(id)).collect(),
            ),
        );
        be_write_item(
            &mut fields,
            "item",
            self.item_id
                .as_ref()
                .map(|id| (id.clone(), self.item_count))
                .as_ref(),
        );
        Tag::Compound(fields)
    }

    /// `DecoratedPotBlock`'s analog output is the container formula: fullness
    /// of the single stored stack scaled to 1..=15, zero when empty.
    pub(crate) fn comparator_signal(&self, item_max_stack: u8) -> u8 {
        if self.item_id.is_none() || self.item_count == 0 {
            return 0;
        }
        let fill = f32::from(self.item_count) / f32::from(item_max_stack.max(1));
        (fill.min(1.0) * 14.0).floor() as u8 + 1
    }
}

// ---------- Campfire (`minecraft:campfire`) ----------

/// A campfire slot: `(item id, cooking_time, cooking_total_time)` in ticks.
type BeCampfireSlot = (String, u16, u16);

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct CampfireBlockEntity {
    /// Up to four cookable items with their progress and recipe times.
    pub(crate) slots: [Option<BeCampfireSlot>; 4],
}

impl CampfireBlockEntity {
    pub(crate) fn from_nbt(tag: &Tag) -> Option<Self> {
        let Tag::Compound(_) = tag else {
            return None;
        };
        let items = be_read_items::<4>(tag)?;
        fn times(tag: &Tag, key: &str) -> Option<[u16; 4]> {
            let Some(array) = tag.get(key) else {
                return Some([0; 4]);
            };
            let Tag::IntArray(values) = array else {
                return None;
            };
            let mut out = [0; 4];
            for (tick, &saved) in out.iter_mut().zip(values) {
                *tick = u16::try_from(saved).ok()?;
            }
            Some(out)
        }
        let cooking_times = times(tag, "CookingTimes")?;
        let cooking_total_times = times(tag, "CookingTotalTimes")?;
        let mut slots: [Option<BeCampfireSlot>; 4] = std::array::from_fn(|_| None);
        for (((slot, item), &time), &total) in slots
            .iter_mut()
            .zip(items)
            .zip(&cooking_times)
            .zip(&cooking_total_times)
        {
            *slot = item.map(|(id, _)| (id, time, total));
        }
        Some(Self { slots })
    }

    pub(crate) fn to_nbt(&self) -> Tag {
        let mut fields = BlockEntityNbtFields::new();
        let items: [Option<BeSlotItem>; 4] =
            std::array::from_fn(|i| self.slots[i].as_ref().map(|(id, _, _)| (id.clone(), 1)));
        be_write_items(&mut fields, &items);
        let times = |which: usize| -> Vec<i32> {
            self.slots
                .iter()
                .map(|slot| slot.as_ref().map_or(0, |s| i32::from(match which {
                    0 => s.1,
                    _ => s.2,
                })))
                .collect()
        };
        block_entity_put(&mut fields, "CookingTimes", Tag::IntArray(times(0)));
        block_entity_put(
            &mut fields,
            "CookingTotalTimes",
            Tag::IntArray(times(1)),
        );
        Tag::Compound(fields)
    }

    /// `CampfireBlockEntity.place`: take the first free slot for `total` ticks.
    #[cfg(test)]
    pub(crate) fn place(&mut self, item: &str, cooking_total_time: u16) -> Option<usize> {
        let slot = self.slots.iter().position(Option::is_none)?;
        self.slots[slot] = Some((item.to_string(), 0, cooking_total_time));
        Some(slot)
    }

    /// `CampfireBlockEntity.cookTick`/`cooldownTick`: a lit campfire advances
    /// each occupied slot one tick and finishes at its recipe time; an unlit
    /// one rolls progress back two ticks, clamped to the recipe time. Returns
    /// the slot index and item id of every slot that finished this tick - the
    /// caller maps the cooked result through the campfire recipe table.
    pub(crate) fn tick(&mut self, lit: bool) -> Vec<(usize, String)> {
        let mut finished = Vec::new();
        for (index, slot) in self.slots.iter_mut().enumerate() {
            let Some((item, time, total)) = slot else {
                continue;
            };
            if !lit {
                if *time > 0 {
                    *time = (*time).saturating_sub(2).min(*total);
                }
            } else {
                *time = time.saturating_add(1);
                if *time >= *total {
                    finished.push((index, std::mem::take(item)));
                    *slot = None;
                }
            }
        }
        finished
    }
}

// ---------- Brewing stand (`minecraft:brewing_stand`) ----------

#[cfg(test)]
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct BrewingStandBlockEntity {
    /// `(item id, count)` per slot: 0..=2 bottles, 3 ingredient, 4 fuel.
    pub(crate) slots: [Option<BeSlotItem>; 5],
    /// `BrewTime`: ticks left in the current brew (vanilla starts at 400).
    pub(crate) brew_time: u16,
    /// `Fuel`: brew cycles the loaded blaze powder still pays for.
    pub(crate) fuel: u8,
}

#[cfg(test)]
impl BrewingStandBlockEntity {
    pub(crate) fn from_nbt(tag: &Tag) -> Option<Self> {
        let Tag::Compound(_) = tag else {
            return None;
        };
        Some(Self {
            slots: be_read_items::<5>(tag)?,
            brew_time: u16::try_from(be_num(tag, "BrewTime", 0)?).ok()?,
            fuel: u8::try_from(be_num(tag, "Fuel", 0)?).ok()?,
        })
    }

    pub(crate) fn to_nbt(&self) -> Tag {
        let mut fields = BlockEntityNbtFields::new();
        be_write_items(&mut fields, &self.slots);
        block_entity_put(&mut fields, "BrewTime", Tag::Short(self.brew_time as i16));
        block_entity_put(&mut fields, "Fuel", Tag::Byte(self.fuel as i8));
        Tag::Compound(fields)
    }

    /// `BrewingStandBlock`'s analog output is the container formula over the
    /// five slots (`Container.getMaxStackSize` = 64). Potion stacks are
    /// unstackable in 1.21.1 and glass bottles stack to 16; everything else
    /// reads as a 64 stack.
    pub(crate) fn comparator_signal(&self) -> u8 {
        be_container_signal(&self.slots, 64, |item| {
            match item.strip_prefix("minecraft:").unwrap_or(item) {
                "potion" | "splash_potion" | "lingering_potion" => 1,
                "glass_bottle" | "honey_bottle" => 16,
                _ => 64,
            }
        })
    }
}

/// The live stand's next `BrewingStand.tick` as a pure predicate: true only
/// when the pass is guaranteed to leave every field untouched. It reads the
/// same derived state `tick` reads, so it is exact rather than conservative:
/// a queued powder burn (`fuel == 0 && blaze_powder > 0`) mutates, an active
/// brew (`brew_time > 0`) counts down, and `brewable && fuel > 0` starts one;
/// everything else returns `BrewingTick::Idle` without touching a field. A
/// fuel-empty stand can never start a brew, so only the powder queue gates
/// it. The fingerprint-sleep caller runs this after a real pass, while the
/// derived `bottles`/`ingredient`/`blaze_powder` are still freshly synced.
fn brewing_inert(stand: &BrewingStand) -> bool {
    if stand.brew_time != 0 {
        return false;
    }
    if stand.fuel == 0 {
        return stand.blaze_powder == 0;
    }
    let Some((ingredient, _count)) = stand.ingredient.filter(|&(_, count)| count > 0) else {
        return true;
    };
    stand
        .bottles
        .map(|bottle| brew_potion(bottle?, ingredient))
        .iter()
        .all(Option::is_none)
}

// ---------- Crafter (`minecraft:crafter`) ----------

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct CrafterBlockEntity {
    /// `(item id, count)` per crafting-grid slot 0..=8.
    pub(crate) slots: [Option<BeSlotItem>; 9],
    /// `disabled_slots` as a bitmask: bit N marks slot N toggled off.
    pub(crate) disabled_slots_mask: u16,
    /// `triggered`: high while the crafter holds a redstone charge.
    pub(crate) triggered: bool,
}

impl CrafterBlockEntity {
    pub(crate) fn from_nbt(tag: &Tag) -> Option<Self> {
        let Tag::Compound(_) = tag else {
            return None;
        };
        let mut disabled_slots_mask = 0u16;
        if let Some(disabled) = tag.get("disabled_slots") {
            let Tag::IntArray(values) = disabled else {
                return None;
            };
            for &slot in values {
                if let Ok(slot) = u16::try_from(slot) {
                    if slot < 9 {
                        disabled_slots_mask |= 1 << slot;
                    }
                }
            }
        }
        Some(Self {
            slots: be_read_items::<9>(tag)?,
            disabled_slots_mask,
            triggered: be_num(tag, "triggered", 0)? != 0,
        })
    }

    pub(crate) fn to_nbt(&self) -> Tag {
        let mut fields = BlockEntityNbtFields::new();
        be_write_items(&mut fields, &self.slots);
        let disabled: Vec<i32> = (0..9)
            .filter(|slot| self.disabled_slots_mask & (1 << slot) != 0)
            .collect();
        block_entity_put(&mut fields, "disabled_slots", Tag::IntArray(disabled));
        block_entity_put(
            &mut fields,
            "triggered",
            Tag::Byte(i8::from(self.triggered)),
        );
        Tag::Compound(fields)
    }

    /// Whether a slot accepts input (enabled) and is empty.
    pub(crate) fn slot_disabled(&self, slot: usize) -> bool {
        slot < 9 && self.disabled_slots_mask & (1 << slot) != 0
    }

    /// Toggle a slot; only empty slots can be toggled, like the vanilla UI.
    pub(crate) fn set_slot_disabled(&mut self, slot: usize, disabled: bool) -> bool {
        if !matches!(self.slots.get(slot), Some(None)) {
            return false;
        }
        if disabled {
            self.disabled_slots_mask |= 1 << slot;
        } else {
            self.disabled_slots_mask &= !(1 << slot);
        }
        true
    }

    /// `CrafterBlock`'s analog output: the number of slots that are occupied or
    /// disabled, 0..=9.
    pub(crate) fn comparator_signal(&self) -> u8 {
        self.slots
            .iter()
            .enumerate()
            .filter(|(slot, item)| self.slot_disabled(*slot) || item.is_some())
            .count() as u8
    }

    /// `CrafterBlockEntity` rising-edge detection: a newly powered crafter
    /// schedules its craft four ticks out. Returns true only on the edge.
    pub(crate) fn set_powered(&mut self, powered: bool) -> bool {
        let was_triggered = std::mem::replace(&mut self.triggered, powered);
        powered && !was_triggered
    }
}

// ---------- Monster spawner (`minecraft:mob_spawner`) ----------

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct MobSpawnerBlockEntity {
    /// `SpawnData.entity.id`, e.g. "minecraft:zombie"; `minecraft:pig` default.
    pub(crate) entity_id: String,
    /// `Delay`: ticks until the next spawn attempt; vanilla starts at 20.
    pub(crate) delay: u16,
    /// `MinSpawnDelay` / `MaxSpawnDelay`: the reroll interval (200/800 default).
    pub(crate) min_spawn_delay: u16,
    pub(crate) max_spawn_delay: u16,
    /// `SpawnCount`: entities per attempt (4 default).
    pub(crate) spawn_count: u8,
    /// `MaxNearbyEntities`: activation cap in `spawn_range` (6 default).
    pub(crate) max_nearby_entities: u8,
    /// `RequiredPlayerRange`: player distance that arms the spawner (16).
    pub(crate) required_player_range: u8,
    /// `SpawnRange`: horizontal half-extent of the spawn volume (4 default).
    pub(crate) spawn_range: u8,
}

impl Default for MobSpawnerBlockEntity {
    fn default() -> Self {
        Self {
            entity_id: "minecraft:pig".to_string(),
            delay: 20,
            min_spawn_delay: 200,
            max_spawn_delay: 800,
            spawn_count: 4,
            max_nearby_entities: 6,
            required_player_range: 16,
            spawn_range: 4,
        }
    }
}

impl MobSpawnerBlockEntity {
    pub(crate) fn from_nbt(tag: &Tag) -> Option<Self> {
        let Tag::Compound(_) = tag else {
            return None;
        };
        let mut spawner = Self::default();
        if let Some(spawn_data) = tag.get("SpawnData") {
            let entity = spawn_data.get("entity")?;
            spawner.entity_id = block_entity_str_field(entity, "id")?;
        }
        for (key, slot) in [
            ("Delay", &mut spawner.delay),
            ("MinSpawnDelay", &mut spawner.min_spawn_delay),
            ("MaxSpawnDelay", &mut spawner.max_spawn_delay),
        ] {
            *slot = u16::try_from(be_num(tag, key, i64::from(*slot))?).ok()?;
        }
        for (key, slot) in [
            ("SpawnCount", &mut spawner.spawn_count),
            ("MaxNearbyEntities", &mut spawner.max_nearby_entities),
            ("RequiredPlayerRange", &mut spawner.required_player_range),
            ("SpawnRange", &mut spawner.spawn_range),
        ] {
            *slot = u8::try_from(be_num(tag, key, i64::from(*slot))?).ok()?;
        }
        Some(spawner)
    }

    pub(crate) fn to_nbt(&self) -> Tag {
        let entity = block_entity_compound([("id", block_entity_string(&self.entity_id))]);
        let mut spawn_data = BlockEntityNbtFields::new();
        block_entity_put(&mut spawn_data, "entity", entity);
        let mut fields = BlockEntityNbtFields::new();
        block_entity_put(&mut fields, "SpawnData", Tag::Compound(spawn_data));
        for (key, value) in [
            ("Delay", self.delay),
            ("MinSpawnDelay", self.min_spawn_delay),
            ("MaxSpawnDelay", self.max_spawn_delay),
        ] {
            block_entity_put(&mut fields, key, Tag::Short(value as i16));
        }
        for (key, value) in [
            ("SpawnCount", self.spawn_count),
            ("MaxNearbyEntities", self.max_nearby_entities),
            ("RequiredPlayerRange", self.required_player_range),
            ("SpawnRange", self.spawn_range),
        ] {
            block_entity_put(&mut fields, key, Tag::Short(i16::from(value)));
        }
        Tag::Compound(fields)
    }

    /// `BaseSpawner.delay`: reroll `Delay` uniformly in
    /// `[min_spawn_delay, max_spawn_delay)` after a spawn attempt.
    /// `random_0_to_1` is one draw on `[0, 1)` from the world RNG.
    pub(crate) fn reset_delay(&mut self, random_0_to_1: f32) {
        let (min, max) = (i64::from(self.min_spawn_delay), i64::from(self.max_spawn_delay));
        self.delay = if max <= min {
            min
        } else {
            min + (random_0_to_1.clamp(0.0, 1.0 - f32::EPSILON) * (max - min) as f32) as i64
        } as u16;
    }

    /// One `serverTick` step: `Delay` counts down, and reaching zero is the
    /// spawn attempt tick. Returns true on the attempt.
    pub(crate) fn delay_tick(&mut self) -> bool {
        if self.delay == 0 {
            return true;
        }
        self.delay -= 1;
        false
    }

    /// `BaseSpawner.isNearPlayer`: `dx/dy/dz` is the player's offset from the
    /// block's centre (`pos + 0.5` on each axis); the spawner arms while the
    /// player is within `required_player_range`.
    pub(crate) fn player_in_range(&self, dx: f64, dy: f64, dz: f64) -> bool {
        let range = f64::from(self.required_player_range);
        dx * dx + dy * dy + dz * dz <= range * range
    }
}

// ---------- Conduit (`minecraft:conduit`) ----------

/// `ConduitBlockEntity` (1.21.1 `dqt`) tuning, all JAR-verified: the frame
/// scan runs every `CONDUIT_RESCAN_PERIOD_TICKS` (40) game ticks; `active`
/// needs `CONDUIT_MIN_ACTIVE_FRAME` (16) frame blocks and `hunting`
/// `CONDUIT_MIN_KILL_FRAME` (42 — a complete three-plane cross). The effect
/// radius is `(frame_blocks / 7) * 16` (32 at 16 blocks, 96 at 42), the
/// `conduit_power` grant lasts 260 ticks, and a hunting conduit deals
/// `CONDUIT_ATTACK_DAMAGE` (4.0) magic damage while its target stays inside
/// the 8.0-block `closerThan` bubble (dropped otherwise).
pub(crate) const CONDUIT_MIN_ACTIVE_FRAME: usize = 16;
pub(crate) const CONDUIT_MIN_KILL_FRAME: usize = 42;
pub(crate) const CONDUIT_RESCAN_PERIOD_TICKS: i64 = 40;
pub(crate) const CONDUIT_EFFECT_DURATION_TICKS: i32 = 260;
pub(crate) const CONDUIT_ATTACK_DAMAGE: f32 = 4.0;
/// `BlockPos.closerThan(target, 8.0)` as a squared distance.
pub(crate) const CONDUIT_ATTACK_RANGE_SQ: f64 = 64.0;

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct ConduitState {
    /// `active` (field `j`): the last frame scan found at least 16 blocks.
    pub(crate) active: bool,
    /// `hunting` (field `k`): the last frame scan found at least 42 blocks.
    pub(crate) hunting: bool,
    /// Frame blocks counted by the last scan — vanilla keeps the `List` size;
    /// the model caches the count the effect range and hunt gate read.
    pub(crate) frame_blocks: u16,
    /// The hunted mob's entity id (vanilla caches the `Mob` reference and a
    /// pending `UUID`; the name-keyed model keeps the id string).
    pub(crate) target: Option<String>,
}

impl ConduitState {
    /// `ConduitBlockEntity.updateShape`'s frame count. The 3x3x3 interior must
    /// be all water — `is_water(dx, dy, dz)` — or the scan fails with 0. The
    /// 42 candidate cells are the three perpendicular cross planes through the
    /// conduit: offsets in `[-2, 2]^3` where one axis is `0` and another has
    /// `abs == 2` (edge centres and face centres of the cross, never corners).
    /// `is_frame(dx, dy, dz)` answers whether the cell holds prismarine,
    /// prismarine bricks, sea lantern or dark prismarine. Returns the number
    /// of frame blocks found.
    pub(crate) fn scan_frame_blocks(
        mut is_water: impl FnMut(i32, i32, i32) -> bool,
        mut is_frame: impl FnMut(i32, i32, i32) -> bool,
    ) -> usize {
        for y in -1..=1 {
            for x in -1..=1 {
                for z in -1..=1 {
                    if !is_water(x, y, z) {
                        return 0;
                    }
                }
            }
        }
        let mut count = 0;
        for x in -2_i32..=2_i32 {
            for y in -2_i32..=2_i32 {
                for z in -2_i32..=2_i32 {
                    let (ax, ay, az) = (x.abs(), y.abs(), z.abs());
                    if ax <= 1 && ay <= 1 && az <= 1 {
                        continue;
                    }
                    let on_cross = (x == 0 && (ay == 2 || az == 2))
                        || (y == 0 && (ax == 2 || az == 2))
                        || (z == 0 && (ax == 2 || ay == 2));
                    if on_cross && is_frame(x, y, z) {
                        count += 1;
                    }
                }
            }
        }
        count
    }

    /// The refresh gate: `level.getGameTime() % 40 == 0`.
    pub(crate) fn rescan_due(game_time: i64) -> bool {
        game_time % CONDUIT_RESCAN_PERIOD_TICKS == 0
    }

    /// Commit a fresh scan: threshold flags plus the cached count the effect
    /// range and the hunt gate read until the next rescan.
    pub(crate) fn commit_scan(&mut self, frame_blocks: usize) {
        self.active = frame_blocks >= CONDUIT_MIN_ACTIVE_FRAME;
        self.hunting = frame_blocks >= CONDUIT_MIN_KILL_FRAME;
        self.frame_blocks = frame_blocks.min(usize::from(u16::MAX)) as u16;
    }

    /// `ConduitBlockEntity`'s effect radius: `(frame_blocks / 7) * 16` —
    /// 32 at the 16-block minimum, 96 at the full 42-block frame.
    pub(crate) fn effect_range(&self) -> i32 {
        i32::from(self.frame_blocks) / 7 * 16
    }

    /// The `conduit_power` grant pass: players within `effect_range` of the
    /// conduit block centre who are in water or rain. `players` is the
    /// caller-ordered `(x, y, z, in_water_or_rain)` list; returns the indices
    /// to grant `CONDUIT_EFFECT_DURATION_TICKS` (260) ticks of amplifier 0.
    pub(crate) fn effect_targets(
        &self,
        conduit: (i32, i32, i32),
        players: &[(f64, f64, f64, bool)],
    ) -> Vec<usize> {
        if !self.active {
            return Vec::new();
        }
        let range = f64::from(self.effect_range());
        let center = [
            f64::from(conduit.0) + 0.5,
            f64::from(conduit.1) + 0.5,
            f64::from(conduit.2) + 0.5,
        ];
        let range_sq = range * range;
        players
            .iter()
            .enumerate()
            .filter(|(_, &(x, y, z, wet))| {
                wet
                    && (x - center[0]).powi(2) + (y - center[1]).powi(2)
                        + (z - center[2]).powi(2)
                        < range_sq
            })
            .map(|(index, _)| index)
            .collect()
    }

    /// One hunting pass (`a(dcw,jd,dtc,List,dqt)`): with fewer than 42 frame
    /// blocks the target is dropped; a live target inside the 8.0
    /// `closerThan` bubble is kept and takes `CONDUIT_ATTACK_DAMAGE` magic
    /// damage this pass; otherwise the nearest living candidate inside the
    /// inflated search box becomes the target. `candidates` is the caller's
    /// `(entity_id, alive, distance_sq_to_conduit)` hostile list; returns the
    /// `(entity id, damage)` pair to apply, if any. Vanilla picks the new
    /// target uniformly at random; the name-keyed model picks the nearest
    /// deterministically.
    pub(crate) fn hunt_tick(
        &mut self,
        candidates: &[(String, bool, f64)],
    ) -> Option<(String, f32)> {
        if self.frame_blocks < CONDUIT_MIN_KILL_FRAME as u16 {
            self.target = None;
            return None;
        }
        if let Some(target) = self.target.clone() {
            let kept = candidates.iter().any(|(id, alive, distance_sq)| {
                *id == target && *alive && *distance_sq < CONDUIT_ATTACK_RANGE_SQ
            });
            if !kept {
                // Vanilla nulls the target at the end of the pass and only
                // re-acquires on the next hunting tick.
                self.target = None;
                return None;
            }
            return Some((target, CONDUIT_ATTACK_DAMAGE));
        }
        let mut best: Option<&(String, bool, f64)> = None;
        for candidate in candidates {
            if !candidate.1 {
                continue;
            }
            if best.is_none_or(|b| candidate.2 < b.2) {
                best = Some(candidate);
            }
        }
        let Some((id, _, distance_sq)) = best else {
            return None;
        };
        // The pick passes through the same `closerThan(8.0)` + alive check a
        // kept target does; outside the bubble it is dropped undamaged.
        if *distance_sq >= CONDUIT_ATTACK_RANGE_SQ {
            return None;
        }
        self.target = Some(id.clone());
        Some((id.clone(), CONDUIT_ATTACK_DAMAGE))
    }

    pub(crate) fn from_nbt(tag: &Tag) -> Option<Self> {
        let Tag::Compound(_) = tag else {
            return None;
        };
        // Vanilla writes the hunted mob's UUID int-array under `Target`; the
        // name-keyed model persists the entity id string instead.
        let target = be_str(tag, "Target")?;
        Some(Self {
            active: be_num(tag, "active", 0)? != 0,
            hunting: be_num(tag, "hunting", 0)? != 0,
            frame_blocks: u16::try_from(be_num(tag, "frame_blocks", 0)?).ok()?,
            target,
        })
    }

    pub(crate) fn to_nbt(&self) -> Tag {
        let mut fields = BlockEntityNbtFields::new();
        block_entity_put(&mut fields, "active", Tag::Byte(i8::from(self.active)));
        block_entity_put(&mut fields, "hunting", Tag::Byte(i8::from(self.hunting)));
        block_entity_put(&mut fields, "frame_blocks", Tag::Short(self.frame_blocks as i16));
        if let Some(target) = &self.target {
            block_entity_put(&mut fields, "Target", block_entity_string(target));
        }
        Tag::Compound(fields)
    }
}

// ---------- Beehive / bee nest (`minecraft:beehive`, `minecraft:bee_nest`) ----------

/// `BeehiveBlockEntity` (1.21.1 `dqe`) constants: at most
/// `BEEHIVE_MAX_OCCUPANTS` (3) bees, `BEEHIVE_MAX_HONEY_LEVEL` (5) honey
/// levels, and a bee leaves after `BEEHIVE_OCCUPATION_TICKS` (600) ticks —
/// `BEEHIVE_NECTAR_OCCUPATION_TICKS` (2400) when it arrived with nectar.
pub(crate) const BEEHIVE_MAX_OCCUPANTS: usize = 3;
pub(crate) const BEEHIVE_MAX_HONEY_LEVEL: u8 = 5;
pub(crate) const BEEHIVE_OCCUPATION_TICKS: u32 = 600;
pub(crate) const BEEHIVE_NECTAR_OCCUPATION_TICKS: u32 = 2400;

/// One `BeehiveBlockEntity.BeeData` occupant in the name-keyed model.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct BeeOccupant {
    /// The bee's entity id, e.g. `minecraft:bee`.
    pub(crate) entity_id: String,
    /// `min_occupation_ticks` snapshot taken at entry (600 or 2400).
    pub(crate) min_occupation_ticks: u32,
    /// `ticks_in_hive` — incremented once per hive tick.
    pub(crate) ticks_in_hive: u32,
    /// Whether the bee arrived carrying nectar (drives the honey bonus).
    pub(crate) has_nectar: bool,
}

/// What a harvest (`shears` / `glass_bottle` at honey level 5) produces and
/// what happens to the occupants: vanilla empties the hive and the bees leave
/// angry unless a campfire smokes below.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct BeehiveHarvest {
    /// Item id of the yield — `minecraft:honeycomb` or `minecraft:honey_bottle`.
    pub(crate) item: &'static str,
    /// Stack count — 3 honeycomb from shears, 1 honey bottle from a bottle.
    pub(crate) count: u8,
    /// Occupants released by the harvest (empty hive afterwards).
    pub(crate) released: Vec<BeeOccupant>,
    /// Whether the released bees turn hostile (`!campfire_below`).
    pub(crate) angers_bees: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct BeehiveState {
    /// `bees` — at most `BEEHIVE_MAX_OCCUPANTS` occupants.
    pub(crate) occupants: Vec<BeeOccupant>,
    /// The `honey_level` blockstate mirrored into the entity for the
    /// comparator and harvest gates (vanilla keeps it on the block).
    pub(crate) honey_level: u8,
}

impl BeehiveState {
    /// `BeehiveBlockEntity.tryEnterHive`: a bee enters only while fewer than
    /// three occupants are inside; `has_nectar` picks the 2400-tick occupation
    /// time over the plain 600.
    pub(crate) fn try_enter_hive(&mut self, entity_id: &str, has_nectar: bool) -> bool {
        if self.occupants.len() >= BEEHIVE_MAX_OCCUPANTS {
            return false;
        }
        self.occupants.push(BeeOccupant {
            entity_id: entity_id.to_string(),
            min_occupation_ticks: if has_nectar {
                BEEHIVE_NECTAR_OCCUPATION_TICKS
            } else {
                BEEHIVE_OCCUPATION_TICKS
            },
            ticks_in_hive: 0,
            has_nectar,
        });
        true
    }

    /// `BeehiveBlockEntity.tickOccupants`: every occupant ages one tick and a
    /// bee whose `ticks_in_hive` passed `min_occupation_ticks` leaves when
    /// `can_leave` (daylight and no rain, computed by the caller). Each
    /// released nectar bee raises `honey_level` by 1 — or by 2 on the
    /// `nectar_bonus_roll < 0.01` (~1%) roll — capped at
    /// `BEEHIVE_MAX_HONEY_LEVEL`. Returns the released occupants in order.
    pub(crate) fn tick_occupants(
        &mut self,
        can_leave: bool,
        nectar_bonus_roll: f32,
    ) -> Vec<BeeOccupant> {
        let mut released = Vec::new();
        let mut index = 0;
        while index < self.occupants.len() {
            let occupant = &mut self.occupants[index];
            occupant.ticks_in_hive += 1;
            if occupant.ticks_in_hive > occupant.min_occupation_ticks && can_leave {
                let occupant = self.occupants.remove(index);
                if occupant.has_nectar && self.honey_level < BEEHIVE_MAX_HONEY_LEVEL {
                    let bonus = if nectar_bonus_roll < 0.01 { 2 } else { 1 };
                    self.honey_level = (self.honey_level + bonus).min(BEEHIVE_MAX_HONEY_LEVEL);
                }
                released.push(occupant);
            } else {
                index += 1;
            }
        }
        released
    }

    /// `BeehiveBlock`'s shears harvest: requires `honey_level == 5`, yields 3
    /// `minecraft:honeycomb`, resets the level and empties the hive. The
    /// released bees anger unless `campfire_below`.
    pub(crate) fn harvest_shears(&mut self, campfire_below: bool) -> Option<BeehiveHarvest> {
        self.harvest("minecraft:honeycomb", 3, campfire_below)
    }

    /// `BeehiveBlock`'s bottle harvest: same gates, yields 1
    /// `minecraft:honey_bottle`.
    pub(crate) fn harvest_bottle(&mut self, campfire_below: bool) -> Option<BeehiveHarvest> {
        self.harvest("minecraft:honey_bottle", 1, campfire_below)
    }

    fn harvest(
        &mut self,
        item: &'static str,
        count: u8,
        campfire_below: bool,
    ) -> Option<BeehiveHarvest> {
        if self.honey_level < BEEHIVE_MAX_HONEY_LEVEL {
            return None;
        }
        self.honey_level = 0;
        Some(BeehiveHarvest {
            item,
            count,
            released: std::mem::take(&mut self.occupants),
            angers_bees: !campfire_below,
        })
    }

    /// `BeehiveBlock.playerWillDestroy`/`onRemove`: with Silk Touch the
    /// occupants move into the dropped item (preserved here for the caller to
    /// attach) and stay calm; without it they are released and anger unless a
    /// campfire smokes below.
    pub(crate) fn on_break(
        &mut self,
        silk_touch: bool,
        campfire_below: bool,
    ) -> (Vec<BeeOccupant>, Vec<BeeOccupant>, bool) {
        let occupants = std::mem::take(&mut self.occupants);
        if silk_touch {
            (occupants, Vec::new(), false)
        } else {
            let angers = !campfire_below;
            (Vec::new(), occupants, angers)
        }
    }

    /// `BeehiveBlock.getAnalogOutputSignal`: the honey level itself, 0..=5.
    pub(crate) fn comparator_signal(&self) -> u8 {
        self.honey_level.min(BEEHIVE_MAX_HONEY_LEVEL)
    }

    pub(crate) fn from_nbt(tag: &Tag) -> Option<Self> {
        let Tag::Compound(_) = tag else {
            return None;
        };
        let mut state = Self::default();
        state.honey_level =
            u8::try_from(be_num(tag, "honey_level", 0)?).ok()?.min(BEEHIVE_MAX_HONEY_LEVEL);
        if let Some(bees) = tag.get("bees") {
            let entries = bees.list()?;
            if entries.len() > BEEHIVE_MAX_OCCUPANTS {
                return None;
            }
            for entry in entries {
                let entity_data = entry.get("entity_data")?;
                let entity_id = block_entity_str_field(entity_data, "id")?;
                let has_nectar = match entry.get("has_nectar") {
                    None => false,
                    Some(value) => nbt_int_val(value)? != 0,
                };
                state.occupants.push(BeeOccupant {
                    entity_id,
                    min_occupation_ticks: u32::try_from(
                        be_num(entry, "min_occupation_ticks", 0)?,
                    )
                    .ok()?,
                    ticks_in_hive: u32::try_from(be_num(entry, "ticks_in_hive", 0)?).ok()?,
                    has_nectar,
                });
            }
        }
        Some(state)
    }

    pub(crate) fn to_nbt(&self) -> Tag {
        let mut fields = BlockEntityNbtFields::new();
        block_entity_put(
            &mut fields,
            "honey_level",
            Tag::Byte(self.honey_level.min(BEEHIVE_MAX_HONEY_LEVEL) as i8),
        );
        if !self.occupants.is_empty() {
            let bees = self
                .occupants
                .iter()
                .map(|occupant| {
                    block_entity_compound([
                        (
                            "entity_data",
                            block_entity_compound([("id", block_entity_string(&occupant.entity_id))]),
                        ),
                        (
                            "min_occupation_ticks",
                            Tag::Int(occupant.min_occupation_ticks as i32),
                        ),
                        ("ticks_in_hive", Tag::Int(occupant.ticks_in_hive as i32)),
                        ("has_nectar", Tag::Byte(i8::from(occupant.has_nectar))),
                    ])
                })
                .collect();
            block_entity_put(&mut fields, "bees", block_entity_list(10, bees));
        }
        Tag::Compound(fields)
    }
}

const _: fn(&mut BeehiveState, &str, bool) -> bool = BeehiveState::try_enter_hive;
const _: fn(&mut BeehiveState, bool, bool) -> (Vec<BeeOccupant>, Vec<BeeOccupant>, bool) =
    BeehiveState::on_break;

// Live world-mechanism ownership. All eleven families share the same
// dimension key, including the brewing stand's component-aware potion
// representation.
// Accessors recover poisoned mutexes at each call site.
#[derive(Debug)]
pub(crate) enum VanillaBlockEntity {
    BrewingStand(BrewingStand),
    Crafter(CrafterBlockEntity),
    Campfire(CampfireBlockEntity),
    Jukebox(JukeboxBlockEntity),
    MobSpawner(MobSpawnerBlockEntity),
    Lectern(LecternBlockEntity),
    ChiseledBookshelf(ChiseledBookshelfBlockEntity),
    DecoratedPot(DecoratedPotBlockEntity),
    Beacon(BeaconBlockEntity),
    Conduit(ConduitState),
    Beehive(BeehiveState),
}

type WorldBlockEntityRegistry = std::sync::Mutex<
    std::collections::HashMap<(DimId, i32, i32, i32), VanillaBlockEntity>,
>;

static WORLD_BLOCK_ENTITY_REGISTRY: std::sync::OnceLock<WorldBlockEntityRegistry> =
    std::sync::OnceLock::new();

fn world_block_entity_registry() -> &'static WorldBlockEntityRegistry {
    WORLD_BLOCK_ENTITY_REGISTRY.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

#[cfg(test)]
mod p5e_parity_block_entity_tests {
    use super::*;

    fn roundtrip<T>(entity: &T, to: impl Fn(&T) -> Tag, from: impl Fn(&Tag) -> Option<T>) -> T
    where
        T: PartialEq + std::fmt::Debug,
    {
        let tag = to(entity);
        let bytes = nbt::write_named(&NbtString::text("be"), &tag).unwrap();
        let (_, decoded) = nbt::read_named(&bytes).unwrap();
        from(&decoded).unwrap()
    }

    #[test]
    fn p5e_parity_beacon_roundtrip_and_pyramid_levels() {
        let beacon = BeaconBlockEntity {
            levels: 4,
            primary_effect: Some("minecraft:haste".into()),
            secondary_effect: Some("minecraft:regeneration".into()),
        };
        let decoded = roundtrip(&beacon, BeaconBlockEntity::to_nbt, BeaconBlockEntity::from_nbt);
        assert_eq!(decoded, beacon);
        assert_eq!(decoded.effect_range(), 50);
        assert_eq!(decoded.effect_duration_ticks(), 340);
        assert_eq!(
            decoded.active_effects(),
            (Some("minecraft:haste".into()), Some("minecraft:regeneration".into()))
        );
        let low = BeaconBlockEntity {
            levels: 2,
            primary_effect: Some("minecraft:speed".into()),
            secondary_effect: Some("minecraft:jump_boost".into()),
        };
        assert_eq!(
            low.active_effects(),
            (Some("minecraft:speed".into()), None)
        );
        // Pyramid scan: full 9x9 only -> level 1; every layer -> 4; gap -> 0.
        let solid = |depth: i32| move |_: i32, dy: i32, _: i32| dy >= -depth;
        assert_eq!(beacon_pyramid_levels(64, -64, solid(1)), 1);
        assert_eq!(beacon_pyramid_levels(64, -64, solid(4)), 4);
        assert_eq!(beacon_pyramid_levels(64, -64, |_, _, _| false), 0);
        // A layer-2 hole caps the level at 1 even with deeper layers solid.
        assert_eq!(
            beacon_pyramid_levels(64, -64, |x, dy, z| !(dy == -2 && x == 0 && z == 0)),
            1
        );
        // `updateBase` stops at the dimension floor: a beacon at y=2 over
        // min_y=0 can never complete the depth-3 layer, even when solid.
        assert_eq!(beacon_pyramid_levels(2, 0, |_, _, _| true), 2);
        assert_eq!(beacon_pyramid_levels(4, 0, |_, _, _| true), 4);
        // Fail closed on non-compound roots and mistyped fields.
        assert!(BeaconBlockEntity::from_nbt(&Tag::Int(3)).is_none());
        let bad = block_entity_compound([("Levels", block_entity_string("high"))]);
        assert!(BeaconBlockEntity::from_nbt(&bad).is_none());
        // A saved level above 4 clamps like the runtime model.
        let over = block_entity_compound([("Levels", Tag::Int(9))]);
        assert_eq!(BeaconBlockEntity::from_nbt(&over).unwrap().levels, 4);
    }

    #[test]
    fn p5e_parity_lectern_roundtrip_page_clamp_and_signal() {
        let lectern = LecternBlockEntity {
            book_item: Some("minecraft:written_book".into()),
            page: 3,
            total_pages: 7,
        };
        let decoded = roundtrip(&lectern, LecternBlockEntity::to_nbt, LecternBlockEntity::from_nbt);
        assert_eq!(decoded, lectern);
        // Vanilla fraction: 3 / (7 - 1) * 14 = 7, plus the has-book bump.
        assert_eq!(decoded.comparator_signal(), 8);
        let mut first = decoded.clone();
        assert!(first.set_page(0));
        assert_eq!(first.page, 0);
        assert!(first.set_page(99));
        assert_eq!(first.page, 6);
        assert!(!first.set_page(6));
        assert_eq!(first.comparator_signal(), 15);
        let empty = LecternBlockEntity::default();
        assert_eq!(empty.comparator_signal(), 0);
        // Fail closed on a mistyped Book.
        let bad = block_entity_compound([("Book", Tag::Int(1))]);
        assert!(LecternBlockEntity::from_nbt(&bad).is_none());
        assert!(LecternBlockEntity::from_nbt(&Tag::Int(0)).is_none());
    }

    #[test]
    fn p5e_parity_jukebox_roundtrip_and_signal_table() {
        let jukebox = JukeboxBlockEntity {
            record_item: Some("minecraft:music_disc_cat".into()),
            ticks_since_song_started: 740,
        };
        let decoded = roundtrip(&jukebox, JukeboxBlockEntity::to_nbt, JukeboxBlockEntity::from_nbt);
        assert_eq!(decoded, jukebox);
        assert_eq!(decoded.comparator_signal(), 2);
        assert_eq!(JukeboxBlockEntity::default().comparator_signal(), 0);
        // Every vanilla disc maps to its jukebox_song comparator_output.
        for (disc, signal) in [
            ("music_disc_13", 1),
            ("music_disc_stal", 8),
            ("music_disc_ward", 10),
            ("music_disc_11", 11),
            ("music_disc_creator", 12),
            ("music_disc_pigstep", 13),
            ("music_disc_precipice", 13),
            ("music_disc_otherside", 14),
            ("music_disc_relic", 14),
            ("music_disc_5", 15),
        ] {
            assert_eq!(jukebox_comparator_output(disc), Some(signal), "{disc}");
        }
        assert_eq!(jukebox_comparator_output("apple"), None);
        let bad = block_entity_compound([("ticks_since_song_started", Tag::Long(-4))]);
        assert!(JukeboxBlockEntity::from_nbt(&bad).is_none());
    }

    #[test]
    fn p5e_parity_chiseled_bookshelf_roundtrip_and_last_slot() {
        let mut shelf = ChiseledBookshelfBlockEntity::default();
        // Vanilla `lastInteractedSlot` is an `int` defaulting to 0, so an
        // untouched shelf reads `0 + 1 = 1` (`dqq.j() + 1` in 1.21.1).
        assert_eq!(shelf.last_interacted_slot, 0);
        assert_eq!(shelf.comparator_signal(), 1);
        assert!(shelf.insert_book(0, "minecraft:book"));
        assert!(shelf.insert_book(4, "written_book"));
        assert!(!shelf.insert_book(4, "book")); // occupied
        assert!(!shelf.insert_book(6, "book")); // out of range
        assert_eq!(shelf.comparator_signal(), 5); // slot 4 + 1
        assert_eq!(shelf.remove_book(4), Some("written_book".into()));
        assert_eq!(shelf.comparator_signal(), 5);
        for slot in 0..6 {
            let mut one = ChiseledBookshelfBlockEntity::default();
            assert!(one.insert_book(slot, "minecraft:book"));
            assert_eq!(one.comparator_signal(), slot as u8 + 1);
        }
        let decoded = roundtrip(
            &shelf,
            ChiseledBookshelfBlockEntity::to_nbt,
            ChiseledBookshelfBlockEntity::from_nbt,
        );
        assert_eq!(decoded, shelf);
        // Vanilla persists `last_interacted_slot` unconditionally as an int;
        // untouched writes `0`, never a sentinel.
        let nbt = ChiseledBookshelfBlockEntity::default().to_nbt();
        assert_eq!(nbt.get("last_interacted_slot").and_then(Tag::int), Some(0));
        let decoded_default = roundtrip(
            &ChiseledBookshelfBlockEntity::default(),
            ChiseledBookshelfBlockEntity::to_nbt,
            ChiseledBookshelfBlockEntity::from_nbt,
        );
        assert_eq!(decoded_default.comparator_signal(), 1);
        assert_eq!(decoded_default.last_interacted_slot, 0);
        // A saved out-of-range slot index is kept verbatim like vanilla's raw
        // int read; the comparator still answers `value + 1`.
        let bad_slot = block_entity_compound([("last_interacted_slot", Tag::Int(9))]);
        let loaded = ChiseledBookshelfBlockEntity::from_nbt(&bad_slot).unwrap();
        assert_eq!(loaded.last_interacted_slot, 9);
        assert_eq!(loaded.comparator_signal(), 10);
        assert!(ChiseledBookshelfBlockEntity::from_nbt(&Tag::Int(0)).is_none());
        // `getMaxStackSize` = 1: each occupied slot is a full container share.
        let mut full = ChiseledBookshelfBlockEntity::default();
        for slot in 0..6 {
            assert!(full.insert_book(slot, "minecraft:book"));
        }
        assert_eq!(full.last_interacted_slot, 5);
        assert_eq!(full.comparator_signal(), 6);
        // fill = 6 * (1 / min(1, 64)) over 6 slots -> floor(6/6*14)+1 = 15.
        assert_eq!(full.container_signal(), 15);
        assert_eq!(ChiseledBookshelfBlockEntity::default().container_signal(), 0);
    }

    #[test]
    fn p5e_parity_decorated_pot_roundtrip() {
        let pot = DecoratedPotBlockEntity {
            sherds: [
                "minecraft:brick".into(),
                "minecraft:archer_pottery_sherd".into(),
                "minecraft:prize_pottery_sherd".into(),
                "minecraft:brick".into(),
            ],
            item_id: Some("minecraft:diamond".into()),
            item_count: 16,
        };
        let decoded = roundtrip(&pot, DecoratedPotBlockEntity::to_nbt, DecoratedPotBlockEntity::from_nbt);
        assert_eq!(decoded, pot);
        assert_eq!(decoded.comparator_signal(64), 4); // floor(16/64*14)+1
        assert_eq!(decoded.comparator_signal(16), 15);
        assert_eq!(DecoratedPotBlockEntity::default().comparator_signal(64), 0);
        // `sherds` must hold exactly four strings.
        let short = block_entity_compound([(
            "sherds",
            block_entity_list(8, vec![block_entity_string("minecraft:brick")]),
        )]);
        assert!(DecoratedPotBlockEntity::from_nbt(&short).is_none());
        let wrong = block_entity_compound([("sherds", Tag::Int(1))]);
        assert!(DecoratedPotBlockEntity::from_nbt(&wrong).is_none());
        assert!(DecoratedPotBlockEntity::from_nbt(&Tag::Int(0)).is_none());
    }

    #[test]
    fn p5e_parity_campfire_roundtrip_and_cook_tick() {
        let mut campfire = CampfireBlockEntity::default();
        assert_eq!(campfire.place("minecraft:beef", 30), Some(0));
        assert_eq!(campfire.place("minecraft:porkchop", 30), Some(1));
        assert_eq!(campfire.place("minecraft:cod", 30), Some(2));
        assert_eq!(campfire.place("minecraft:potato", 30), Some(3));
        assert_eq!(campfire.place("minecraft:mutton", 30), None); // all four full
        let decoded = roundtrip(&campfire, CampfireBlockEntity::to_nbt, CampfireBlockEntity::from_nbt);
        assert_eq!(decoded, campfire);
        let mut ticking = decoded;
        for _ in 0..29 {
            assert!(ticking.tick(true).is_empty());
        }
        // Tick 30 finishes every slot at once.
        let mut finished = ticking.tick(true);
        finished.sort();
        assert_eq!(
            finished,
            vec![
                (0, "minecraft:beef".to_string()),
                (1, "minecraft:porkchop".to_string()),
                (2, "minecraft:cod".to_string()),
                (3, "minecraft:potato".to_string()),
            ]
        );
        // Unlit rolls progress back two ticks but never below zero.
        let mut unlit = CampfireBlockEntity::default();
        unlit.place("minecraft:beef", 100);
        unlit.tick(true);
        unlit.tick(false);
        unlit.tick(false);
        let (_, time, _) = unlit.slots[0].clone().unwrap();
        assert_eq!(time, 0);
        assert!(CampfireBlockEntity::from_nbt(&Tag::Int(0)).is_none());
        let bad = block_entity_compound([(
            "CookingTimes",
            block_entity_list(8, vec![block_entity_string("x")]),
        )]);
        assert!(CampfireBlockEntity::from_nbt(&bad).is_none());
    }

    #[test]
    fn p5e_parity_brewing_stand_roundtrip_and_signal() {
        let stand = BrewingStandBlockEntity {
            slots: [
                Some(("minecraft:potion".into(), 1)),
                Some(("minecraft:potion".into(), 1)),
                None,
                Some(("minecraft:nether_wart".into(), 42)),
                Some(("minecraft:blaze_powder".into(), 10)),
            ],
            brew_time: 400,
            fuel: 20,
        };
        let decoded = roundtrip(&stand, BrewingStandBlockEntity::to_nbt, BrewingStandBlockEntity::from_nbt);
        assert_eq!(decoded, stand);
        // fill = 1/1 + 1/1 + 42/64 + 10/64 over 5 slots -> floor(2.8125/5*14)+1 = 8
        let expected = (((1.0f32 + 1.0 + 42.0 / 64.0 + 10.0 / 64.0) / 5.0 * 14.0).floor() as u8) + 1;
        assert_eq!(decoded.comparator_signal(), expected);
        assert_eq!(BrewingStandBlockEntity::default().comparator_signal(), 0);
        assert!(BrewingStandBlockEntity::from_nbt(&Tag::Int(0)).is_none());
        let bad = block_entity_compound([("BrewTime", block_entity_string("soon"))]);
        assert!(BrewingStandBlockEntity::from_nbt(&bad).is_none());
    }

    #[test]
    fn p5e_parity_crafter_roundtrip_disabled_slots_and_edge() {
        let mut crafter = CrafterBlockEntity::default();
        crafter.slots[0] = Some(("minecraft:stick".into(), 2));
        crafter.slots[3] = Some(("minecraft:stick".into(), 1));
        assert!(crafter.set_slot_disabled(1, true));
        assert!(!crafter.set_slot_disabled(0, true)); // occupied slots stay
        assert!(!crafter.set_slot_disabled(9, true)); // out of range
        // Rising edge fires once, then latches until power drops.
        assert!(crafter.set_powered(true));
        assert!(!crafter.set_powered(true));
        assert!(!crafter.set_powered(false));
        assert!(crafter.set_powered(true));
        let decoded = roundtrip(&crafter, CrafterBlockEntity::to_nbt, CrafterBlockEntity::from_nbt);
        assert_eq!(decoded, crafter);
        // occupied 0,3 + disabled 1 -> signal 3.
        assert_eq!(decoded.comparator_signal(), 3);
        assert!(CrafterBlockEntity::from_nbt(&Tag::Int(0)).is_none());
        let bad = block_entity_compound([("disabled_slots", block_entity_string("all"))]);
        assert!(CrafterBlockEntity::from_nbt(&bad).is_none());
    }

    #[test]
    fn p5e_parity_mob_spawner_roundtrip_defaults_and_delay() {
        let spawner = MobSpawnerBlockEntity {
            entity_id: "minecraft:zombie".into(),
            delay: 37,
            min_spawn_delay: 100,
            max_spawn_delay: 400,
            spawn_count: 3,
            max_nearby_entities: 5,
            required_player_range: 12,
            spawn_range: 6,
        };
        let decoded = roundtrip(&spawner, MobSpawnerBlockEntity::to_nbt, MobSpawnerBlockEntity::from_nbt);
        assert_eq!(decoded, spawner);
        // An empty compound decodes to vanilla defaults (fail-open fields).
        let default = MobSpawnerBlockEntity::from_nbt(&block_entity_compound([])).unwrap();
        assert_eq!(default, MobSpawnerBlockEntity::default());
        // SpawnData present but missing entity.id fails closed.
        let bad = block_entity_compound([("SpawnData", block_entity_compound([]))]);
        assert!(MobSpawnerBlockEntity::from_nbt(&bad).is_none());
        assert!(MobSpawnerBlockEntity::from_nbt(&Tag::Int(0)).is_none());
        // Delay countdown and reroll stay inside the configured interval.
        let mut ticking = spawner.clone();
        while !ticking.delay_tick() {}
        for r in [0.0_f32, 0.5, 0.999] {
            ticking.reset_delay(r);
            assert!((100..400).contains(&ticking.delay));
        }
        ticking.reset_delay(0.0);
        assert_eq!(ticking.delay, 100);
        assert!(ticking.player_in_range(12.0, 0.0, 0.0)); // range 12 -> dist 12 in
        assert!(!ticking.player_in_range(12.1, 0.0, 0.0));
    }

    #[test]
    fn p5e_parity_items_decode_is_fail_closed() {
        // `Items` present but not a list.
        let bad = block_entity_compound([("Items", Tag::Int(0))]);
        assert!(be_read_items::<5>(&bad).is_none());
        // Entry missing a string `id`.
        let bad = block_entity_compound([(
            "Items",
            block_entity_list(
                10,
                vec![block_entity_compound([("count", Tag::Int(1))])],
            ),
        )]);
        assert!(be_read_items::<5>(&bad).is_none());
        // Out-of-range Slot indexes are skipped like vanilla loadAllItems.
        let out_of_range = block_entity_compound([(
            "Items",
            block_entity_list(
                10,
                vec![block_entity_compound([
                    ("Slot", Tag::Byte(8)),
                    ("id", block_entity_string("minecraft:potion")),
                    ("count", Tag::Int(1)),
                ])],
            ),
        )]);
        assert!(be_read_items::<5>(&out_of_range)
            .unwrap()
            .iter()
            .all(Option::is_none));
    }

    #[test]
    fn p5e_parity_conduit_frame_scan_thresholds_and_ranges() {
        // The 42 candidate cells are the three cross planes; a "full" frame
        // fills every one of them.
        let is_frame = |_: i32, _: i32, _: i32| true;
        let all_water = |_: i32, _: i32, _: i32| true;
        assert_eq!(
            ConduitState::scan_frame_blocks(all_water, is_frame),
            CONDUIT_MIN_KILL_FRAME
        );
        // Off-cross cells never count: only (0,0,±2)-style axes plus the
        // face rings are scanned, so frame blocks at (±2,±1,±1) are ignored.
        let off_cross_only = |x: i32, y: i32, z: i32| {
            x.abs() == 2 && y.abs() == 1 && z.abs() == 1
        };
        assert_eq!(
            ConduitState::scan_frame_blocks(all_water, off_cross_only),
            0
        );
        // Any non-water cell in the 3x3x3 interior fails the whole scan.
        let dry = |x: i32, y: i32, z: i32| !(x == 0 && y == 0 && z == 0);
        assert_eq!(ConduitState::scan_frame_blocks(dry, is_frame), 0);

        // Thresholds: 15 inactive, 16 active at range 32, 21 -> 48, 28 -> 64,
        // 42 hunting at range 96 (`frame_blocks / 7 * 16`).
        let mut conduit = ConduitState::default();
        conduit.commit_scan(15);
        assert!(!conduit.active && !conduit.hunting);
        conduit.commit_scan(16);
        assert!(conduit.active && !conduit.hunting);
        assert_eq!(conduit.effect_range(), 32);
        conduit.commit_scan(21);
        assert_eq!(conduit.effect_range(), 48);
        conduit.commit_scan(28);
        assert_eq!(conduit.effect_range(), 64);
        conduit.commit_scan(42);
        assert!(conduit.active && conduit.hunting);
        assert_eq!(conduit.effect_range(), 96);

        // The rescan cadence is `game_time % 40 == 0`.
        assert!(ConduitState::rescan_due(0));
        assert!(ConduitState::rescan_due(40));
        assert!(!ConduitState::rescan_due(1));
        assert!(!ConduitState::rescan_due(39));

        // `conduit_power` targets: in-range wet players only, while active.
        let players = [
            (0.5, 0.5, 0.5, true),   // centre, wet -> in
            (32.0, 0.5, 0.5, true),  // exactly 32 out on x -> boundary in
            (33.0, 0.5, 0.5, true),  // beyond range -> out
            (1.0, 1.0, 1.0, false),  // dry -> out
        ];
        conduit.commit_scan(16);
        assert_eq!(conduit.effect_targets((0, 0, 0), &players), vec![0, 1]);
        conduit.commit_scan(0);
        assert!(conduit.effect_targets((0, 0, 0), &players).is_empty());
    }

    #[test]
    fn p5e_parity_conduit_hunt_keeps_drops_and_returns_target() {
        let mut conduit = ConduitState::default();
        conduit.commit_scan(CONDUIT_MIN_KILL_FRAME);
        let zombie = || ("minecraft:zombie".to_string(), true, 25.0);
        let skeleton = || ("minecraft:skeleton".to_string(), true, 4.0);

        // First pass picks a target and reports it for the 4.0 magic hit.
        assert_eq!(
            conduit.hunt_tick(&[zombie(), skeleton()]),
            Some(("minecraft:skeleton".to_string(), CONDUIT_ATTACK_DAMAGE))
        );
        assert_eq!(conduit.target.as_deref(), Some("minecraft:skeleton"));
        // A kept target keeps being hit while inside the 8.0 bubble.
        assert_eq!(
            conduit.hunt_tick(&[zombie(), skeleton()]),
            Some(("minecraft:skeleton".to_string(), CONDUIT_ATTACK_DAMAGE))
        );
        // Leaving the 8.0 `closerThan` bubble drops the target and the pass
        // ends — vanilla only re-acquires on the next hunting tick.
        let far = ("minecraft:skeleton".to_string(), true, 65.0);
        assert_eq!(conduit.hunt_tick(&[zombie(), far]), None);
        assert_eq!(conduit.target, None);
        assert_eq!(
            conduit.hunt_tick(&[zombie(), far]),
            Some(("minecraft:zombie".to_string(), CONDUIT_ATTACK_DAMAGE))
        );
        // A dead target is dropped the same way.
        let dead = ("minecraft:zombie".to_string(), false, 4.0);
        assert_eq!(conduit.hunt_tick(&[dead]), None);
        assert_eq!(conduit.target, None);
        // A fresh pick outside the 8.0 bubble is dropped undamaged.
        let edge = ("minecraft:zombie".to_string(), true, 80.0);
        assert_eq!(conduit.hunt_tick(&[edge]), None);
        assert_eq!(conduit.target, None);
        // Below 42 frame blocks hunting is off entirely.
        conduit.commit_scan(41);
        assert_eq!(conduit.hunt_tick(&[zombie()]), None);
        assert_eq!(conduit.target, None);

        let decoded = roundtrip(&conduit, ConduitState::to_nbt, ConduitState::from_nbt);
        assert_eq!(decoded, conduit);
        assert!(ConduitState::from_nbt(&Tag::Int(0)).is_none());
        let bad = block_entity_compound([("frame_blocks", block_entity_string("full"))]);
        assert!(ConduitState::from_nbt(&bad).is_none());
    }

    #[test]
    fn p5e_parity_beehive_occupants_honey_and_harvest() {
        let mut hive = BeehiveState::default();
        assert_eq!(hive.comparator_signal(), 0);
        // Three-bee cap; nectar picks the 2400-tick occupation time.
        assert!(hive.try_enter_hive("minecraft:bee", false));
        assert!(hive.try_enter_hive("minecraft:bee", true));
        assert!(hive.try_enter_hive("minecraft:bee", false));
        assert!(!hive.try_enter_hive("minecraft:bee", false));
        assert_eq!(hive.occupants.len(), BEEHIVE_MAX_OCCUPANTS);
        assert_eq!(hive.occupants[1].min_occupation_ticks, 2400);
        assert_eq!(hive.occupants[0].min_occupation_ticks, 600);

        // Bees stay inside until `ticks_in_hive` passes `min_occupation_ticks`
        // AND `can_leave` holds — both non-nectar bees leave on the same tick.
        for _ in 0..600 {
            assert!(hive.tick_occupants(false, 0.5).is_empty());
        }
        assert_eq!(hive.occupants.len(), 3);
        let mut released = hive.tick_occupants(true, 0.5);
        assert_eq!(released.len(), 2);
        released.sort_by(|a, b| a.entity_id.cmp(&b.entity_id));
        assert!(released.iter().all(|b| !b.has_nectar));
        assert_eq!(hive.occupants.len(), 1);
        assert!(hive.occupants[0].has_nectar);
        // The nectar bee needs `ticks_in_hive > 2400` to leave.
        for _ in 0..1799 {
            hive.tick_occupants(true, 0.5);
        }
        assert_eq!(hive.occupants.len(), 1);
        assert_eq!(hive.occupants[0].ticks_in_hive, 2400);
        let released = hive.tick_occupants(true, 0.5);
        assert_eq!(released.len(), 1);
        assert!(hive.occupants.is_empty());
        // Its nectar raised honey_level by 1 (roll 0.5 misses the 2x bonus).
        assert_eq!(hive.honey_level, 1);
        assert_eq!(hive.comparator_signal(), 1);

        // Nectar releases raise honey_level: +1 normally, +2 on the ~1% roll,
        // capped at MAX_HONEY_LEVEL.
        let mut hive = BeehiveState::default();
        hive.honey_level = 3;
        hive.try_enter_hive("minecraft:bee", true);
        for _ in 0..2401 {
            hive.tick_occupants(true, 0.5);
        }
        assert_eq!(hive.honey_level, 4);
        let mut hive = BeehiveState::default();
        hive.honey_level = 3;
        hive.try_enter_hive("minecraft:bee", true);
        for _ in 0..2401 {
            hive.tick_occupants(true, 0.005);
        }
        assert_eq!(hive.honey_level, 5);
        assert_eq!(hive.comparator_signal(), 5);

        // Shears harvest: honey_level 5 -> 3 honeycomb, level reset, bees out.
        let mut hive = BeehiveState::default();
        hive.honey_level = 5;
        hive.try_enter_hive("minecraft:bee", false);
        let harvest = hive.harvest_shears(false).expect("full hive");
        assert_eq!(harvest.item, "minecraft:honeycomb");
        assert_eq!(harvest.count, 3);
        assert!(harvest.angers_bees);
        assert_eq!(harvest.released.len(), 1);
        assert_eq!(hive.honey_level, 0);
        assert!(hive.occupants.is_empty());
        assert_eq!(hive.comparator_signal(), 0);
        // Campfire below suppresses the anger.
        hive.honey_level = 5;
        let harvest = hive.harvest_shears(true).expect("full hive");
        assert!(!harvest.angers_bees);
        // Bottle harvest: 1 honey_bottle, same gates.
        hive.honey_level = 5;
        let harvest = hive.harvest_bottle(false).expect("full hive");
        assert_eq!(harvest.item, "minecraft:honey_bottle");
        assert_eq!(harvest.count, 1);
        assert!(hive.harvest_bottle(false).is_none()); // level already 0
        hive.honey_level = 4;
        assert!(hive.harvest_shears(false).is_none());
        assert!(hive.harvest_bottle(false).is_none());

        // Breaking: Silk Touch preserves occupants calmly; plain break
        // releases them, angering unless a campfire smokes below.
        let mut hive = BeehiveState::default();
        hive.try_enter_hive("minecraft:bee", false);
        let (kept, released, angers) = hive.on_break(true, false);
        assert_eq!(kept.len(), 1);
        assert!(released.is_empty() && !angers);
        hive.try_enter_hive("minecraft:bee", false);
        let (kept, released, angers) = hive.on_break(false, true);
        assert!(kept.is_empty());
        assert_eq!(released.len(), 1);
        assert!(!angers); // campfire suppresses
        hive.try_enter_hive("minecraft:bee", false);
        let (_, released, angers) = hive.on_break(false, false);
        assert_eq!(released.len(), 1);
        assert!(angers);

        // NBT roundtrip keeps occupants and honey level.
        let mut hive = BeehiveState::default();
        hive.honey_level = 3;
        hive.try_enter_hive("minecraft:bee", true);
        let decoded = roundtrip(&hive, BeehiveState::to_nbt, BeehiveState::from_nbt);
        assert_eq!(decoded, hive);
        assert_eq!(decoded.comparator_signal(), 3);
        assert!(BeehiveState::from_nbt(&Tag::Int(0)).is_none());
        let bad = block_entity_compound([("bees", Tag::Int(1))]);
        assert!(BeehiveState::from_nbt(&bad).is_none());
    }
}
