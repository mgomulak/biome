use anyhow::bail;
use anyhow::Context;
use anyhow::Error;
use anyhow::Result;
use futures::Sink;
use futures::SinkExt;
use futures::Stream;
use futures::StreamExt;
use rome_lsp::config::AnalysisWorkspaceSettings;
use rome_lsp::config::WorkspaceSettings;
use rome_lsp::server::build_server;
use rome_lsp::server::LSPServer;
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::{from_value, to_value};
use std::any::type_name;
use std::slice;
use std::time::Duration;
use tower::timeout::Timeout;
use tower::{Service, ServiceExt};
use tower_lsp::jsonrpc;
use tower_lsp::jsonrpc::Response;
use tower_lsp::lsp_types::ClientCapabilities;
use tower_lsp::lsp_types::CodeActionContext;
use tower_lsp::lsp_types::CodeActionParams;
use tower_lsp::lsp_types::CodeActionResponse;
use tower_lsp::lsp_types::DidCloseTextDocumentParams;
use tower_lsp::lsp_types::DidOpenTextDocumentParams;
use tower_lsp::lsp_types::InitializeResult;
use tower_lsp::lsp_types::InitializedParams;
use tower_lsp::lsp_types::PartialResultParams;
use tower_lsp::lsp_types::Position;
use tower_lsp::lsp_types::Range;
use tower_lsp::lsp_types::TextDocumentIdentifier;
use tower_lsp::lsp_types::TextDocumentItem;
use tower_lsp::lsp_types::Url;
use tower_lsp::lsp_types::WorkDoneProgressParams;
use tower_lsp::LspService;
use tower_lsp::{jsonrpc::Request, lsp_types::InitializeParams};

struct Server {
    service: Timeout<LspService<LSPServer>>,
}

impl Server {
    fn new(service: LspService<LSPServer>) -> Self {
        Self {
            service: Timeout::new(service, Duration::from_secs(1)),
        }
    }

    async fn notify<P>(&mut self, method: &'static str, params: P) -> Result<()>
    where
        P: Serialize,
    {
        self.service
            .ready()
            .await
            .map_err(Error::msg)
            .context("ready() returned an error")?
            .call(
                Request::build(method)
                    .params(to_value(&params).context("failed to serialize params")?)
                    .finish(),
            )
            .await
            .map_err(Error::msg)
            .context("call() returned an error")
            .and_then(|res| {
                if let Some(res) = res {
                    bail!("shutdown returned {:?}", res)
                } else {
                    Ok(())
                }
            })
    }

    async fn request<P, R>(
        &mut self,
        method: &'static str,
        id: &'static str,
        params: P,
    ) -> Result<Option<R>>
    where
        P: Serialize,
        R: DeserializeOwned,
    {
        self.service
            .ready()
            .await
            .map_err(Error::msg)
            .context("ready() returned an error")?
            .call(
                Request::build(method)
                    .id(id)
                    .params(to_value(&params).context("failed to serialize params")?)
                    .finish(),
            )
            .await
            .map_err(Error::msg)
            .context("call() returned an error")?
            .map(|res| {
                let (_, body) = res.into_parts();

                let body =
                    body.with_context(|| format!("response to {method:?} contained an error"))?;

                from_value(body.clone()).with_context(|| {
                    format!(
                        "failed to deserialize type {} from response {body:?}",
                        type_name::<R>()
                    )
                })
            })
            .transpose()
    }

    /// Basic implementation of the `initialize` request for tests
    // The `root_path` field is deprecated, but we still need to specify it
    #[allow(deprecated)]
    async fn initialize(&mut self) -> Result<()> {
        let _res: InitializeResult = self
            .request(
                "initialize",
                "_init",
                InitializeParams {
                    process_id: None,
                    root_path: None,
                    root_uri: None,
                    initialization_options: None,
                    capabilities: ClientCapabilities::default(),
                    trace: None,
                    workspace_folders: None,
                    client_info: None,
                    locale: None,
                },
            )
            .await?
            .context("initialize returned None")?;

        Ok(())
    }

    /// Basic implementation of the `initialized` notification for tests
    async fn initialized(&mut self) -> Result<()> {
        self.notify("initialized", InitializedParams {}).await
    }

    /// Basic implementation of the `shutdown` notification for tests
    async fn shutdown(&mut self) -> Result<()> {
        self.service
            .ready()
            .await
            .map_err(Error::msg)
            .context("ready() returned an error")?
            .call(Request::build("shutdown").finish())
            .await
            .map_err(Error::msg)
            .context("call() returned an error")
            .and_then(|res| {
                if let Some(res) = res {
                    bail!("shutdown returned {:?}", res)
                } else {
                    Ok(())
                }
            })
    }

    async fn open_document(&mut self) -> Result<()> {
        self.notify(
            "textDocument/didOpen",
            DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: Url::parse("test://workspace/document.js")?,
                    language_id: String::from("javascript"),
                    version: 0,
                    text: String::from("for(;a == b;);"),
                },
            },
        )
        .await
    }

    async fn close_document(&mut self) -> Result<()> {
        self.notify(
            "textDocument/didClose",
            DidCloseTextDocumentParams {
                text_document: TextDocumentIdentifier {
                    uri: Url::parse("test://workspace/document.js")?,
                },
            },
        )
        .await
    }
}

/// Basic handler for requests and notifications coming from the server for tests
async fn client_handler<I, O>(mut stream: I, mut sink: O) -> Result<()>
where
    // This function has to be generic as `RequestStream` and `ResponseSink`
    // are not exported from `tower_lsp` and cannot be named in the signature
    I: Stream<Item = Request> + Unpin,
    O: Sink<Response> + Unpin,
{
    while let Some(req) = stream.next().await {
        let id = match req.id() {
            Some(id) => id,
            None => continue,
        };

        let res = match req.method() {
            "workspace/configuration" => {
                let settings = WorkspaceSettings {
                    analysis: AnalysisWorkspaceSettings {
                        enable_diagnostics: true,
                        enable_code_actions: true,
                    },
                    ..WorkspaceSettings::default()
                };

                let result =
                    to_value(slice::from_ref(&settings)).context("failed to serialize settings")?;

                Response::from_ok(id.clone(), result)
            }
            _ => Response::from_error(id.clone(), jsonrpc::Error::method_not_found()),
        };

        sink.send(res).await.ok();
    }

    Ok(())
}

#[tokio::test]
async fn basic_lifecycle() -> Result<()> {
    let (service, client) = build_server();
    let (stream, sink) = client.split();
    let mut server = Server::new(service);

    let reader = tokio::spawn(client_handler(stream, sink));

    server.initialize().await?;
    server.initialized().await?;

    server.shutdown().await?;
    reader.abort();

    Ok(())
}

#[tokio::test]
async fn document_lifecycle() -> Result<()> {
    let (service, client) = build_server();
    let (stream, sink) = client.split();
    let mut server = Server::new(service);

    let reader = tokio::spawn(client_handler(stream, sink));

    server.initialize().await?;
    server.initialized().await?;

    server.open_document().await?;
    server.close_document().await?;

    server.shutdown().await?;
    reader.abort();

    Ok(())
}

#[tokio::test]
async fn pull_code_actions() -> Result<()> {
    let (service, client) = build_server();
    let (stream, sink) = client.split();
    let mut server = Server::new(service);

    let reader = tokio::spawn(client_handler(stream, sink));

    server.initialize().await?;
    server.initialized().await?;

    server.open_document().await?;

    let res: CodeActionResponse = server
        .request(
            "textDocument/codeAction",
            "pull_code_actions",
            CodeActionParams {
                text_document: TextDocumentIdentifier {
                    uri: Url::parse("test://workspace/document.js")?,
                },
                range: Range {
                    start: Position {
                        line: 0,
                        character: 8,
                    },
                    end: Position {
                        line: 0,
                        character: 8,
                    },
                },
                context: CodeActionContext {
                    diagnostics: vec![],
                    only: None,
                },
                work_done_progress_params: WorkDoneProgressParams {
                    work_done_token: None,
                },
                partial_result_params: PartialResultParams {
                    partial_result_token: None,
                },
            },
        )
        .await?
        .context("codeAction returned None")?;

    assert!(!res.is_empty());

    server.close_document().await?;

    server.shutdown().await?;
    reader.abort();

    Ok(())
}