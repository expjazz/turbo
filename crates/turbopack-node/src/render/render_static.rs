use anyhow::{bail, Context, Result};
use turbo_tasks::primitives::StringVc;
use turbo_tasks_env::ProcessEnvVc;
use turbo_tasks_fs::{File, FileContent, FileSystemPathVc};
use turbopack_core::{
    asset::{Asset, AssetContentVc, AssetVc},
    chunk::ChunkingContextVc,
    error::PrettyPrintError,
};
use turbopack_dev_server::{
    html::DevHtmlAssetVc,
    source::{HeaderListVc, RewriteBuilder, RewriteVc},
};
use turbopack_ecmascript::{chunk::EcmascriptChunkPlaceablesVc, EcmascriptModuleAssetVc};

use super::{
    issue::RenderingIssue, RenderDataVc, RenderStaticIncomingMessage, RenderStaticOutgoingMessage,
};
use crate::{
    get_intermediate_asset, get_renderer_pool, pool::NodeJsOperation,
    render::error_page::error_html_body, source_map::trace_stack,
};

#[turbo_tasks::value]
pub enum StaticResult {
    Content {
        content: AssetContentVc,
        status_code: u16,
        headers: HeaderListVc,
    },
    Rewrite(RewriteVc),
}

#[turbo_tasks::value_impl]
impl StaticResultVc {
    #[turbo_tasks::function]
    pub fn content(content: AssetContentVc, status_code: u16, headers: HeaderListVc) -> Self {
        StaticResult::Content {
            content,
            status_code,
            headers,
        }
        .cell()
    }

    #[turbo_tasks::function]
    pub fn rewrite(rewrite: RewriteVc) -> Self {
        StaticResult::Rewrite(rewrite).cell()
    }
}

/// Renders a module as static HTML in a node.js process.
#[turbo_tasks::function]
pub async fn render_static(
    cwd: FileSystemPathVc,
    env: ProcessEnvVc,
    path: FileSystemPathVc,
    module: EcmascriptModuleAssetVc,
    runtime_entries: EcmascriptChunkPlaceablesVc,
    fallback_page: DevHtmlAssetVc,
    chunking_context: ChunkingContextVc,
    intermediate_output_path: FileSystemPathVc,
    output_root: FileSystemPathVc,
    project_dir: FileSystemPathVc,
    data: RenderDataVc,
) -> Result<StaticResultVc> {
    let intermediate_asset = get_intermediate_asset(
        module.as_evaluated_chunk(chunking_context, Some(runtime_entries)),
        intermediate_output_path,
    );
    let renderer_pool = get_renderer_pool(
        cwd,
        env,
        intermediate_asset,
        intermediate_output_path,
        output_root,
        project_dir,
        /* debug */ false,
    );
    // Read this strongly consistent, since we don't want to run inconsistent
    // node.js code.
    let pool = renderer_pool.strongly_consistent().await?;
    let mut operation = match pool.operation().await {
        Ok(operation) => operation,
        Err(err) => {
            return Ok(StaticResultVc::content(
                static_error(path, err, None, fallback_page).await?,
                500,
                HeaderListVc::empty(),
            ))
        }
    };

    Ok(
        match run_static_operation(
            &mut operation,
            data,
            intermediate_asset,
            intermediate_output_path,
            project_dir,
        )
        .await
        {
            Ok(result) => result,
            Err(err) => StaticResultVc::content(
                static_error(path, err, Some(operation), fallback_page).await?,
                500,
                HeaderListVc::empty(),
            ),
        },
    )
}

async fn run_static_operation(
    operation: &mut NodeJsOperation,
    data: RenderDataVc,
    intermediate_asset: AssetVc,
    intermediate_output_path: FileSystemPathVc,
    project_dir: FileSystemPathVc,
) -> Result<StaticResultVc> {
    let data = data.await?;

    operation
        .send(RenderStaticOutgoingMessage::Headers { data: &data })
        .await
        .context("sending headers to node.js process")?;
    Ok(
        match operation
            .recv()
            .await
            .context("receiving from node.js process")?
        {
            RenderStaticIncomingMessage::Rewrite { path } => {
                StaticResultVc::rewrite(RewriteBuilder::new(path).build())
            }
            RenderStaticIncomingMessage::Response {
                status_code,
                headers,
                body,
            } => StaticResultVc::content(
                FileContent::Content(File::from(body)).into(),
                status_code,
                HeaderListVc::cell(headers),
            ),
            RenderStaticIncomingMessage::Error(error) => {
                bail!(
                    trace_stack(
                        error,
                        intermediate_asset,
                        intermediate_output_path,
                        project_dir
                    )
                    .await?
                )
            }
        },
    )
}

async fn static_error(
    path: FileSystemPathVc,
    error: anyhow::Error,
    operation: Option<NodeJsOperation>,
    fallback_page: DevHtmlAssetVc,
) -> Result<AssetContentVc> {
    let status = match operation {
        Some(operation) => Some(operation.wait_or_kill().await?),
        None => None,
    };

    let error = format!("{}", PrettyPrintError(&error));
    let mut message = error
        // TODO this is pretty inefficient
        .replace('&', "&amp;")
        .replace('>', "&gt;")
        .replace('<', "&lt;");

    if let Some(status) = status {
        message.push_str(&format!("\n\nStatus: {}", status));
    }

    let mut body = "<script id=\"__NEXT_DATA__\" type=\"application/json\">{ \"props\": {} \
                    }</script>"
        .to_string();

    body.push_str(
        error_html_body(500, "Error rendering page".to_string(), message)
            .await?
            .as_str(),
    );

    let issue = RenderingIssue {
        context: path,
        message: StringVc::cell(error),
        status: status.and_then(|status| status.code()),
    };

    issue.cell().as_issue().emit();

    let html = fallback_page.with_body(body);

    Ok(html.content())
}
