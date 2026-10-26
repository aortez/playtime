//! Transactional recipe authoring, independent of execution readiness.
use crate::{
    application::{ApiError, Result},
    lifecycle::Ledger,
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use playtime_core::RunMode;
use playtime_protocol::api::*;
use playtime_runner::docker::{FrozenRecipe, Recipe as RunRecipe};
use playtime_worker_control::{digest, random_id};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, sync::Arc};

const ROW_BYTES: usize = 4 << 20;
const CATALOG_BYTES: usize = 128 << 20;
const RECIPES: u64 = 256;
const REVISIONS: u64 = 4096;

pub(crate) fn migrate(db: &Connection) -> rusqlite::Result<()> {
    db.execute_batch("CREATE TABLE recipe_heads (
        seq INTEGER PRIMARY KEY AUTOINCREMENT, id TEXT NOT NULL UNIQUE, head TEXT NOT NULL);
        CREATE TABLE recipe_revisions (
        execution TEXT PRIMARY KEY, recipe TEXT NOT NULL REFERENCES recipe_heads(id),
        ordinal INTEGER NOT NULL, digest TEXT NOT NULL, name TEXT NOT NULL,
        definition TEXT NOT NULL, UNIQUE(recipe,ordinal));
        CREATE TABLE recipe_saves (key TEXT PRIMARY KEY, request TEXT NOT NULL, response TEXT NOT NULL);")
}
fn database(_: rusqlite::Error) -> ApiError {
    ApiError::new("catalog_corrupt")
}
fn invalid() -> ApiError {
    ApiError::new("invalid_parameters")
}
fn encode<T: Serialize>(value: &T) -> Result<String> {
    let bytes = crate::http::bounded_json(value, ROW_BYTES)?;
    String::from_utf8(bytes).map_err(|_| invalid())
}
fn decode<T: serde::de::DeserializeOwned>(value: &str) -> Result<T> {
    if value.len() > ROW_BYTES {
        return Err(ApiError::new("catalog_corrupt"));
    }
    serde_json::from_str(value).map_err(|_| ApiError::new("catalog_corrupt"))
}
fn id(value: &str) -> Result<RecipeId> {
    RecipeId::new(value).map_err(|_| ApiError::new("catalog_corrupt"))
}

#[derive(Clone, Copy)]
struct Budget {
    families: u64,
    revisions: u64,
    bytes: usize,
    page_bytes: usize,
}
impl Default for Budget {
    fn default() -> Self {
        Self {
            families: RECIPES,
            revisions: REVISIONS,
            bytes: CATALOG_BYTES,
            page_bytes: ApiLimits::default().json_bytes,
        }
    }
}

#[derive(Clone)]
pub struct Catalog {
    ledger: Ledger,
    sources: Arc<BTreeMap<String, FrozenRecipe>>,
    generation: String,
    budget: Budget,
}
impl Catalog {
    pub fn new(ledger: Ledger, recipes: Vec<RunRecipe>) -> Result<Self> {
        let mut sources = BTreeMap::new();
        for recipe in recipes {
            if recipe.id.starts_with("saved-") || sources.contains_key(&recipe.id) {
                return Err(invalid());
            }
            sources.insert(
                recipe.id.clone(),
                FrozenRecipe::current(recipe).map_err(|_| invalid())?,
            );
        }
        let identities: BTreeMap<_, _> = sources
            .iter()
            .map(|(id, value)| Ok((id.clone(), value.digest().map_err(|_| invalid())?)))
            .collect::<Result<_>>()?;
        let generation = digest(encode(&identities)?.as_bytes()).to_string();
        Ok(Self {
            ledger,
            sources: Arc::new(sources),
            generation,
            budget: Budget::default(),
        })
    }
    async fn call<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Connection) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        self.ledger
            .call(move |db| Ok(f(db)))
            .await
            .map_err(|_| ApiError::new("backend_unavailable"))?
    }
    /// Startup validation must not replace damaged history with an empty catalog.
    pub async fn check(&self) -> Result<()> {
        let service = self.ledger.service_id().as_str().to_owned();
        self.call(move |db| check_catalog(db, service)).await
    }
    pub async fn frozen(&self, execution: &str) -> Result<(FrozenRecipe, Option<SavedRecipe>)> {
        let execution = execution.to_owned();
        let sources = self.sources.clone();
        self.call(move |db| frozen(db, &sources, &execution)).await
    }
    pub async fn model_settings(&self, execution: &RecipeId) -> Result<RecipeSettingsView> {
        self.frozen(execution.as_str())
            .await?
            .0
            .settings_view()
            .map_err(|_| ApiError::new("unsupported_capability"))
    }
    pub async fn harness(&self, execution: &RecipeId) -> Result<RecipeHarnessView> {
        self.frozen(execution.as_str())
            .await?
            .0
            .harness_view()
            .map_err(|_| ApiError::new("catalog_corrupt"))
    }
    pub async fn check_context(&self, execution: &RecipeId) -> Result<()> {
        self.frozen(execution.as_str())
            .await?
            .0
            .check_initial_context()
            .map_err(settings_error)
    }
    pub async fn definition(&self, execution: &RecipeId) -> Result<RecipeDefinition> {
        let (definition, saved) = self.frozen(execution.as_str()).await?;
        definition
            .view(saved)
            .map_err(|_| ApiError::new("catalog_corrupt"))
    }
    pub async fn lookup(&self, key: &IdempotencyKey) -> Result<RecipeSaveResponse> {
        let key = key.to_string();
        self.call(move |db| replay(db, &key, None)?.ok_or_else(|| ApiError::new("not_found")))
            .await
    }
    pub async fn save(
        &self,
        key: IdempotencyKey,
        request: RecipeMutation,
    ) -> Result<RecipeSaveResponse> {
        let catalog = self.clone();
        self.call(move |db| save_recipe(db, &catalog, key, request))
            .await
    }
    pub async fn head(&self, recipe: &RecipeId) -> Result<SavedRecipe> {
        let recipe = recipe.to_string();
        self.call(move |db| {
            let execution: String = db
                .query_row("SELECT head FROM recipe_heads WHERE id=?1", [recipe], |r| {
                    r.get(0)
                })
                .optional()
                .map_err(database)?
                .ok_or_else(|| ApiError::new("not_found"))?;
            metadata(db, &execution)
        })
        .await
    }
    /// A page's heads are read in one transaction. The high-water mark is a
    /// creation sequence, never a head ID, name, or timestamp.
    pub async fn page(
        &self,
        query: PageQuery,
        history: Option<RecipeId>,
        combined: Option<Vec<Recipe>>,
    ) -> Result<Page<Recipe>> {
        let catalog = self.clone();
        self.call(move |db| list_page(db, &catalog, query, history, combined))
            .await
    }
}

fn check_catalog(db: &mut Connection, service: String) -> Result<()> {
    let corrupt = || ApiError::new("catalog_corrupt");
    let heads: u64 = db
        .query_row("SELECT count(*) FROM recipe_heads", [], |r| integer(r, 0))
        .map_err(database)?;
    let revisions: u64 = db
        .query_row("SELECT count(*) FROM recipe_revisions", [], |r| {
            integer(r, 0)
        })
        .map_err(database)?;
    let saves: u64 = db
        .query_row("SELECT count(*) FROM recipe_saves", [], |r| integer(r, 0))
        .map_err(database)?;
    let bytes: u64 = db.query_row("SELECT (SELECT coalesce(sum(length(CAST(definition AS BLOB))),0) FROM recipe_revisions)
            + (SELECT coalesce(sum(length(CAST(request AS BLOB))+length(CAST(response AS BLOB))),0)
            FROM recipe_saves)", [], |r| integer(r,0)).map_err(database)?;
    if heads > RECIPES
        || revisions > REVISIONS
        || saves != revisions
        || bytes > CATALOG_BYTES as u64
    {
        return Err(corrupt());
    }
    let broken: bool = db
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM recipe_heads h LEFT JOIN recipe_revisions r ON
            r.execution=h.head WHERE h.seq<1 OR length(h.id)>128 OR length(h.head)>128 OR r.recipe
            IS NULL OR r.recipe!=h.id OR (SELECT max(ordinal) FROM recipe_revisions WHERE
            recipe=h.id)!=(SELECT count(*) FROM recipe_revisions WHERE recipe=h.id) OR
            r.ordinal!=(SELECT count(*) FROM recipe_revisions WHERE recipe=h.id)) OR EXISTS(SELECT 1
            FROM recipe_revisions r LEFT JOIN recipe_heads h ON h.id=r.recipe WHERE h.id IS NULL OR
            r.ordinal<1 OR r.ordinal>4096 OR length(r.execution)>128 OR length(r.digest)!=64 OR
            length(CAST(r.name AS BLOB))>128)",
            [],
            |r| r.get(0),
        )
        .map_err(database)?;
    if broken {
        return Err(corrupt());
    }
    let mut statement = db
        .prepare("SELECT CASE WHEN length(key)<=128 THEN key ELSE NULL END FROM recipe_saves")
        .map_err(database)?;
    let keys = statement
        .query_map([], |r| r.get::<_, String>(0))
        .map_err(database)?;
    let mut executions = std::collections::BTreeSet::new();
    for key in keys {
        let key = key.map_err(database)?;
        IdempotencyKey::new(&key).map_err(|_| corrupt())?;
        let reply = replay(db, &key, None)?.ok_or_else(corrupt)?;
        if reply.service_id.as_str() != service
            || !executions.insert(reply.definition.execution_recipe_id.clone())
        {
            return Err(corrupt());
        }
        let (definition, saved) = frozen(
            db,
            &BTreeMap::new(),
            reply.definition.execution_recipe_id.as_str(),
        )?;
        if definition.view(saved).map_err(|_| corrupt())? != reply.definition {
            return Err(corrupt());
        }
    }
    Ok(())
}

fn save_recipe(
    db: &mut Connection,
    catalog: &Catalog,
    key: IdempotencyKey,
    request: RecipeMutation,
) -> Result<RecipeSaveResponse> {
    let budget = catalog.budget;
    let sources = catalog.sources.clone();
    let service_id = ServiceId::new(catalog.ledger.service_id().as_str()).unwrap();

    let tx = db
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(database)?;
    let request_json = encode(&request)?;
    if let Some(reply) = replay(&tx, key.as_str(), Some(&request_json))? {
        return Ok(reply);
    }
    let (execution, name, prompt, family, expected_digest) = match &request {
        RecipeMutation::Clone { request } => (
            request.source.execution_recipe_id.as_str(),
            &request.name,
            &request.prompt,
            None,
            Some(request.source.definition_digest.as_str()),
        ),
        RecipeMutation::Revise { recipe_id, request } => (
            request.expected_head.as_str(),
            &request.name,
            &request.prompt,
            Some(recipe_id.as_str()),
            None,
        ),
    };
    if name.trim().is_empty()
        || name.len() > 128
        || name.chars().any(char::is_control)
        || prompt.trim().is_empty()
        || prompt.len() > 65536
    {
        return Err(invalid());
    }
    let (mut definition, previous) = frozen(&tx, &sources, execution)?;
    let source_digest = definition.digest().map_err(|_| invalid())?;
    if expected_digest.is_some_and(|d| d != source_digest) {
        return Err(ApiError::new("source_conflict"));
    }
    let (recipe_id, ordinal, creation) = if let Some(family) = family {
        let old = previous.ok_or_else(|| ApiError::new("head_conflict"))?;
        let head: Option<String> = tx
            .query_row("SELECT head FROM recipe_heads WHERE id=?1", [family], |r| {
                r.get(0)
            })
            .optional()
            .map_err(database)?;
        if old.recipe_id.as_str() != family || head.as_deref() != Some(execution) {
            return Err(ApiError::new("head_conflict"));
        }
        (old.recipe_id, old.revision + 1, old.creation_sequence)
    } else {
        let count: u64 = tx
            .query_row("SELECT count(*) FROM recipe_heads", [], |r| integer(r, 0))
            .map_err(database)?;
        if count >= budget.families {
            return Err(ApiError::new("catalog_full"));
        }
        let recipe_id = id(&format!(
            "recipe-{}",
            random_id()
                .map_err(|_| ApiError::new("internal_error"))?
                .as_str()
        ))?;
        tx.execute(
            "INSERT INTO recipe_heads(id,head) VALUES(?1,'')",
            [recipe_id.as_str()],
        )
        .map_err(database)?;
        (recipe_id, 1, tx.last_insert_rowid() as u64)
    };
    let execution_id = id(&format!(
        "saved-{}",
        random_id()
            .map_err(|_| ApiError::new("internal_error"))?
            .as_str()
    ))?;
    definition.source = Some(RecipeSource {
        execution_recipe_id: id(execution)?,
        definition_digest: source_digest,
    });
    definition.recipe.id = execution_id.to_string();
    definition.recipe.name = name.clone();
    definition.recipe.prompt = prompt.clone();
    let harness = match &request {
        RecipeMutation::Clone { request } => &request.harness,
        RecipeMutation::Revise { request, .. } => &request.harness,
    };
    if let Some(id) = harness {
        definition.select_harness(id).map_err(settings_error)?;
    }
    let compaction = match &request {
        RecipeMutation::Clone { request } => &request.compaction,
        RecipeMutation::Revise { request, .. } => &request.compaction,
    };
    if let Some(settings) = compaction {
        definition
            .apply_compaction_settings(settings)
            .map_err(settings_error)?;
    }
    let selected = match &request {
        RecipeMutation::Clone { request } => &request.model_registration,
        RecipeMutation::Revise { request, .. } => &request.model_registration,
    };
    if let Some(id) = selected {
        let model = crate::registrations::retained(&tx, id).map_err(|error| {
            if error.code() == "not_found" {
                let mut error = invalid();
                error.0.message = "The selected model registration is unavailable. Refresh registered models and choose an existing identity.".into();
                error
            } else { error }
        })?;
        definition
            .select_registered_model(model)
            .map_err(settings_error)?;
    }
    let settings = match &request {
        RecipeMutation::Clone { request } => &request.model_settings,
        RecipeMutation::Revise { request, .. } => &request.model_settings,
    };
    if let Some(settings) = settings {
        definition
            .apply_model_settings(settings)
            .map_err(settings_error)?;
    }
    definition.validate().map_err(|_| invalid())?;
    definition
        .check_model_settings(&definition.model_settings().map_err(settings_error)?)
        .map_err(settings_error)?;
    definition.check_initial_context().map_err(settings_error)?;
    let definition_json = encode(&definition)?;
    let saved = SavedRecipe {
        recipe_id: recipe_id.clone(),
        execution_recipe_id: execution_id.clone(),
        revision: ordinal,
        creation_sequence: creation,
        definition_digest: definition.digest().map_err(|_| invalid())?,
        name: name.clone(),
    };
    let response = RecipeSaveResponse {
        service_id,
        key: key.clone(),
        request: request.clone(),
        definition: definition
            .view(Some(saved.clone()))
            .map_err(|_| invalid())?,
        replayed: false,
    };
    let response_json = encode(&response)?;
    let (count, bytes): (u64, u64) = tx.query_row(
                "SELECT count(*), coalesce(sum(length(CAST(definition AS BLOB))),0) FROM recipe_revisions", [], |r| Ok((integer(r,0)?,integer(r,1)?))).map_err(database)?;
    let receipts: u64 = tx
        .query_row(
            "SELECT coalesce(sum(length(CAST(request AS BLOB))+length(CAST(response AS BLOB))),0)
            FROM recipe_saves",
            [],
            |r| integer(r, 0),
        )
        .map_err(database)?;
    if count >= budget.revisions
        || bytes
            + receipts
            + (definition_json.len() + request_json.len() + response_json.len()) as u64
            > budget.bytes as u64
    {
        return Err(ApiError::new("catalog_full"));
    }
    tx.execute(
        "INSERT INTO recipe_revisions VALUES(?1,?2,?3,?4,?5,?6)",
        params![
            execution_id.as_str(),
            recipe_id.as_str(),
            ordinal as i64,
            saved.definition_digest,
            name,
            definition_json
        ],
    )
    .map_err(database)?;
    tx.execute(
        "UPDATE recipe_heads SET head=?2 WHERE id=?1",
        params![recipe_id.as_str(), execution_id.as_str()],
    )
    .map_err(database)?;
    tx.execute(
        "INSERT INTO recipe_saves VALUES(?1,?2,?3)",
        params![key.as_str(), request_json, response_json],
    )
    .map_err(database)?;
    tx.commit().map_err(database)?;
    Ok(response)
}

fn list_page(
    db: &mut Connection,
    catalog: &Catalog,
    query: PageQuery,
    history: Option<RecipeId>,
    combined: Option<Vec<Recipe>>,
) -> Result<Page<Recipe>> {
    let page_bytes = catalog.budget.page_bytes;
    let service = catalog.ledger.service_id().as_str().to_owned();
    let generation = if let Some(items) = &combined {
        digest(encode(&(catalog.generation.clone(), items))?.as_bytes()).to_string()
    } else {
        String::new()
    };
    let sources = catalog.sources.clone();

    let tx = db.transaction().map_err(database)?;
    let kind = if combined.is_some() {
        "picker"
    } else if history.is_some() {
        "history"
    } else {
        "saved"
    };
    let scope = format!(
        "{service}/{kind}/{}",
        history.as_ref().map_or("", |id| id.as_str())
    );
    let limit = query.limit(&ApiLimits::default()).map_err(|_| invalid())? as usize;
    let mut cursor = match &query.cursor {
        Some(encoded) => {
            if encoded.len() > 2048 {
                return Err(ApiError::new("invalid_cursor"));
            }
            let bytes = URL_SAFE_NO_PAD
                .decode(encoded)
                .map_err(|_| ApiError::new("invalid_cursor"))?;
            let c: CatalogCursor =
                serde_json::from_slice(&bytes).map_err(|_| ApiError::new("invalid_cursor"))?;
            if c.version != 1
                || c.scope != scope
                || c.generation != generation
                || c.after.len() > 256
                || c.cutoff > i64::MAX as u64
                || c.revision_cutoff > REVISIONS
            {
                return Err(ApiError::new("invalid_cursor"));
            }
            c
        }
        None => CatalogCursor {
            version: 1,
            scope,
            generation,
            after: String::new(),
            cutoff: tx
                .query_row("SELECT coalesce(max(seq),0) FROM recipe_heads", [], |r| {
                    integer(r, 0)
                })
                .map_err(database)?,
            revision_cutoff: tx
                .query_row(
                    "SELECT coalesce(max(ordinal),0) FROM recipe_revisions WHERE recipe=?1",
                    [history.as_ref().map_or("", |id| id.as_str())],
                    |r| integer(r, 0),
                )
                .map_err(database)?,
        },
    };
    let mut rows = Vec::new();
    if let Some(items) = combined {
        for r in items {
            rows.push((format!("0-{}", r.recipe_id), r));
        }
        for (execution, definition) in sources.iter() {
            rows.push((
                format!("0-{execution}"),
                Recipe {
                    recipe_id: id(execution)?,
                    name: definition.recipe.name.clone(),
                    modes: vec![RunMode::Experiment],
                    saved: None,
                },
            ));
        }
    }
    let sql = if history.is_some() {
        "SELECT r.execution FROM recipe_revisions r JOIN recipe_heads h ON h.id=r.recipe WHERE
            r.recipe=?1 AND r.ordinal<=?2 ORDER BY r.ordinal"
    } else {
        "SELECT head FROM recipe_heads WHERE seq<=?2 ORDER BY seq"
    };
    let mut statement = tx.prepare(sql).map_err(database)?;
    let mut entries = statement
        .query(params![
            history.as_ref().map_or("", |id| id.as_str()),
            (if history.is_some() {
                cursor.revision_cutoff
            } else {
                cursor.cutoff
            }) as i64
        ])
        .map_err(database)?;
    while let Some(row) = entries.next().map_err(database)? {
        let saved = metadata(&tx, &row.get::<_, String>(0).map_err(database)?)?;
        let order = if history.is_some() {
            saved.revision
        } else {
            saved.creation_sequence
        };
        rows.push((
            format!("1-{order:020}-{}", saved.recipe_id),
            Recipe {
                recipe_id: saved.execution_recipe_id.clone(),
                name: saved.name.clone(),
                modes: vec![RunMode::Experiment],
                saved: Some(saved),
            },
        ));
    }
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    let mut items = Vec::new();
    let mut bytes = 1600;
    let mut more = false;
    for (key, row) in rows {
        if key <= cursor.after {
            continue;
        }
        let size = encode(&row)?.len() + 1;
        if items.len() == limit || bytes + size > page_bytes {
            if items.is_empty() {
                return Err(ApiError::new("catalog_corrupt"));
            }
            more = true;
            break;
        }
        bytes += size;
        cursor.after = key;
        items.push(row);
    }
    Ok(Page {
        items,
        next_cursor: more.then(|| URL_SAFE_NO_PAD.encode(serde_json::to_vec(&cursor).unwrap())),
        frontier: None,
    })
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CatalogCursor {
    version: u32,
    scope: String,
    generation: String,
    after: String,
    cutoff: u64,
    revision_cutoff: u64,
}

fn metadata(db: &Connection, execution: &str) -> Result<SavedRecipe> {
    db.query_row(
        "SELECT CASE WHEN length(h.id)<=128 THEN h.id ELSE NULL END,r.ordinal,h.seq,CASE WHEN
            length(r.digest)=64 THEN r.digest ELSE NULL END,CASE WHEN length(CAST(r.name AS
            BLOB))<=128 THEN r.name ELSE NULL END FROM recipe_revisions r JOIN recipe_heads h ON
            h.id=r.recipe WHERE r.execution=?1",
        [execution],
        |r| {
            Ok((
                r.get::<_, String>(0)?,
                integer(r, 1)?,
                integer(r, 2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
            ))
        },
    )
    .optional()
    .map_err(database)?
    .ok_or_else(|| ApiError::new("not_found"))
    .and_then(
        |(family, revision, creation_sequence, definition_digest, name)| {
            Ok(SavedRecipe {
                recipe_id: id(&family)?,
                execution_recipe_id: id(execution)?,
                revision,
                creation_sequence,
                definition_digest,
                name,
            })
        },
    )
}
fn frozen(
    db: &Connection,
    sources: &BTreeMap<String, FrozenRecipe>,
    execution: &str,
) -> Result<(FrozenRecipe, Option<SavedRecipe>)> {
    if let Some(value) = sources.get(execution) {
        return Ok((value.clone(), None));
    }
    let saved = metadata(db, execution)?;
    let json:String=db.query_row("SELECT CASE WHEN length(CAST(definition AS BLOB))<=4194304 THEN definition ELSE NULL END
            FROM recipe_revisions WHERE execution=?1",[execution],|r|r.get(0)).map_err(database)?;
    let value: FrozenRecipe = decode(&json)?;
    value
        .validate()
        .map_err(|_| ApiError::new("catalog_corrupt"))?;
    if value.recipe.id != execution
        || value.recipe.name != saved.name
        || value
            .digest()
            .map_err(|_| ApiError::new("catalog_corrupt"))?
            != saved.definition_digest
    {
        return Err(ApiError::new("catalog_corrupt"));
    }
    Ok((value, Some(saved)))
}
fn replay(db: &Connection, key: &str, request: Option<&str>) -> Result<Option<RecipeSaveResponse>> {
    let stored:Option<(String,String)>=db.query_row("SELECT CASE WHEN length(CAST(request AS BLOB))<=4194304 THEN request ELSE NULL END,CASE
            WHEN length(CAST(response AS BLOB))<=4194304 THEN response ELSE NULL END FROM
            recipe_saves WHERE key=?1",[key],|r|Ok((r.get(0)?,r.get(1)?))).optional().map_err(database)?;
    stored
        .map(|(original, response)| {
            if request.is_some_and(|r| r != original) {
                return Err(ApiError::new("idempotency_conflict"));
            }
            let mut response: RecipeSaveResponse = decode(&response)?;
            if response.key.as_str() != key || encode(&response.request)? != original {
                return Err(ApiError::new("catalog_corrupt"));
            }
            let definition = &response.definition;
            let saved = definition
                .saved
                .as_ref()
                .ok_or_else(|| ApiError::new("catalog_corrupt"))?;
            let source = definition
                .source
                .as_ref()
                .ok_or_else(|| ApiError::new("catalog_corrupt"))?;
            let (name, prompt, consistent) = match &response.request {
                RecipeMutation::Clone { request } => (
                    &request.name,
                    &request.prompt,
                    request.source == *source && saved.revision == 1,
                ),
                RecipeMutation::Revise { recipe_id, request } => (
                    &request.name,
                    &request.prompt,
                    *recipe_id == saved.recipe_id
                        && request.expected_head == source.execution_recipe_id
                        && saved.revision > 1,
                ),
            };
            if !consistent || *name != definition.name || *prompt != definition.prompt {
                return Err(ApiError::new("catalog_corrupt"));
            }
            let (frozen, saved) = frozen(
                db,
                &BTreeMap::new(),
                definition.execution_recipe_id.as_str(),
            )?;
            if frozen
                .view(saved)
                .map_err(|_| ApiError::new("catalog_corrupt"))?
                != *definition
            {
                return Err(ApiError::new("catalog_corrupt"));
            }
            response.replayed = true;
            Ok(response)
        })
        .transpose()
}

fn integer(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<u64> {
    let value: i64 = row.get(index)?;
    value.try_into().map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            index,
            rusqlite::types::Type::Integer,
            Box::new(error),
        )
    })
}

fn settings_error(error: playtime_runner::Error) -> ApiError {
    let mut result = ApiError::new("invalid_parameters");
    if let playtime_runner::Error::Invalid(message) = error {
        result.0.message = message;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn capacity_refusals_preserve_replays_and_byte_pages_keep_the_cutoff() {
        let path = std::env::temp_dir().join(format!(
            "playtime-catalog-{}",
            random_id().unwrap().as_str()
        ));
        std::fs::create_dir(&path).unwrap();
        let ledger = Ledger::open(&path).await.unwrap();
        let recipes = crate::daemon::catalog::load(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../recipes"),
        )
        .unwrap();
        let mut catalog = Catalog::new(ledger, recipes).unwrap();
        let source = catalog
            .definition(&RecipeId::new("curiosity").unwrap())
            .await
            .unwrap();
        let request = RecipeMutation::Clone {
            request: CloneRecipeRequest {
                compaction: None,
                harness: None,
                model_registration: None,
                model_settings: None,
                source: RecipeSource {
                    execution_recipe_id: source.execution_recipe_id,
                    definition_digest: source.definition_digest,
                },
                name: "clone".into(),
                prompt: "new prompt".into(),
            },
        };
        let key = IdempotencyKey::new("original").unwrap();
        let original = catalog.save(key.clone(), request.clone()).await.unwrap();
        for budget in [
            Budget {
                families: 1,
                ..Default::default()
            },
            Budget {
                revisions: 1,
                ..Default::default()
            },
            Budget {
                bytes: 1,
                ..Default::default()
            },
        ] {
            catalog.budget = budget;
            assert_eq!(
                catalog
                    .save(IdempotencyKey::new("refused").unwrap(), request.clone())
                    .await
                    .unwrap_err()
                    .code(),
                "catalog_full"
            );
            assert_eq!(
                catalog
                    .lookup(&IdempotencyKey::new("refused").unwrap())
                    .await
                    .unwrap_err()
                    .code(),
                "not_found"
            );
            assert_eq!(
                catalog
                    .save(key.clone(), request.clone())
                    .await
                    .unwrap()
                    .definition,
                original.definition
            );
            assert_eq!(
                catalog
                    .page(PageQuery::default(), None, None)
                    .await
                    .unwrap()
                    .items
                    .len(),
                1
            );
            catalog.check().await.unwrap();
        }
        catalog.budget = Budget::default();
        for n in 0..5 {
            catalog
                .save(
                    IdempotencyKey::new(format!("n{n}")).unwrap(),
                    request.clone(),
                )
                .await
                .unwrap();
        }
        let first = catalog
            .page(PageQuery::default(), None, None)
            .await
            .unwrap();
        catalog.budget.page_bytes = 1600 + encode(&first.items[0]).unwrap().len() + 2;
        let mut cursor = None;
        let mut ids = std::collections::BTreeSet::new();
        loop {
            let page = catalog
                .page(
                    PageQuery {
                        cursor,
                        limit: Some(100),
                    },
                    None,
                    None,
                )
                .await
                .unwrap();
            assert_eq!(page.items.len(), 1);
            assert!(ids.insert(page.items[0].saved.as_ref().unwrap().recipe_id.clone()));
            cursor = page.next_cursor;
            if cursor.is_none() {
                break;
            }
        }
        assert_eq!(ids.len(), 6);
        drop(catalog);
        std::fs::remove_dir_all(path).unwrap();
    }
}
