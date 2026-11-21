// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! Create-typed recipe view layered over [`mod_adapter_core::data_loader`].

use std::collections::BTreeMap;
use std::path::Path;

use mod_adapter_core::data_loader::{
    load_from_directory as load_mod_directory, load_from_stored_zip as load_mod_zip,
    parse_mod_recipe_json, LoadedModData,
};
pub use mod_adapter_core::data_loader::{DataLoadError, ItemFluidIdTable};
use mod_offload::recipe_processor::Recipe;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum CreateRecipeKind {
    Pressing,
    Mixing,
    Milling,
    Crushing,
    Cutting,
    Splashing,
    Haunting,
    Filling,
    Emptying,
}

impl CreateRecipeKind {
    pub fn from_type_str(value: &str) -> Option<Self> {
        match value {
            "create:pressing" => Some(Self::Pressing),
            "create:mixing" => Some(Self::Mixing),
            "create:milling" => Some(Self::Milling),
            "create:crushing" => Some(Self::Crushing),
            "create:cutting" => Some(Self::Cutting),
            "create:splashing" => Some(Self::Splashing),
            "create:haunting" => Some(Self::Haunting),
            "create:filling" => Some(Self::Filling),
            "create:emptying" => Some(Self::Emptying),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedCreateRecipe {
    pub name: String,
    pub kind: CreateRecipeKind,
    pub recipe: Recipe,
}

#[derive(Clone, Debug, Default)]
pub struct LoadedCreateData {
    pub recipes: Vec<ParsedCreateRecipe>,
    pub tags: BTreeMap<String, Vec<String>>,
    /// Declared blockstate property tables per block id (see
    /// [`mod_adapter_core::data_loader::LoadedModData::blockstates`]).
    pub blockstates: BTreeMap<String, Vec<mod_adapter_core::blockstates::PropertyDef>>,
    pub skipped_compressed_zip_entries: usize,
}

pub fn load_from_directory(
    root: &Path,
    ids: &mut ItemFluidIdTable,
) -> Result<LoadedCreateData, DataLoadError> {
    Ok(filter_create_data(load_mod_directory(root, ids)?))
}

pub fn load_from_stored_zip(
    bytes: &[u8],
    ids: &mut ItemFluidIdTable,
) -> Result<LoadedCreateData, DataLoadError> {
    Ok(filter_create_data(load_mod_zip(bytes, ids)?))
}

pub fn parse_create_recipe_json(
    recipe_name: &str,
    recipe_id: u32,
    json_bytes: &[u8],
    ids: &mut ItemFluidIdTable,
) -> Result<Option<ParsedCreateRecipe>, DataLoadError> {
    let Some(parsed) = parse_mod_recipe_json(recipe_name, recipe_id, json_bytes, ids)? else {
        return Ok(None);
    };
    let Some(kind) = CreateRecipeKind::from_type_str(&parsed.recipe_type) else {
        return Ok(None);
    };
    Ok(Some(ParsedCreateRecipe {
        name: parsed.name,
        kind,
        recipe: parsed.recipe,
    }))
}

fn filter_create_data(raw: LoadedModData) -> LoadedCreateData {
    let recipes = raw
        .recipes
        .into_iter()
        .filter_map(|r| {
            let kind = CreateRecipeKind::from_type_str(&r.recipe_type)?;
            Some(ParsedCreateRecipe {
                name: r.name,
                kind,
                recipe: r.recipe,
            })
        })
        .collect();
    LoadedCreateData {
        recipes,
        tags: raw.tags,
        blockstates: raw.blockstates,
        skipped_compressed_zip_entries: raw.skipped_compressed_zip_entries,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_stored_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        for (name, data) in entries {
            out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
            out.extend_from_slice(&20u16.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&0u32.to_le_bytes());
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            out.extend_from_slice(&(name.len() as u16).to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(data);
        }
        out
    }

    #[test]
    fn loads_pressing_and_filling_recipes_from_stored_jar() {
        let pressing = br#"{
          "type": "create:pressing",
          "ingredients": [{"item": "minecraft:iron_ingot"}],
          "results": [{"id": "create:iron_sheet", "count": 1}],
          "processing_time": 40
        }"#;
        let filling = br#"{
          "type": "create:filling",
          "ingredients": [
            {"item": "minecraft:bucket"},
            {"fluid": "minecraft:water", "amount": 1000}
          ],
          "results": [{"id": "minecraft:water_bucket", "count": 1}],
          "processing_time": 20
        }"#;
        let jar = build_stored_zip(&[
            ("data/create/recipe/pressing/iron_sheet.json", pressing),
            ("data/create/recipe/filling/water_bucket.json", filling),
        ]);
        let mut ids = ItemFluidIdTable::new();
        let loaded = load_from_stored_zip(&jar, &mut ids).unwrap();
        assert_eq!(loaded.recipes.len(), 2);
        assert_eq!(loaded.recipes[0].kind, CreateRecipeKind::Filling);
        assert_eq!(loaded.recipes[1].kind, CreateRecipeKind::Pressing);
    }
}
