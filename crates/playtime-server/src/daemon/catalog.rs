//! The reviewed recipe catalog (#74): one directory of `*.toml` recipes, read
//! with their seed directories when the daemon starts.
use playtime_runner::docker::{BOUNDS, Recipe};
use std::{
    collections::BTreeMap,
    io::Read,
    path::{Component, Path},
};

const RECIPES: usize = 32;
const RECIPE_FILE_BYTES: usize = 256 * 1024;
const SEED_DEPTH: usize = 8;

/// Loads and checks every recipe in `directory`. A recipe's `seeds` names a
/// directory relative to the catalog's parent, the repository root, and its
/// files are frozen into the recipe. One invalid recipe fails the catalog.
pub fn load(directory: &Path) -> Result<Vec<Recipe>, String> {
    let root = directory
        .parent()
        .ok_or("the recipe directory needs a parent")?;
    let mut paths = std::fs::read_dir(directory)
        .and_then(|entries| entries.map(|entry| entry.map(|e| e.path())).collect())
        .map(|paths: Vec<_>| -> Vec<_> {
            paths
                .into_iter()
                .filter(|path| path.extension().is_some_and(|e| e == "toml"))
                .collect()
        })
        .map_err(|e| format!("{}: {e}", directory.display()))?;
    paths.sort();
    if paths.is_empty() || paths.len() > RECIPES {
        return Err(format!(
            "{}: expected 1 to {RECIPES} recipe files",
            directory.display()
        ));
    }
    paths
        .iter()
        .map(|path| recipe(root, path).map_err(|e| format!("{}: {e}", path.display())))
        .collect()
}

fn recipe(root: &Path, path: &Path) -> Result<Recipe, String> {
    let mut table: toml::Table =
        toml::from_str(&read(path, RECIPE_FILE_BYTES)?).map_err(|e| e.to_string())?;
    let seeds = match table.remove("seeds") {
        None => BTreeMap::new(),
        Some(toml::Value::String(relative)) => seeds(root, &relative)?,
        Some(_) => return Err("seeds must name a directory".into()),
    };
    let mut recipe: Recipe = table.try_into().map_err(|e| e.to_string())?;
    if path.file_stem().and_then(|stem| stem.to_str()) != Some(recipe.id.as_str()) {
        return Err("the recipe id must match its file name".into());
    }
    recipe.seeds = seeds;
    recipe.check().map_err(|e| e.to_string())?;
    Ok(recipe)
}

/// Regular UTF-8 files only, by path relative to the seed directory. Empty
/// directories aren't recorded, and file modes aren't kept.
fn seeds(root: &Path, relative: &str) -> Result<BTreeMap<String, String>, String> {
    let plain = Path::new(relative);
    if relative.is_empty()
        || plain
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err("seeds must be a plain path relative to the repository".into());
    }
    let mut files = BTreeMap::new();
    walk(&root.join(plain), "", 0, &mut files)?;
    Ok(files)
}

fn walk(
    directory: &Path,
    prefix: &str,
    depth: usize,
    files: &mut BTreeMap<String, String>,
) -> Result<(), String> {
    let failed = |e: std::io::Error| format!("{}: {e}", directory.display());
    if !std::fs::symlink_metadata(directory)
        .map_err(failed)?
        .is_dir()
        || depth > SEED_DEPTH
    {
        return Err(format!(
            "{}: seeds must be real directories at most {SEED_DEPTH} deep",
            directory.display()
        ));
    }
    let mut entries = std::fs::read_dir(directory)
        .and_then(|entries| entries.collect::<Result<Vec<_>, _>>())
        .map_err(failed)?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| "seed names must be UTF-8")?;
        let relative = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        };
        let kind = entry.file_type().map_err(failed)?;
        if kind.is_dir() {
            walk(&entry.path(), &relative, depth + 1, files)?;
        } else if kind.is_file() && files.len() < BOUNDS.seed_files {
            files.insert(relative, read(&entry.path(), BOUNDS.seed_file_bytes)?);
        } else {
            return Err(format!(
                "{relative}: seeds must be at most {} regular files, without links",
                BOUNDS.seed_files
            ));
        }
    }
    Ok(())
}

fn read(path: &Path, limit: usize) -> Result<String, String> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .and_then(|file| file.take(limit as u64 + 1).read_to_end(&mut bytes))
        .map_err(|e| format!("{}: {e}", path.display()))?;
    if bytes.len() > limit {
        return Err(format!("{}: larger than {limit} bytes", path.display()));
    }
    String::from_utf8(bytes).map_err(|_| format!("{}: not UTF-8", path.display()))
}
