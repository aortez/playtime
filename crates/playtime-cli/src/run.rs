use crate::{
    args::*,
    observe,
    output::{Failure, Output},
};
use playtime_client::{
    Client,
    config::{self, Environment},
    state::{LaunchReceipt, ReceiptStore},
};
use playtime_core::RunId;
use playtime_protocol::api::*;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

pub type Recovery = Arc<Mutex<Option<Value>>>;
pub async fn blocking<T: Send + 'static>(
    task: impl FnOnce() -> Result<T, Failure> + Send + 'static,
) -> Result<T, Failure> {
    tokio::task::spawn_blocking(task)
        .await
        .map_err(|_| Failure::new(1, "local_task", "local I/O task failed"))?
}
struct Context {
    client: Client,
    store: ReceiptStore,
    output: Output,
    explicit: bool,
    timeout: std::time::Duration,
    recovery: Recovery,
    cancel: CancellationToken,
}
impl Context {
    fn recover(&self, saved: &LaunchReceipt) -> Value {
        json!({"key":saved.key,"receipt_path":self.store.path(&saved.key),"run_id":saved.run_id,"service_id":saved.service_id})
    }
    async fn send(&self, saved: LaunchReceipt, observe: &Observe) -> Result<i32, Failure> {
        saved.check_target(
            self.client.service_id(),
            self.client.endpoint(),
            self.explicit,
        )?;
        *self.recovery.lock().unwrap() = Some(self.recover(&saved));
        self.output
            .diagnostic("notice", json!({"saved_launch":self.recover(&saved)}))
            .await?;
        let accepted = self
            .client
            .launch(&saved.pending())
            .await
            .map_err(Failure::from)?;
        let mut recovery = self.recover(&saved);
        recovery["run_id"] = json!(accepted.run_id);
        *self.recovery.lock().unwrap() = Some(recovery);
        let store = self.store.clone();
        let original = saved.clone();
        let id = accepted.run_id.clone();
        let saved = blocking(move || Ok(store.acknowledge(&original, &id)?)).await?;
        let result = json!({"receipt":saved,"receipt_path":self.store.path(&saved.key),"acceptance":accepted});
        if observe.follow {
            self.output.emit("launch", &result).await?;
            let view = observe::follow(
                &self.client,
                &accepted.run_id,
                None,
                None,
                self.output,
                Instant::now() + self.timeout,
                &self.cancel,
            )
            .await?;
            let exit = observe::outcome_exit(&view, false);
            self.output
                .emit("result", json!({"run":view,"exit_code":exit}))
                .await?;
            Ok(exit)
        } else if observe.wait {
            let view = observe::wait(
                &self.client,
                &accepted.run_id,
                Instant::now() + self.timeout,
            )
            .await?;
            let exit = observe::outcome_exit(&view, false);
            self.output
                .emit(
                    "result",
                    json!({"launch":result,"run":view,"exit_code":exit}),
                )
                .await?;
            Ok(exit)
        } else {
            self.output.emit("launch", result).await?;
            Ok(0)
        }
    }
    async fn existing(&self, key: &IdempotencyKey) -> Result<Option<LaunchReceipt>, Failure> {
        let store = self.store.clone();
        let key = key.clone();
        blocking(move || Ok(store.read(&key)?)).await
    }
    async fn launch(&self, args: &Launch) -> Result<i32, Failure> {
        if let Some(key) = &args.idempotency_key
            && let Some(saved) = self.existing(key).await?
        {
            if saved.request.recipe_id != args.recipe_id
                || saved.request.mode != args.mode.into()
                || args
                    .preview_revision
                    .as_ref()
                    .is_some_and(|r| *r != saved.request.preview_revision)
            {
                return Err(Failure::new(
                    5,
                    "idempotency_conflict",
                    "the key already has a different saved request",
                ));
            }
            return self.send(saved, &args.observe).await;
        }
        let mode = args.mode.into();
        if !self.client.capabilities().modes.contains(&mode) {
            return Err(Failure::new(
                6,
                "unsupported_capability",
                "this backend does not support the requested mode",
            ));
        }
        let preview_revision = if let Some(revision) = &args.preview_revision {
            revision.clone()
        } else {
            self.client
                .preview(&args.recipe_id, mode)
                .await?
                .preview_revision
        };
        let pending = self.client.prepare_launch(
            LaunchRequest {
                recipe_id: args.recipe_id.clone(),
                mode,
                preview_revision,
            },
            args.idempotency_key.clone(),
        )?;
        let store = self.store.clone();
        let endpoint = self.client.endpoint().to_owned();
        let saved = blocking(move || Ok(store.save(&pending, &endpoint)?)).await?;
        self.send(saved, &args.observe).await
    }
    async fn wait(&self, id: &RunId, cancellation: bool) -> Result<i32, Failure> {
        let view = observe::wait(&self.client, id, Instant::now() + self.timeout).await?;
        let exit = observe::outcome_exit(&view, cancellation);
        self.output
            .emit("result", json!({"run":view,"exit_code":exit}))
            .await?;
        Ok(exit)
    }
    async fn events(&self, args: &Events) -> Result<i32, Failure> {
        let cwd = std::env::current_dir().map_err(|_| {
            Failure::new(
                7,
                "cursor_path",
                "cannot resolve cursor path from current directory",
            )
        })?;
        let cursor_file = args.cursor_file.as_ref().map(|p| {
            if p.is_absolute() {
                p.clone()
            } else {
                cwd.join(p)
            }
        });
        let cursor = if cursor_file.is_some() {
            observe::load_cursor(cursor_file.as_deref(), &self.client, &args.run_id).await?
        } else {
            args.cursor.clone()
        };
        if args.follow {
            let view = observe::follow(
                &self.client,
                &args.run_id,
                cursor,
                cursor_file,
                self.output,
                Instant::now() + self.timeout,
                &self.cancel,
            )
            .await?;
            self.output.emit("result", &view).await?;
            return Ok(0); // Inspection does not classify the experiment outcome.
        }
        let mut query = PageQuery {
            cursor,
            limit: args.limit,
        };
        loop {
            let page = self.client.history(&args.run_id, &query).await?;
            if self.output.0 == Format::Json {
                self.output.emit("page", &page).await?;
                observe::persist(
                    cursor_file.as_deref(),
                    &self.client,
                    &args.run_id,
                    page.resume_cursor.clone(),
                )
                .await?;
            } else {
                for event in &page.events {
                    let cursor = EventCursor::new(page.frontier.clone(), event.sequence.get())
                        .and_then(|c| c.encode())
                        .map_err(|_| {
                            Failure::new(6, "invalid_cursor", "invalid history position")
                        })?;
                    self.output
                        .emit("event", json!({"event":event,"cursor":cursor}))
                        .await?;
                    observe::persist(cursor_file.as_deref(), &self.client, &args.run_id, cursor)
                        .await?;
                }
                self.output.emit("page", json!({"frontier":page.frontier,"has_more":page.has_more,"next_cursor":page.next_cursor,"resume_cursor":page.resume_cursor})).await?;
            }
            if !args.all || !page.has_more {
                break;
            }
            query.cursor = page.next_cursor;
        }
        Ok(0)
    }
}

pub async fn execute(
    args: &Cli,
    output: Output,
    recovery: Recovery,
    cancel: CancellationToken,
) -> Result<i32, Failure> {
    args.validate()
        .map_err(|m| Failure::new(2, "invalid_options", m))?;
    if let Command::Reserved(values) = &args.command {
        let name = values.first().and_then(|s| s.to_str()).unwrap_or("");
        let owner = match name {
            "prepare" | "check" => Some("#23/#24"),
            "smoke" => Some("#24"),
            "report" => Some("#26"),
            "repeat" | "pilot-prepare" | "pilot-run" => Some("#27"),
            "serve" | "worker" => Some("#20 (playtimed/internal worker)"),
            _ => None,
        };
        return Err(if let Some(owner) = owner {
            Failure::new(
                6,
                "unsupported_capability",
                format!(
                    "{name} is reserved; implementation belongs to {owner}; the Python launcher remains available"
                ),
            )
        } else {
            Failure::new(2, "invalid_command", "unknown command; see --help")
        });
    }
    let options = config::Options {
        connection_file: args.connection_file.clone(),
        endpoint: args.endpoint.clone(),
        token_file: args.token_file.clone(),
        config: args.config.clone(),
        state_dir: args.state_dir.clone(),
    };
    let env = Environment::capture();
    let cwd = std::env::current_dir()
        .map_err(|_| Failure::new(2, "configuration", "cannot determine current directory"))?;
    let resolved = blocking(move || Ok(config::resolve(&options, &env, &cwd)?)).await?;
    let store = ReceiptStore::new(&resolved.state_dir);
    if let Command::Launches(Launches::List(page)) = &args.command {
        let after = page
            .cursor
            .as_ref()
            .map(IdempotencyKey::new)
            .transpose()
            .map_err(|_| Failure::new(2, "invalid_cursor", "invalid local receipt cursor"))?;
        let limit = page.limit.unwrap_or(200) as usize;
        let page = blocking(move || Ok(store.list(after.as_ref(), limit)?)).await?;
        output.emit("page", page).await?;
        return Ok(0);
    }
    let selection = resolved.clone();
    let loaded = blocking(move || Ok(selection.load()?)).await?;
    let client = loaded.connect().await?;
    output
        .diagnostic(
            "notice",
            json!({"backend":client.capabilities().backend,"service_id":client.service_id()}),
        )
        .await?;
    let ctx = Context {
        client,
        store,
        output,
        explicit: resolved.explicit_connection,
        timeout: args.timeout,
        recovery,
        cancel,
    };
    match &args.command {
        Command::Service(Service::Status) => {
            output
                .emit(
                    "result",
                    json!({"capabilities":ctx.client.capabilities(),"connection":resolved}),
                )
                .await?
        }
        Command::Recipes(Recipes::List(page)) => {
            output
                .emit("page", ctx.client.recipes(&page.query()).await?)
                .await?
        }
        Command::Recipes(Recipes::Preview { recipe_id, mode }) => {
            output
                .emit(
                    "result",
                    ctx.client.preview(recipe_id, (*mode).into()).await?,
                )
                .await?
        }
        Command::Run(launch) => return ctx.launch(launch).await,
        Command::Launches(Launches::Show { key }) => {
            let saved = ctx.existing(key).await?;
            let reply = if let Some(saved) = &saved {
                saved.check_target(ctx.client.service_id(), ctx.client.endpoint(), ctx.explicit)?;
                *ctx.recovery.lock().unwrap() = Some(ctx.recover(saved));
                let reply = ctx.client.lookup(&saved.pending()).await?;
                let store = ctx.store.clone();
                let original = saved.clone();
                let id = reply.run_id.clone();
                blocking(move || Ok(store.acknowledge(&original, &id)?)).await?;
                reply
            } else {
                ctx.client.lookup_key(key).await?
            };
            output.emit("launch", reply).await?;
        }
        Command::Launches(Launches::Retry { key, observe }) => {
            let saved = ctx.existing(key).await?.ok_or_else(|| {
                Failure::new(
                    5,
                    "not_found",
                    "no saved request for that key; retry cannot create a new request",
                )
            })?;
            return ctx.send(saved, observe).await;
        }
        Command::Runs(Runs::List(page)) => {
            output
                .emit("page", ctx.client.runs(&page.query()).await?)
                .await?
        }
        Command::Runs(Runs::Show { run_id }) => {
            output.emit("result", ctx.client.run(run_id).await?).await?
        }
        Command::Runs(Runs::Wait { run_id }) => return ctx.wait(run_id, false).await,
        Command::Runs(Runs::Cancel { run_id, wait }) => {
            let reply = ctx.client.cancel(run_id).await?;
            if *wait {
                return ctx.wait(run_id, true).await;
            }
            output.emit("result", reply).await?;
        }
        Command::Events(events) => return ctx.events(events).await,
        Command::Checkpoints(Checkpoints::List { run_id, page }) => {
            output
                .emit("page", ctx.client.checkpoints(run_id, &page.query()).await?)
                .await?
        }
        Command::Checkpoints(Checkpoints::Show { run_id, session_id }) => {
            output
                .emit("result", ctx.client.checkpoint(run_id, session_id).await?)
                .await?
        }
        Command::Artifacts(Artifacts::List { run_id, page }) => {
            output
                .emit("page", ctx.client.artifacts(run_id, &page.query()).await?)
                .await?
        }
        Command::Artifacts(Artifacts::Show {
            run_id,
            artifact_id,
        }) => {
            output
                .emit("result", ctx.client.artifact(run_id, artifact_id).await?)
                .await?
        }
        Command::Artifacts(Artifacts::Download {
            run_id,
            artifact_id,
            output: path,
            force,
        }) => {
            let artifact = ctx.client.artifact(run_id, artifact_id).await?;
            let bytes = ctx
                .client
                .download_to_path_with_force(run_id, &artifact, path, *force, &ctx.cancel)
                .await?;
            output.emit("result", json!({"run_id":run_id,"artifact":artifact,"output":path,"verified_bytes":bytes})).await?;
        }
        Command::Launches(Launches::List(_)) | Command::Reserved(_) => unreachable!(),
    }
    Ok(0)
}
