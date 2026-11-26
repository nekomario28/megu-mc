// SPDX-License-Identifier: LGPL-3.0-or-later
#![forbid(unsafe_code)]
//! Typed player-inventory slot coordinates.
//!
//! Minecraft uses three different slot coordinate spaces for the same player
//! inventory: menu indices (`0..=45`), persistent player NBT indices
//! (`0..=35`, `100..=103`, `150`), and the zero-based hotbar index visible in
//! input and HUD code. These checked newtypes keep those spaces distinct and
//! convert between them only at explicit boundaries.

use std::error::Error;
use std::fmt;

/// Error returned when an integer cannot represent the requested slot type or
/// a slot cannot be represented in the target coordinate space.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SlotIndexError {
    index: u8,
}

impl SlotIndexError {
    /// The integer that failed validation or conversion.
    pub const fn index(self) -> u8 {
        self.index
    }
}

impl fmt::Display for SlotIndexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "slot index {} is outside the target coordinate space",
            self.index
        )
    }
}

impl Error for SlotIndexError {}

/// Zero-based hotbar coordinate, `0..=8`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct HotbarSlot(u8);

impl HotbarSlot {
    pub const COUNT: u8 = 9;
    pub const MIN: u8 = 0;
    pub const MAX: u8 = 8;

    /// Construct a hotbar slot, rejecting every index outside `0..=8`.
    pub const fn new(index: u8) -> Option<Self> {
        if index <= Self::MAX {
            Some(Self(index))
        } else {
            None
        }
    }

    /// The zero-based hotbar index.
    pub const fn index(self) -> u8 {
        self.0
    }

    /// This hotbar slot in the player inventory menu coordinate space
    /// (`36..=44`).
    pub const fn to_player_menu_slot(self) -> PlayerMenuSlot {
        PlayerMenuSlot(36 + self.0)
    }

    /// This hotbar slot in the persistent player NBT coordinate space
    /// (`0..=8`).
    pub const fn to_player_nbt_slot(self) -> PlayerNbtSlot {
        PlayerNbtSlot(self.0)
    }
}

impl TryFrom<u8> for HotbarSlot {
    type Error = SlotIndexError;

    fn try_from(index: u8) -> Result<Self, Self::Error> {
        Self::new(index).ok_or(SlotIndexError { index })
    }
}

impl From<HotbarSlot> for u8 {
    fn from(slot: HotbarSlot) -> Self {
        slot.index()
    }
}

/// Player inventory menu coordinate, `0..=45`.
///
/// The vanilla player menu layout is: crafting result/input `0..=4`, armor
/// `5..=8` ordered helmet/chestplate/leggings/boots, main inventory `9..=35`,
/// hotbar `36..=44`, and offhand `45`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PlayerMenuSlot(u8);

impl PlayerMenuSlot {
    pub const COUNT: u8 = 46;
    pub const MAX: u8 = 45;
    pub const CRAFT_LAST: u8 = 4;
    pub const ARMOR_FIRST: u8 = 5;
    pub const ARMOR_LAST: u8 = 8;
    pub const MAIN_FIRST: u8 = 9;
    pub const MAIN_LAST: u8 = 35;
    pub const HOTBAR_FIRST: u8 = 36;
    pub const HOTBAR_LAST: u8 = 44;
    pub const OFFHAND: u8 = 45;

    /// Construct a player menu slot, rejecting every index outside `0..=45`.
    pub const fn new(index: u8) -> Option<Self> {
        if index <= Self::MAX {
            Some(Self(index))
        } else {
            None
        }
    }

    /// The raw menu index.
    pub const fn index(self) -> u8 {
        self.0
    }

    /// Whether this is one of the crafting result/input slots (`0..=4`).
    pub const fn is_craft(self) -> bool {
        self.0 <= Self::CRAFT_LAST
    }

    /// Whether this is one of the four armor slots (`5..=8`).
    pub const fn is_armor(self) -> bool {
        self.0 >= Self::ARMOR_FIRST && self.0 <= Self::ARMOR_LAST
    }

    /// Whether this is one of the 27 main-inventory slots (`9..=35`).
    pub const fn is_main(self) -> bool {
        self.0 >= Self::MAIN_FIRST && self.0 <= Self::MAIN_LAST
    }

    /// Whether this is one of the nine hotbar slots (`36..=44`).
    pub const fn is_hotbar(self) -> bool {
        self.0 >= Self::HOTBAR_FIRST && self.0 <= Self::HOTBAR_LAST
    }

    /// Whether this is the offhand slot (`45`).
    pub const fn is_offhand(self) -> bool {
        self.0 == Self::OFFHAND
    }

    /// Zero-based armor index using persistent inventory ordering:
    /// boots `0`, leggings `1`, chestplate `2`, helmet `3`.
    pub const fn armor_index(self) -> Option<u8> {
        if self.is_armor() {
            Some(Self::ARMOR_LAST - self.0)
        } else {
            None
        }
    }

    /// Zero-based main-inventory row-major index (`0..=26`).
    pub const fn main_index(self) -> Option<u8> {
        if self.is_main() {
            Some(self.0 - Self::MAIN_FIRST)
        } else {
            None
        }
    }

    /// Hotbar coordinate for `36..=44`, or `None` outside the hotbar band.
    pub const fn hotbar(self) -> Option<HotbarSlot> {
        if self.is_hotbar() {
            Some(HotbarSlot(self.0 - Self::HOTBAR_FIRST))
        } else {
            None
        }
    }

    /// Convert to the persistent player NBT coordinate space. The five craft
    /// slots are transient menu state and intentionally have no persistent
    /// player-inventory representation.
    pub const fn to_player_nbt_slot(self) -> Option<PlayerNbtSlot> {
        if self.is_craft() {
            None
        } else if self.is_armor() {
            // Menu armor is helmet/chestplate/leggings/boots while NBT armor
            // is boots/leggings/chestplate/helmet at 100..=103.
            Some(PlayerNbtSlot(108 - self.0))
        } else if self.is_main() {
            Some(PlayerNbtSlot(self.0))
        } else if self.is_hotbar() {
            Some(PlayerNbtSlot(self.0 - Self::HOTBAR_FIRST))
        } else {
            Some(PlayerNbtSlot(150))
        }
    }

    /// Convert a persistent player inventory NBT coordinate into its menu
    /// coordinate. Every valid persistent player slot has a menu representation.
    pub const fn from_player_nbt_slot(slot: PlayerNbtSlot) -> Self {
        let index = slot.index();
        if index <= 8 {
            Self(Self::HOTBAR_FIRST + index)
        } else if index <= 35 {
            Self(index)
        } else if index == 150 {
            Self(Self::OFFHAND)
        } else {
            // Inverse of the armor inversion in `to_player_nbt_slot`.
            Self(108 - index)
        }
    }
}

impl TryFrom<u8> for PlayerMenuSlot {
    type Error = SlotIndexError;

    fn try_from(index: u8) -> Result<Self, Self::Error> {
        Self::new(index).ok_or(SlotIndexError { index })
    }
}

impl From<PlayerMenuSlot> for u8 {
    fn from(slot: PlayerMenuSlot) -> Self {
        slot.index()
    }
}

impl From<HotbarSlot> for PlayerMenuSlot {
    fn from(slot: HotbarSlot) -> Self {
        slot.to_player_menu_slot()
    }
}

impl TryFrom<PlayerMenuSlot> for HotbarSlot {
    type Error = SlotIndexError;

    fn try_from(slot: PlayerMenuSlot) -> Result<Self, Self::Error> {
        slot.hotbar().ok_or(SlotIndexError {
            index: slot.index(),
        })
    }
}

impl From<PlayerNbtSlot> for PlayerMenuSlot {
    fn from(slot: PlayerNbtSlot) -> Self {
        Self::from_player_nbt_slot(slot)
    }
}

/// Persistent player inventory NBT coordinate.
///
/// Valid values are hotbar `0..=8`, main inventory `9..=35`, armor
/// `100..=103` ordered boots/leggings/chestplate/helmet, and offhand `150`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PlayerNbtSlot(u8);

impl PlayerNbtSlot {
    pub const MAIN_FIRST: u8 = 9;
    pub const MAIN_LAST: u8 = 35;
    pub const ARMOR_FIRST: u8 = 100;
    pub const ARMOR_LAST: u8 = 103;
    pub const OFFHAND: u8 = 150;

    /// Construct a persistent player inventory slot, rejecting indices outside
    /// the four valid NBT bands.
    pub const fn new(index: u8) -> Option<Self> {
        if index <= Self::MAIN_LAST
            || (index >= Self::ARMOR_FIRST && index <= Self::ARMOR_LAST)
            || index == Self::OFFHAND
        {
            Some(Self(index))
        } else {
            None
        }
    }

    /// The raw persistent NBT index.
    pub const fn index(self) -> u8 {
        self.0
    }

    /// Whether this is one of the hotbar slots (`0..=8`).
    pub const fn is_hotbar(self) -> bool {
        self.0 <= 8
    }

    /// Whether this is one of the 27 main-inventory slots (`9..=35`).
    pub const fn is_main(self) -> bool {
        self.0 >= Self::MAIN_FIRST && self.0 <= Self::MAIN_LAST
    }

    /// Whether this is one of the four armor slots (`100..=103`).
    pub const fn is_armor(self) -> bool {
        self.0 >= Self::ARMOR_FIRST && self.0 <= Self::ARMOR_LAST
    }

    /// Whether this is the offhand slot (`150`).
    pub const fn is_offhand(self) -> bool {
        self.0 == Self::OFFHAND
    }

    /// Zero-based armor index in persistent ordering: boots `0`, leggings `1`,
    /// chestplate `2`, helmet `3`.
    pub const fn armor_index(self) -> Option<u8> {
        if self.is_armor() {
            Some(self.0 - Self::ARMOR_FIRST)
        } else {
            None
        }
    }

    /// Zero-based main-inventory row-major index (`0..=26`).
    pub const fn main_index(self) -> Option<u8> {
        if self.is_main() {
            Some(self.0 - Self::MAIN_FIRST)
        } else {
            None
        }
    }

    /// Hotbar coordinate for `0..=8`, or `None` outside the hotbar band.
    pub const fn hotbar(self) -> Option<HotbarSlot> {
        if self.is_hotbar() {
            Some(HotbarSlot(self.0))
        } else {
            None
        }
    }

    /// Convert to the player inventory menu coordinate space.
    pub const fn to_player_menu_slot(self) -> PlayerMenuSlot {
        PlayerMenuSlot::from_player_nbt_slot(self)
    }
}

impl TryFrom<u8> for PlayerNbtSlot {
    type Error = SlotIndexError;

    fn try_from(index: u8) -> Result<Self, Self::Error> {
        Self::new(index).ok_or(SlotIndexError { index })
    }
}

impl From<PlayerNbtSlot> for u8 {
    fn from(slot: PlayerNbtSlot) -> Self {
        slot.index()
    }
}

impl From<HotbarSlot> for PlayerNbtSlot {
    fn from(slot: HotbarSlot) -> Self {
        slot.to_player_nbt_slot()
    }
}

impl TryFrom<PlayerNbtSlot> for HotbarSlot {
    type Error = SlotIndexError;

    fn try_from(slot: PlayerNbtSlot) -> Result<Self, Self::Error> {
        slot.hotbar().ok_or(SlotIndexError {
            index: slot.index(),
        })
    }
}

impl TryFrom<PlayerMenuSlot> for PlayerNbtSlot {
    type Error = SlotIndexError;

    fn try_from(slot: PlayerMenuSlot) -> Result<Self, Self::Error> {
        slot.to_player_nbt_slot().ok_or(SlotIndexError {
            index: slot.index(),
        })
    }
}

/// Position of the 36 player inventory slots inside an arbitrary container or
/// workstation menu.
///
/// Vanilla standard menus append the player inventory in row-major main
/// inventory order first (`27` slots), then the hotbar (`9` slots). Different
/// containers merely choose a different `player_inv_start`; the relative
/// ordering is invariant.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MenuInventoryLayout {
    player_inv_start: usize,
}

impl MenuInventoryLayout {
    pub const MAIN_SLOT_COUNT: u8 = 27;
    pub const HOTBAR_OFFSET: usize = 27;
    pub const PLAYER_SLOT_SPAN: usize = 36;

    /// Construct a layout. The constructor rejects starts so close to
    /// `usize::MAX` that even the final hotbar index could overflow.
    pub const fn new(player_inv_start: usize) -> Option<Self> {
        if player_inv_start <= usize::MAX - (Self::PLAYER_SLOT_SPAN - 1) {
            Some(Self { player_inv_start })
        } else {
            None
        }
    }

    /// Menu index of player main-inventory row-major slot `0`.
    pub const fn player_inv_start(&self) -> usize {
        self.player_inv_start
    }

    /// Absolute menu index for one zero-based hotbar coordinate.
    pub const fn hotbar_slot(&self, hotbar: HotbarSlot) -> usize {
        self.player_inv_start + Self::HOTBAR_OFFSET + hotbar.index() as usize
    }

    /// Absolute menu index for a zero-based main-inventory row-major index
    /// (`0..=26`), or `None` when the index is outside the main inventory.
    pub const fn main_slot(&self, index: u8) -> Option<usize> {
        if index < Self::MAIN_SLOT_COUNT {
            Some(self.player_inv_start + index as usize)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_nbt_indices() -> Vec<u8> {
        (0..=35)
            .chain(100..=103)
            .chain(std::iter::once(150))
            .collect()
    }

    #[test]
    fn hotbar_slot_accepts_exactly_zero_through_eight() {
        for index in 0..=8 {
            let slot = HotbarSlot::new(index).expect("valid hotbar slot");
            assert_eq!(slot.index(), index);
            assert_eq!(u8::from(slot), index);
            assert_eq!(HotbarSlot::try_from(index), Ok(slot));
        }
        for index in 9..=u8::MAX {
            assert_eq!(HotbarSlot::new(index), None);
            assert!(HotbarSlot::try_from(index).is_err());
        }
    }

    #[test]
    fn player_menu_slot_accepts_exactly_zero_through_forty_five() {
        for index in 0..=45 {
            let slot = PlayerMenuSlot::new(index).expect("valid menu slot");
            assert_eq!(slot.index(), index);
            assert_eq!(u8::from(slot), index);
            assert_eq!(PlayerMenuSlot::try_from(index), Ok(slot));
        }
        for index in 46..=u8::MAX {
            assert_eq!(PlayerMenuSlot::new(index), None);
            assert!(PlayerMenuSlot::try_from(index).is_err());
        }
    }

    #[test]
    fn player_nbt_slot_accepts_only_the_four_persistent_bands() {
        for index in valid_nbt_indices() {
            let slot = PlayerNbtSlot::new(index).expect("valid persistent slot");
            assert_eq!(slot.index(), index);
            assert_eq!(PlayerNbtSlot::try_from(index), Ok(slot));
        }
        for index in 0..=u8::MAX {
            if !(index <= 35 || (100..=103).contains(&index) || index == 150) {
                assert_eq!(PlayerNbtSlot::new(index), None);
                assert!(PlayerNbtSlot::try_from(index).is_err());
            }
        }
    }

    #[test]
    fn menu_and_nbt_round_trip_every_persistent_player_slot() {
        let indices = valid_nbt_indices();
        assert_eq!(indices.len(), 41);
        for index in indices {
            let nbt = PlayerNbtSlot::new(index).expect("valid persistent slot");
            let menu = nbt.to_player_menu_slot();
            assert_eq!(menu.to_player_nbt_slot(), Some(nbt));
            assert_eq!(PlayerMenuSlot::from_player_nbt_slot(nbt), menu);
            assert_eq!(PlayerNbtSlot::try_from(menu), Ok(nbt));
        }
    }

    #[test]
    fn coordinate_space_mappings_match_the_vanilla_layout() {
        for raw in 0..=8 {
            let hotbar = HotbarSlot::new(raw).expect("valid hotbar slot");
            let menu = hotbar.to_player_menu_slot();
            assert_eq!(menu.index(), 36 + raw);
            assert_eq!(menu.hotbar(), Some(hotbar));
            assert_eq!(
                menu.to_player_nbt_slot().and_then(PlayerNbtSlot::hotbar),
                Some(hotbar)
            );
        }

        for raw in 9..=35 {
            let menu = PlayerMenuSlot::new(raw).expect("main menu slot");
            let nbt = menu.to_player_nbt_slot().expect("main persistent slot");
            assert_eq!(nbt.index(), raw);
            assert_eq!(menu.main_index(), nbt.main_index());
            assert_eq!(menu.main_index(), Some(raw - 9));
        }

        // NBT armor is boots/leggings/chestplate/helmet at 100..=103;
        // menu armor is helmet/chestplate/leggings/boots at 5..=8.
        for (menu_index, nbt_index, armor_index) in
            [(5, 103, 3), (6, 102, 2), (7, 101, 1), (8, 100, 0)]
        {
            let menu = PlayerMenuSlot::new(menu_index).expect("armor menu slot");
            let nbt = menu.to_player_nbt_slot().expect("armor persistent slot");
            assert_eq!(nbt.index(), nbt_index);
            assert_eq!(menu.armor_index(), Some(armor_index));
            assert_eq!(nbt.armor_index(), Some(armor_index));
            assert_eq!(nbt.to_player_menu_slot(), menu);
        }

        let offhand_menu = PlayerMenuSlot::new(45).expect("offhand menu slot");
        let offhand_nbt = PlayerNbtSlot::new(150).expect("offhand persistent slot");
        assert_eq!(offhand_menu.to_player_nbt_slot(), Some(offhand_nbt));
        assert_eq!(offhand_nbt.to_player_menu_slot(), offhand_menu);
    }

    #[test]
    fn craft_slots_have_no_persistent_player_inventory_slot() {
        for index in 0..=4 {
            let menu = PlayerMenuSlot::new(index).expect("craft menu slot");
            assert!(menu.is_craft());
            assert_eq!(menu.to_player_nbt_slot(), None);
            assert!(PlayerNbtSlot::try_from(menu).is_err());
        }
    }

    #[test]
    fn container_layout_places_main_inventory_then_hotbar() {
        let layout = MenuInventoryLayout::new(3).expect("valid layout");
        assert_eq!(layout.player_inv_start(), 3);
        assert_eq!(layout.main_slot(0), Some(3));
        assert_eq!(layout.main_slot(26), Some(29));
        assert_eq!(layout.main_slot(27), None);
        assert_eq!(layout.main_slot(u8::MAX), None);
        assert_eq!(layout.hotbar_slot(HotbarSlot::new(0).expect("hotbar")), 30);
        assert_eq!(layout.hotbar_slot(HotbarSlot::new(8).expect("hotbar")), 38);
    }

    #[test]
    fn container_layout_constructor_rejects_overflowing_starts() {
        assert_eq!(
            MenuInventoryLayout::new(usize::MAX - 35).map(|x| x.player_inv_start()),
            Some(usize::MAX - 35)
        );
        assert_eq!(MenuInventoryLayout::new(usize::MAX - 34), None);
        assert_eq!(MenuInventoryLayout::new(usize::MAX), None);
    }
}
