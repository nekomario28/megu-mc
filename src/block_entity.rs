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
#[cfg_attr(not(test), allow(dead_code))]
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
#[cfg_attr(not(test), allow(dead_code))]
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
#[cfg_attr(not(test), allow(dead_code))]
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
#[cfg_attr(not(test), allow(dead_code))]
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
#[cfg_attr(not(test), allow(dead_code))]
fn be_num(tag: &Tag, key: &str, default: i64) -> Option<i64> {
    match tag.get(key) {
        None => Some(default),
        Some(value) => nbt_int_val(value),
    }
}

/// A string field that must be an NBT string that decodes when present.
#[cfg_attr(not(test), allow(dead_code))]
fn be_str(tag: &Tag, key: &str) -> Option<Option<String>> {
    match tag.get(key) {
        None => Some(None),
        Some(Tag::String(value)) => Some(Some(value.to_utf8().ok()?)),
        Some(_) => None,
    }
}

/// `AbstractContainerMenu.getRedstoneSignalFromContainer`: each occupied slot
/// contributes `count / min(container limit, item max stack)`, the total is
/// scaled to 0..=14, and any fill bumps the answer to at least 1.
#[cfg_attr(not(test), allow(dead_code))]
fn be_container_signal(slots: &[Option<BeSlotItem>], item_max_stack: impl Fn(&str) -> u8) -> u8 {
    let mut fill = 0.0f32;
    for (id, count) in slots.iter().flatten() {
        let limit = item_max_stack(id).max(1);
        fill += f32::from(*count) / f32::from(limit.min(64));
    }
    let signal = (fill / slots.len().max(1) as f32 * 14.0).floor() as u8;
    signal + u8::from(fill > 0.0)
}

// ---------- Beacon (`minecraft:beacon`) ----------

/// `BeaconBlockEntity.updateLevels` (1.21.1): the number of complete pyramid
/// layers below the beacon, 0..=4. `is_base_block(dx, dy, dz)` answers whether
/// the block at that offset counts toward the pyramid (iron, gold, diamond,
/// emerald or netherite block in vanilla).
#[cfg_attr(not(test), allow(dead_code))]
fn beacon_pyramid_levels(mut is_base_block: impl FnMut(i32, i32, i32) -> bool) -> u8 {
    let mut levels = 0;
    for depth in 1..=4_i32 {
        let complete =
            (-depth..=depth).all(|x| (-depth..=depth).all(|z| is_base_block(x, -depth, z)));
        if !complete {
            break;
        }
        levels = depth as u8;
    }
    levels
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct BeaconBlockEntity {
    /// The saved `Levels` snapshot, always in 0..=4.
    pub(crate) levels: u8,
    /// `primary_effect` mob effect id, e.g. "minecraft:haste".
    pub(crate) primary_effect: Option<String>,
    /// `secondary_effect` mob effect id; only meaningful at level 4.
    pub(crate) secondary_effect: Option<String>,
}

#[cfg_attr(not(test), allow(dead_code))]
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

#[cfg_attr(not(test), allow(dead_code))]
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

#[cfg_attr(not(test), allow(dead_code))]
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
#[cfg_attr(not(test), allow(dead_code))]
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

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct JukeboxBlockEntity {
    /// Item id of the loaded record, e.g. "minecraft:music_disc_cat".
    pub(crate) record_item: Option<String>,
    /// `ticks_since_song_started` - ticks since the song began; 0 when the
    /// jukebox holds no record.
    pub(crate) ticks_since_song_started: u64,
}

#[cfg_attr(not(test), allow(dead_code))]
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
    /// record is loaded, zero otherwise.
    pub(crate) fn comparator_signal(&self) -> u8 {
        self.record_item
            .as_deref()
            .and_then(jukebox_comparator_output)
            .unwrap_or(0)
    }
}

// ---------- Chiseled bookshelf (`minecraft:chiseled_bookshelf`) ----------

/// `last_interacted_slot` value when no slot has been touched yet; vanilla
/// persists this as `-1`, which a `u8` cannot hold, so `u8::MAX` is the model's
/// sentinel.
#[cfg_attr(not(test), allow(dead_code))]
const CHISELED_BOOKSHELF_NO_INTERACTION: u8 = u8::MAX;

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ChiseledBookshelfBlockEntity {
    /// Book item ids in slots 0..=5.
    pub(crate) slots: [Option<String>; 6],
    /// Slot the player last inserted into or took from.
    pub(crate) last_interacted_slot: u8,
}

#[cfg_attr(not(test), allow(dead_code))]
impl Default for ChiseledBookshelfBlockEntity {
    fn default() -> Self {
        Self {
            slots: Default::default(),
            last_interacted_slot: CHISELED_BOOKSHELF_NO_INTERACTION,
        }
    }
}

#[cfg_attr(not(test), allow(dead_code))]
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
        let last = match be_num(tag, "last_interacted_slot", -1)? {
            n if (0..6).contains(&n) => n as u8,
            _ => CHISELED_BOOKSHELF_NO_INTERACTION,
        };
        Some(Self {
            slots,
            last_interacted_slot: last,
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
            Tag::Int(if self.last_interacted_slot < 6 {
                i32::from(self.last_interacted_slot)
            } else {
                -1
            }),
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
        self.last_interacted_slot = slot as u8;
        true
    }

    pub(crate) fn remove_book(&mut self, slot: usize) -> Option<String> {
        let book = self.slots.get_mut(slot)?.take()?;
        self.last_interacted_slot = slot as u8;
        Some(book)
    }

    /// `ChiseledBookShelfBlock`'s analog output: last interacted slot + 1, so
    /// an untouched shelf reads 0 and slot 5 reads 6.
    pub(crate) fn comparator_signal(&self) -> u8 {
        if self.last_interacted_slot < 6 {
            self.last_interacted_slot + 1
        } else {
            0
        }
    }
}

// ---------- Decorated pot (`minecraft:decorated_pot`) ----------

#[cfg_attr(not(test), allow(dead_code))]
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

#[cfg_attr(not(test), allow(dead_code))]
impl Default for DecoratedPotBlockEntity {
    fn default() -> Self {
        Self {
            sherds: std::array::from_fn(|_| "minecraft:brick".to_string()),
            item_id: None,
            item_count: 0,
        }
    }
}

#[cfg_attr(not(test), allow(dead_code))]
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

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct CampfireBlockEntity {
    /// Up to four cookable items with their progress and recipe times.
    pub(crate) slots: [Option<BeCampfireSlot>; 4],
}

#[cfg_attr(not(test), allow(dead_code))]
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

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct BrewingStandBlockEntity {
    /// `(item id, count)` per slot: 0..=2 bottles, 3 ingredient, 4 fuel.
    pub(crate) slots: [Option<BeSlotItem>; 5],
    /// `BrewTime`: ticks left in the current brew (vanilla starts at 400).
    pub(crate) brew_time: u16,
    /// `Fuel`: brew cycles the loaded blaze powder still pays for.
    pub(crate) fuel: u8,
}

#[cfg_attr(not(test), allow(dead_code))]
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
    /// five slots. Potion stacks are unstackable in 1.21.1 and glass bottles
    /// stack to 16; everything else reads as a 64 stack.
    pub(crate) fn comparator_signal(&self) -> u8 {
        be_container_signal(&self.slots, |item| {
            match item.strip_prefix("minecraft:").unwrap_or(item) {
                "potion" | "splash_potion" | "lingering_potion" => 1,
                "glass_bottle" | "honey_bottle" => 16,
                _ => 64,
            }
        })
    }
}

// ---------- Crafter (`minecraft:crafter`) ----------

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct CrafterBlockEntity {
    /// `(item id, count)` per crafting-grid slot 0..=8.
    pub(crate) slots: [Option<BeSlotItem>; 9],
    /// `disabled_slots` as a bitmask: bit N marks slot N toggled off.
    pub(crate) disabled_slots_mask: u16,
    /// `triggered`: high while the crafter holds a redstone charge.
    pub(crate) triggered: bool,
}

#[cfg_attr(not(test), allow(dead_code))]
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

#[cfg_attr(not(test), allow(dead_code))]
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

#[cfg_attr(not(test), allow(dead_code))]
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

#[cfg_attr(not(test), allow(dead_code))]
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
        assert_eq!(beacon_pyramid_levels(solid(1)), 1);
        assert_eq!(beacon_pyramid_levels(solid(4)), 4);
        assert_eq!(beacon_pyramid_levels(|_, _, _| false), 0);
        // A layer-2 hole caps the level at 1 even with deeper layers solid.
        assert_eq!(
            beacon_pyramid_levels(|x, dy, z| !(dy == -2 && x == 0 && z == 0)),
            1
        );
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
        assert!(shelf.insert_book(0, "minecraft:book"));
        assert!(shelf.insert_book(4, "written_book"));
        assert!(!shelf.insert_book(4, "book")); // occupied
        assert!(!shelf.insert_book(6, "book")); // out of range
        assert_eq!(shelf.comparator_signal(), 5); // slot 4 + 1
        assert_eq!(shelf.remove_book(4), Some("written_book".into()));
        assert_eq!(shelf.comparator_signal(), 5);
        let decoded = roundtrip(
            &shelf,
            ChiseledBookshelfBlockEntity::to_nbt,
            ChiseledBookshelfBlockEntity::from_nbt,
        );
        assert_eq!(decoded, shelf);
        // Vanilla persists "no interaction" as -1.
        let nbt = ChiseledBookshelfBlockEntity::default().to_nbt();
        assert_eq!(nbt.get("last_interacted_slot").and_then(Tag::int), Some(-1));
        let decoded_default = roundtrip(
            &ChiseledBookshelfBlockEntity::default(),
            ChiseledBookshelfBlockEntity::to_nbt,
            ChiseledBookshelfBlockEntity::from_nbt,
        );
        assert_eq!(decoded_default.comparator_signal(), 0);
        assert_eq!(
            decoded_default.last_interacted_slot,
            CHISELED_BOOKSHELF_NO_INTERACTION
        );
        // An out-of-range saved slot index also reads as "none".
        let bad_slot = block_entity_compound([("last_interacted_slot", Tag::Int(9))]);
        assert_eq!(
            ChiseledBookshelfBlockEntity::from_nbt(&bad_slot)
                .unwrap()
                .comparator_signal(),
            0
        );
        assert!(ChiseledBookshelfBlockEntity::from_nbt(&Tag::Int(0)).is_none());
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
}
