//! Synthetic live-profile metadata for presentation tests; never launches a worker.
use playtime_protocol::api::Preview;
use serde_json::json;

pub fn live(mut preview: Preview) -> Preview {
    preview.prompt = "Explore the workshop. Leave a concise handoff for the next session.".into();
    preview.protocol = "Use the workshop tools to inspect files. Save lasting discoveries in the workspace.\n\nAt the end of each session, submit a short memento explaining what to try next.".into();
    preview.model_identity = json!({
        "adapter":"ollama", "name":"example-model:8b", "digest":"sha256:0123456789abcdef0123456789abcdef",
        "ollama_version":"synthetic-version", "endpoint":"http://127.0.0.1:11434", "addresses":["127.0.0.1:11434"],
        "settings":{"thinking":null,"temperature":0.7,"top_p":0.9,"top_k":40,"repeat_penalty":1.1}
    });
    preview.workshop_identity = json!({"adapter":"docker", "image":{"image_id":"sha256:abcdef0123456789abcdef0123456789"}, "daemon":{"name":"example-workshop-host","server_version":"synthetic-version"}});
    preview.effective_configuration = json!({
        "command_return_ms":2000, "resources":{"workspace_inodes":4096},
        "scenario":{"inventory":"Python, a shell, and image tools are installed.\n/workspace persists between sessions; /tmp is disposable."},
        "recipe":{"seeds":{"README.txt":"Start here.\n","notes/goal.txt":"Explore 🦀\n"}}
    });
    preview.seeds.files = 2;
    preview.seeds.bytes = 25;
    preview.parameters.limits.tool_parse_retries = 0;
    preview.parameters.limits.tool_parse_error_limit = None;
    preview
}
