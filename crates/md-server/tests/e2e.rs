//! End-to-end MCP protocol test: an in-process client drives the real server
//! over a duplex transport, exercising initialize, tools/list, and a tool call.

use md_core::Vault;
use md_server::MdServer;
use rmcp::model::CallToolRequestParams;
use rmcp::{ServiceExt, serve_client};
use serde_json::{Value, json};

#[tokio::test]
async fn client_lists_all_tools_and_calls_one() {
    let dir = tempfile::tempdir().unwrap();
    let vault = Vault::open(dir.path()).unwrap();
    vault
        .write_atomic("hello.md", b"---\ntitle: Hi\n---\n# Heading\nbody\n")
        .unwrap();
    let server = MdServer::new(vault);

    let (client_transport, server_transport) = tokio::io::duplex(16 * 1024);
    let server_task = tokio::spawn(async move {
        if let Ok(running) = server.serve(server_transport).await {
            running.waiting().await.ok();
        }
    });

    let client = serve_client((), client_transport)
        .await
        .expect("client handshake");

    // tools/list advertises the full 12-tool surface.
    let tools = client.list_all_tools().await.expect("list tools");
    let names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
    assert_eq!(tools.len(), 12, "tools: {names:?}");
    for expected in [
        "read_notes",
        "read_outlines",
        "read_sections",
        "list_notes",
        "search_notes",
        "create_notes",
        "append_notes",
        "edit_sections",
        "edit_properties",
        "rename_notes",
        "relocate_notes",
        "delete_notes",
    ] {
        assert!(
            names.contains(&expected),
            "missing tool {expected}; have {names:?}"
        );
    }

    // Calling read_notes over the protocol returns structured output.
    let mut args = serde_json::Map::new();
    args.insert("paths".into(), json!(["hello.md", "missing.md"]));
    let result = client
        .call_tool(CallToolRequestParams::new("read_notes").with_arguments(args))
        .await
        .expect("call read_notes");
    assert_ne!(result.is_error, Some(true));

    let structured: Value = result.structured_content.expect("structured content");
    let notes = structured["notes"].as_array().unwrap();
    assert_eq!(notes.len(), 2);
    assert_eq!(notes[0]["exists"], json!(true));
    assert_eq!(notes[0]["frontmatter"]["title"], json!("Hi"));
    assert_eq!(notes[1]["exists"], json!(false));

    client.cancel().await.ok();
    server_task.abort();
}

/// The advertised inputSchema must surface the constraints `docs/tool_spec.md`
/// defines: batch arrays cap at maxItems:100, the paged `limit`s carry their
/// maximum, search `mode` defaults to "both", and `edit_sections` documents the
/// `move` operation in its description (not only its enum).
#[tokio::test]
async fn tool_schemas_expose_spec_constraints() {
    let dir = tempfile::tempdir().unwrap();
    let vault = Vault::open(dir.path()).unwrap();
    let server = MdServer::new(vault);

    let (client_transport, server_transport) = tokio::io::duplex(16 * 1024);
    let server_task = tokio::spawn(async move {
        if let Ok(running) = server.serve(server_transport).await {
            running.waiting().await.ok();
        }
    });
    let client = serve_client((), client_transport)
        .await
        .expect("client handshake");
    let tools = client.list_all_tools().await.expect("list tools");

    let schema = |name: &str| -> Value {
        let tool = tools
            .iter()
            .find(|t| t.name.as_ref() == name)
            .unwrap_or_else(|| panic!("missing tool {name}"));
        Value::Object((*tool.input_schema).clone())
    };

    // Every batch array advertises maxItems:100 (the batch_limit cap).
    for (tool, array) in [
        ("read_notes", "paths"),
        ("read_outlines", "paths"),
        ("read_sections", "targets"),
        ("create_notes", "notes"),
        ("append_notes", "appends"),
        ("edit_sections", "edits"),
        ("edit_properties", "edits"),
        ("delete_notes", "paths"),
        ("rename_notes", "renames"),
        ("relocate_notes", "moves"),
    ] {
        assert_eq!(
            schema(tool)["properties"][array]["maxItems"],
            json!(100),
            "{tool}.{array} must advertise maxItems:100"
        );
    }

    // Paged limits carry their maximum.
    assert_eq!(schema("list_notes")["properties"]["limit"]["maximum"], json!(1000));
    assert_eq!(schema("search_notes")["properties"]["limit"]["maximum"], json!(100));

    // search `mode` defaults to "both".
    assert_eq!(schema("search_notes")["properties"]["mode"]["default"], json!("both"));

    // edit_sections description names the `move` operation.
    let desc = tools
        .iter()
        .find(|t| t.name.as_ref() == "edit_sections")
        .unwrap()
        .description
        .as_deref()
        .unwrap_or_default();
    assert!(desc.contains("move"), "edit_sections description: {desc}");

    client.cancel().await.ok();
    server_task.abort();
}
