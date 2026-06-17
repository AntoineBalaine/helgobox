use crate::provider_database::{
    Database, InnerFilterItem, InnerFilterItemCollections, ProviderContext, SortablePresetId,
};
use crate::{
    FilterInput, InnerBuildInput, InnerPresetId, InternalPotPresetKind, PersistentDatabaseId,
    PersistentInnerPresetId, PersistentPresetId, PipeEscaped, PluginKind, PotPreset,
    PotPresetCommon, PotPresetKind, SearchInput,
};
use std::borrow::Cow;

use crate::plugins::{Plugin, PluginCore, SuperPluginKind};
use base::hash_util::{PersistentHash, PersistentHasher};
use camino::Utf8PathBuf;
use either::Either;
use enumset::{enum_set, EnumSet};
use helgobox_api::persistence::PotFilterKind;
use ini::Ini;
use itertools::Itertools;
use std::error::Error;
use std::hash::Hasher;
use std::iter;
use std::str::FromStr;
use walkdir::WalkDir;

pub struct IniDatabase {
    persistent_id: PersistentDatabaseId,
    root_dir: Utf8PathBuf,
    entries: Vec<PresetEntry>,
}

impl IniDatabase {
    pub fn open(
        persistent_id: PersistentDatabaseId,
        root_dir: Utf8PathBuf,
    ) -> Result<Self, Box<dyn Error>> {
        if !root_dir.try_exists()? {
            return Err("path to presets root directory doesn't exist".into());
        }
        let db = Self {
            persistent_id,
            entries: Default::default(),
            root_dir,
        };
        Ok(db)
    }

    fn query_presets_internal<'a>(
        &'a self,
        filter_input: &'a FilterInput,
    ) -> impl Iterator<Item = (usize, &'a PresetEntry)> + 'a {
        let matches = !filter_input.filters.wants_factory_presets_only();
        if !matches {
            return Either::Left(iter::empty());
        }
        let iter = self.entries.iter().enumerate().filter(|(i, e)| {
            let id = InnerPresetId(*i as _);
            filter_input.everything_matches(e.plugin.as_ref(), id)
        });
        Either::Right(iter)
    }
}

struct PresetEntry {
    preset_name: String,
    plugin_kind: PluginKind,
    /// Example: "Massive"
    plugin_identifier: String,
    /// If `None`, it means the corresponding plug-in is not installed/scanned.
    plugin: Option<PluginCore>,
    content_hash: Option<PersistentHash>,
}

/// Stable on-disk mirror of [`PresetEntry`] for the scan cache. One `.ini` file caches a
/// `Vec<CachedIniEntry>` (many presets per file). `plugin_kind` is its string form and the
/// hash is hex. See `POT_DB_DESIGN.md`.
#[derive(serde::Serialize, serde::Deserialize)]
struct CachedIniEntry {
    preset_name: String,
    plugin_kind: String,
    plugin_identifier: String,
    plugin: Option<crate::db::CachedPluginCore>,
    content_hash: Option<String>,
}

impl CachedIniEntry {
    fn from_entry(e: &PresetEntry) -> Self {
        Self {
            preset_name: e.preset_name.clone(),
            plugin_kind: e.plugin_kind.as_ref().to_string(),
            plugin_identifier: e.plugin_identifier.clone(),
            plugin: e.plugin.as_ref().map(crate::db::cached_core_from),
            content_hash: crate::db::opt_hash_to_hex(e.content_hash),
        }
    }

    fn into_entry(self) -> Option<PresetEntry> {
        Some(PresetEntry {
            preset_name: self.preset_name,
            plugin_kind: PluginKind::from_str(&self.plugin_kind).ok()?,
            plugin_identifier: self.plugin_identifier,
            plugin: match &self.plugin {
                Some(c) => Some(crate::db::cached_core_to(c)?),
                None => None,
            },
            content_hash: crate::db::opt_hash_from_hex(&self.content_hash)?,
        })
    }
}

impl Database for IniDatabase {
    fn persistent_id(&self) -> &PersistentDatabaseId {
        &self.persistent_id
    }

    fn name(&self) -> Cow<str> {
        "FX presets".into()
    }

    fn description(&self) -> Cow<str> {
        "All FX presets that you saved via \"Save preset...\" in REAPER's FX window.\n\".vstpreset\"-style presets are not yet supported!"
            .into()
    }

    fn supported_advanced_filter_kinds(&self) -> EnumSet<PotFilterKind> {
        enum_set!(PotFilterKind::Bank)
    }

    fn refresh(&mut self, ctx: &ProviderContext) -> Result<(), Box<dyn Error>> {
        let file_name_regex = base::regex!(r#"(?i)(.*?)-(.*).ini"#);
        // Scan cache: one `.ini` holds many presets, so each file caches a `Vec` blob. On an
        // unchanged (mtime, size) we rebuild all of that file's entries without reading it.
        // Best-effort — any miss falls back to parsing. See `POT_DB_DESIGN.md`.
        let pass = crate::db::pass_ts();
        let provider_key = self.root_dir.as_str();
        self.entries = WalkDir::new(&self.root_dir)
            .max_depth(1)
            .follow_links(true)
            .into_iter()
            .filter_map(|entry| {
                let entry = entry.ok()?;
                if !entry.file_type().is_file() {
                    return None;
                }
                let file_name = entry.file_name().to_str()?;
                // Example file names:
                // - vst-Zebra2.ini
                // - vst3-FM8-1168312232-builtin.ini
                // - vst-TDR Nova-builtin.ini
                // - vst-reacomp.ini
                // - vst3-Massive.ini
                // - clap-org_surge-synth-team_surge-xt.ini
                // - js-analysis_hund.ini
                let captures = file_name_regex.captures(file_name)?;
                let plugin_kind_str = captures.get(1)?.as_str();
                let plugin_kind = PluginKind::from_str(plugin_kind_str).ok()?;
                let plugin_identifier = captures.get(2)?.as_str();
                if plugin_identifier.ends_with("-builtin") {
                    return None;
                }
                let (main_plugin_identifier, shell_qualifier) =
                    match plugin_identifier.rsplit_once('-') {
                        // Example: vst3-Zebra2-959560201.ini
                        // (interpret the number behind the dash as shell qualifier)
                        Some((left, right))
                            if right.len() >= 5 && right.chars().all(|ch| ch.is_ascii_digit()) =>
                        {
                            (left, Some(right))
                        }
                        // Examples: "vst-Tritik-Irid.ini", "vst-Zebra2.ini"
                        _ => (plugin_identifier, None),
                    };
                let plugin = ctx.plugin_db.plugins().find(|p| {
                    if p.common.core.id.kind() != plugin_kind {
                        return false;
                    }
                    match &p.kind {
                        SuperPluginKind::Vst(k) => {
                            let unsafe_char_regex = base::regex!(r#"[^a-zA-Z0-9_]"#);
                            let safe_main_plugin_identifier =
                                unsafe_char_regex.replace_all(main_plugin_identifier, "_");
                            let file_name_prefix = format!("{safe_main_plugin_identifier}.");
                            tracing::trace!(
                                "Test VST '{}' should start with INI plug-in file name prefix '{file_name_prefix}'",
                                k.safe_file_name
                            );
                            if !k.safe_file_name.starts_with(&file_name_prefix) {
                                return false;
                            }
                            let plugin_shell_qualifier = k.shell_qualifier.as_deref();
                            if shell_qualifier != plugin_shell_qualifier {
                                return false;
                            }
                            true
                        }
                        SuperPluginKind::Clap(k) => {
                            let safe_plugin_id = k.id.replace('.', "_");
                            if plugin_identifier != safe_plugin_id {
                                return false;
                            }
                            true
                        }
                        SuperPluginKind::Js(k) => {
                            let lowercase_safe_path = k.path.replace(['/', '\\', '.'], "_").to_lowercase();
                            let lowercase_plugin_identifier = plugin_identifier.to_lowercase();
                            tracing::trace!(
                                "Test JS '{lowercase_safe_path}' vs. INI plug-in identifier '{lowercase_plugin_identifier}'"
                            );
                            if lowercase_plugin_identifier != lowercase_safe_path {
                                return false;
                            }
                            true
                        }
                    }
                });
                let path = entry.path();
                let abs_path = path.to_str();
                let stat = abs_path.and_then(|_| crate::db::file_stat(path));
                // Try the cache first: rebuild every preset in this file from the blob.
                if let (Some(abs), Some((mtime, size))) = (abs_path, stat) {
                    if let Some(blob) = crate::db::cache_lookup(abs, mtime, size, pass) {
                        if let Some(entries) = serde_json::from_str::<Vec<CachedIniEntry>>(&blob)
                            .ok()
                            .and_then(|v| {
                                v.into_iter()
                                    .map(CachedIniEntry::into_entry)
                                    .collect::<Option<Vec<_>>>()
                            })
                        {
                            return Some(entries);
                        }
                    }
                }
                // Cache miss: parse the `.ini` and hash each preset.
                let ini_file = Ini::load_from_file(path).ok()?;
                let general_section = ini_file.section(Some("General"))?;
                let nb_presets = general_section.get("NbPresets")?;
                let preset_count: u32 = nb_presets.parse().ok()?;
                let plugin_identifier = plugin_identifier.to_string();
                let plugin_core = plugin.map(|p| p.common.core);
                let mut file_entries: Vec<PresetEntry> = Vec::with_capacity(preset_count as usize);
                for i in 0..preset_count {
                    // Skip individual malformed presets (missing section/Name/Data) but keep the
                    // rest of the file — preserves the original per-preset filter_map semantics.
                    let built = (|| {
                        let section = ini_file.section(Some(format!("Preset{i}")))?;
                        let name = section.get("Name")?;
                        // Calculate hash. At first add info about the plug-in. Without that info,
                        // the content could be ambiguous.
                        let mut hasher = PersistentHasher::new();
                        let plugin_kind_str = plugin_kind.as_ref();
                        let plugin_info = format!("{plugin_kind_str}-{plugin_identifier}");
                        hasher.write(plugin_info.as_bytes());
                        // Calculate hash out of data properties ("Data", "Data_1", "Data_2", ...)
                        let data = section.get("Data")?;
                        hasher.write(data.as_bytes());
                        let mut j = 1;
                        while let Some(more_data) = section.get(format!("Data{j}")) {
                            hasher.write(more_data.as_bytes());
                            j += 1;
                        }
                        Some(PresetEntry {
                            preset_name: name.to_string(),
                            plugin_kind,
                            plugin_identifier: plugin_identifier.clone(),
                            plugin: plugin_core,
                            content_hash: Some(hasher.digest_128()),
                        })
                    })();
                    if let Some(e) = built {
                        file_entries.push(e);
                    }
                }
                // Populate the cache for next time.
                if let (Some(abs), Some((mtime, size))) = (abs_path, stat) {
                    let cached: Vec<CachedIniEntry> =
                        file_entries.iter().map(CachedIniEntry::from_entry).collect();
                    if let Ok(blob) = serde_json::to_string(&cached) {
                        crate::db::cache_store(abs, mtime, size, provider_key, &blob, pass);
                    }
                }
                Some(file_entries)
            })
            .flatten()
            .collect();
        // Drop cache rows for `.ini` files that vanished.
        crate::db::cache_sweep(provider_key, pass);
        Ok(())
    }

    fn query_filter_collections(
        &self,
        _: &ProviderContext,
        input: InnerBuildInput,
        _: EnumSet<PotFilterKind>,
    ) -> Result<InnerFilterItemCollections, Box<dyn Error>> {
        let mut new_filters = *input.filter_input.filters;
        new_filters.clear_this_and_dependent_filters(PotFilterKind::Bank);
        let product_items = self
            .query_presets_internal(&input.filter_input.with_filters(&new_filters))
            .filter_map(|(_, entry)| Some(entry.plugin.as_ref()?.product_id))
            .unique()
            .map(InnerFilterItem::Product)
            .collect();
        let mut collections = InnerFilterItemCollections::empty();
        collections.set(PotFilterKind::Bank, product_items);
        Ok(collections)
    }

    fn query_presets(
        &self,
        ctx: &ProviderContext,
        input: InnerBuildInput,
    ) -> Result<Vec<SortablePresetId>, Box<dyn Error>> {
        let preset_ids = self
            .query_presets_internal(&input.filter_input)
            .filter(|(_, preset_entry)| {
                let search_input = IniSearchInput { ctx, preset_entry };
                input.search_evaluator.matches(search_input)
            })
            .map(|(i, entry)| SortablePresetId::new(i as _, entry.preset_name.clone()))
            .collect();
        Ok(preset_ids)
    }

    fn find_preset_by_id(
        &self,
        ctx: &ProviderContext,
        preset_id: InnerPresetId,
    ) -> Option<PotPreset> {
        let preset_entry = self.entries.get(preset_id.0 as usize)?;
        let plugin = preset_entry
            .plugin
            .as_ref()
            .and_then(|entry| ctx.plugin_db.find_plugin_by_id(&entry.id));
        let preset = PotPreset {
            common: PotPresetCommon {
                persistent_id: PersistentPresetId::new(
                    self.persistent_id().clone(),
                    create_persistent_inner_id(preset_entry),
                ),
                name: preset_entry.preset_name.clone(),
                context_name: None,
                plugin_ids: preset_entry
                    .plugin
                    .as_ref()
                    .map(|p| p.id)
                    .into_iter()
                    .collect(),
                product_ids: preset_entry
                    .plugin
                    .as_ref()
                    .map(|p| p.product_id)
                    .into_iter()
                    .collect(),
                product_name: Some(build_product_name(preset_entry, plugin).to_string()),
                content_hash: preset_entry.content_hash,
                db_specific_preview_file: None,
                is_supported: true,
                is_available: preset_entry.plugin.is_some(),
                metadata: Default::default(),
            },
            kind: PotPresetKind::Internal(InternalPotPresetKind {
                plugin_id: preset_entry.plugin.as_ref().map(|p| p.id),
            }),
        };
        Some(preset)
    }
}

/// Example: `vst3-Surge XT.ini|My Preset`
fn create_persistent_inner_id(preset_entry: &PresetEntry) -> PersistentInnerPresetId {
    let plugin_kind = preset_entry.plugin_kind.as_ref();
    let plugin_identifier = &preset_entry.plugin_identifier;
    let escaped_preset_name = PipeEscaped(&preset_entry.preset_name);
    let id = format!("{plugin_kind}-{plugin_identifier}.ini|{escaped_preset_name}");
    PersistentInnerPresetId::new(id)
}

struct IniSearchInput<'a> {
    ctx: &'a ProviderContext<'a>,
    preset_entry: &'a PresetEntry,
}

impl SearchInput for IniSearchInput<'_> {
    fn preset_name(&self) -> &str {
        &self.preset_entry.preset_name
    }

    fn product_name(&self) -> Option<Cow<str>> {
        let plugin = self
            .preset_entry
            .plugin
            .as_ref()
            .and_then(|entry| self.ctx.plugin_db.find_plugin_by_id(&entry.id));
        Some(build_product_name(self.preset_entry, plugin))
    }

    fn file_extension(&self) -> Option<&str> {
        None
    }
}

fn build_product_name<'a>(preset_entry: &'a PresetEntry, plugin: Option<&Plugin>) -> Cow<'a, str> {
    match plugin {
        None => preset_entry.plugin_identifier.as_str().into(),
        Some(p) => p.common.to_string().into(),
    }
}
