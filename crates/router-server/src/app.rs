//! Router assembly and the server entry point.
//!
//! Route wiring only; handlers live in `routes/`. The middleware runs on
//! everything, so a new route is protected by default and has to be added to a
//! public table in `auth::guard` to opt out — a deny-by-default shape.

use std::net::SocketAddr;

use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::routing::{get, post, put};
use tower_http::catch_panic::CatchPanicLayer;
use tower_http::compression::CompressionLayer;
use tower_http::cors::CorsLayer;
use tower_http::decompression::RequestDecompressionLayer;
use tower_http::trace::TraceLayer;

use crate::middleware;
use crate::routes::{
    auth, catalog_sync, cli_tools, combos, keys, misc, models, models_test, oauth, pricing,
    provider_nodes, providers, proxy_pools, registry, settings, skills, tags, translator, usage,
    v1, version_update,
};
use crate::state::AppState;

/// Largest decompressed request body the JSON extractors accept. Well above a
/// long conversation, well below a memory DoS.
const MAX_REQUEST_BODY_BYTES: usize = 8 * 1024 * 1024;

/// The LLM API routes under one prefix. Mounted at `/v1`, `/v1/v1` and
/// `/api/v1`; both `/v1/*` and `/v1/v1/*` reach the same handlers.
fn llm_api(prefix: &str) -> Router<AppState> {
    let p = |path: &str| format!("{prefix}{path}");
    Router::new()
        .route(&p(""), get(v1::models))
        .route(&p("/models"), get(v1::models))
        .route(&p("/models/info"), get(v1::model_info))
        .route(&p("/models/{*rest}"), get(v1::model_by_id))
        .route(&p("/chat/completions"), post(v1::chat_completions))
        .route(&p("/messages"), post(v1::messages))
        .route(&p("/messages/count_tokens"), post(v1::count_tokens))
        .route(&p("/responses"), post(v1::responses))
        .route(&p("/responses/compact"), post(v1::responses_compact))
        .route(&p("/api/chat"), post(v1::ollama_chat))
        .route(&p("/embeddings"), post(v1::embeddings))
        .route(&p("/search"), post(v1::search))
        .route(&p("/web/fetch"), post(v1::web_fetch))
        .route(&p("/systemone"), post(v1::systemone))
}

/// Build the router. Separate from `serve` so tests can drive it with
/// `tower::ServiceExt::oneshot`.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route(
            "/api/health",
            get(misc::health).options(misc::health_options),
        )
        .route("/api/init", get(misc::init))
        .route("/api/version", get(misc::version))
        .route("/api/version/shutdown", post(misc::version_shutdown))
        .route("/api/settings/require-login", get(misc::require_login))
        .route("/api/shutdown", post(misc::shutdown))
        .route("/api/auth/login", post(auth::login))
        .route("/api/auth/logout", post(auth::logout))
        .route("/api/auth/status", get(auth::status))
        .route("/api/auth/reset-password", post(auth::reset_password))
        // Dashboard /api/* surface. Every one of these is deny-by-default
        // through the guard; none is on the public allow-list.
        .route("/api/settings", get(settings::get).patch(settings::patch))
        .route(
            "/api/settings/database",
            get(settings::database_export).post(settings::database_import),
        )
        .route("/api/settings/proxy-test", post(settings::proxy_test))
        .route("/api/keys", get(keys::list).post(keys::create))
        .route(
            "/api/keys/{id}",
            get(keys::get).patch(keys::update).delete(keys::delete),
        )
        .route("/api/combos", get(combos::list).post(combos::create))
        .route(
            "/api/combos/presets",
            get(combos::presets).post(combos::create_presets),
        )
        .route(
            "/api/combos/{id}",
            get(combos::get)
                // PUT is what the dashboard sends; PATCH is accepted too so
                // either client works.
                .put(combos::update)
                .patch(combos::update)
                .delete(combos::delete),
        )
        .route(
            "/api/pricing",
            get(pricing::get)
                .patch(pricing::patch)
                .delete(pricing::delete),
        )
        .route("/api/tags", get(tags::list).options(tags::options))
        .route("/api/registry", get(registry::get))
        // The agent-skill markdown, served by the app instead of GitHub raw. On
        // the guard's public allow-list so a pasted link resolves for an AI agent
        // with no session.
        .route("/api/skills/{id}/SKILL.md", get(skills::get))
        .route("/api/models", get(models::list).put(models::update_alias))
        .route(
            "/api/models/alias",
            get(models::list_aliases)
                .put(models::set_alias)
                .delete(models::delete_alias),
        )
        .route(
            "/api/models/disabled",
            get(models::disabled_get)
                .post(models::disabled_post)
                .delete(models::disabled_delete),
        )
        .route(
            "/api/models/custom",
            get(models::custom_list)
                .post(models::custom_add)
                .delete(models::custom_delete),
        )
        .route(
            "/api/models/availability",
            get(models::availability_get).post(models::availability_post),
        )
        .route("/api/models/test", post(models_test::test))
        .route(
            "/api/models/catalog-sync",
            get(catalog_sync::get).post(catalog_sync::post),
        )
        .route(
            "/api/oauth/{provider}/{action}",
            get(oauth::get).post(oauth::post),
        )
        .route(
            "/api/oauth/codex/bulk-import",
            post(oauth::codex_bulk_import),
        )
        .route(
            "/api/oauth/codex/import-token",
            post(oauth::codex_import_token),
        )
        .route(
            "/api/oauth/grok-cli/bulk-import",
            post(oauth::grok_cli_bulk_import),
        )
        .route(
            "/api/cli-tools/claude-settings",
            get(cli_tools::claude_get)
                .post(cli_tools::claude_post)
                .delete(cli_tools::claude_delete),
        )
        .route(
            "/api/cli-tools/codex-settings",
            get(cli_tools::codex_get)
                .post(cli_tools::codex_post)
                .delete(cli_tools::codex_delete),
        )
        .route(
            "/api/cli-tools/hermes-settings",
            get(cli_tools::hermes_get)
                .post(cli_tools::hermes_post)
                .delete(cli_tools::hermes_delete),
        )
        .route("/api/usage/stats", get(usage::stats))
        .route("/api/usage/chart", get(usage::chart))
        .route("/api/usage/history", get(usage::history))
        .route("/api/usage/logs", get(usage::logs))
        .route("/api/usage/providers", get(usage::providers))
        .route("/api/usage/request-logs", get(usage::request_logs))
        .route("/api/usage/stream", get(usage::stream))
        .route("/api/usage/{connectionId}", get(usage::connection_usage))
        .route(
            "/api/usage/{connectionId}/codex-reset-credits",
            get(usage::codex_reset_credits_get).post(usage::codex_reset_credits_post),
        )
        .route(
            "/api/translator/console-logs",
            get(translator::console_logs_get).delete(translator::console_logs_delete),
        )
        .route(
            "/api/translator/console-logs/stream",
            get(translator::console_logs_stream),
        )
        .route("/api/translator/load", get(translator::load))
        .route("/api/translator/save", post(translator::save))
        .route("/api/translator/send", post(translator::send))
        .route("/api/translator/translate", post(translator::translate))
        .route("/api/version/update", post(version_update::update))
        .route(
            "/api/provider-nodes",
            get(provider_nodes::list).post(provider_nodes::create),
        )
        .route(
            "/api/provider-nodes/validate",
            post(provider_nodes::validate),
        )
        .route(
            "/api/provider-nodes/{id}",
            put(provider_nodes::update).delete(provider_nodes::delete),
        )
        .route(
            "/api/providers",
            get(providers::list).post(providers::create),
        )
        .route("/api/providers/client", get(providers::client))
        .route(
            "/api/providers/suggested-models",
            get(providers::suggested_models),
        )
        .route("/api/providers/test-batch", post(providers::test_batch))
        .route("/api/providers/validate", post(providers::validate))
        .route(
            "/api/providers/{id}",
            get(providers::get)
                .put(providers::update)
                .delete(providers::delete),
        )
        .route("/api/providers/{id}/test", post(providers::test))
        .route(
            "/api/providers/{id}/test-models",
            post(providers::test_models),
        )
        .route("/api/providers/{id}/models", get(providers::models))
        .route(
            "/api/proxy-pools",
            get(proxy_pools::list).post(proxy_pools::create),
        )
        .route(
            "/api/proxy-pools/vercel-deploy",
            post(proxy_pools::vercel_deploy),
        )
        .route(
            "/api/proxy-pools/cloudflare-deploy",
            post(proxy_pools::cloudflare_deploy),
        )
        .route(
            "/api/proxy-pools/deno-deploy",
            post(proxy_pools::deno_deploy),
        )
        .route(
            "/api/proxy-pools/{id}",
            get(proxy_pools::get)
                .put(proxy_pools::update)
                .delete(proxy_pools::delete),
        )
        .route("/api/proxy-pools/{id}/test", post(proxy_pools::test))
        // The LLM API surface, mounted at three prefixes, so all three
        // spellings reach the same handlers and carry the request pathname as
        // `ChatRequest.endpoint`. The doubled prefix is not a typo: the
        // CLI-tool writer stores `ANTHROPIC_BASE_URL` ending in `/v1`, and
        // Claude Code appends `/v1/messages` to whatever it is given, so it
        // always requests `/v1/v1/messages`. Without the alias that 404s and
        // the CLI reports the model as unavailable.
        .merge(llm_api("/v1"))
        .merge(llm_api("/v1/v1"))
        .merge(llm_api("/api/v1"))
        .route("/v1beta/models", get(v1::gemini_models))
        .route("/v1beta/models/{*path}", post(v1::gemini_generate))
        .route("/codex/{*rest}", post(v1::codex))
        .route("/responses", post(v1::responses))
        .route("/systemone", post(v1::systemone))
        // Everything else is the embedded dashboard. The guard runs first, so
        // `/dashboard` and `/` still resolve to their redirects before this.
        .fallback(crate::static_assets::serve)
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            middleware::guard,
        ))
        .layer(CatchPanicLayer::new())
        // The limit applies to the decompressed stream, so it sits inside the
        // decompression layer. `DefaultBodyLimit`'s 2 MB default is too small
        // for a long conversation pasted into the dashboard.
        .layer(DefaultBodyLimit::max(MAX_REQUEST_BODY_BYTES))
        .layer(CompressionLayer::new())
        // `pass_through_unaccepted` keeps a client that sends an encoding we do
        // not know (or none at all) working unchanged instead of getting a 415.
        .layer(RequestDecompressionLayer::new().pass_through_unaccepted(true))
        .layer(TraceLayer::new_for_http())
        // Outermost: preflight OPTIONS answers before the guard runs.
        .layer(CorsLayer::permissive())
        .with_state(state)
}

/// Bind and serve until a shutdown signal.
///
/// The connect-info variant is mandatory: the guard extracts
/// `ConnectInfo<SocketAddr>`, and a router served without it answers 500 on
/// every request.
pub async fn serve(state: AppState, addr: SocketAddr) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("listening on http://{addr}");
    axum::serve(
        listener,
        router(state).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut sig) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            sig.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
    tracing::info!("shutdown signal received");
}

#[cfg(test)]
mod tests {
    use super::*;
    use router_db::{Db, Paths};

    /// A scratch data dir under the OS temp dir, unique per process and label.
    ///
    /// Tests run in parallel and SQLite refuses two writers on one file, so
    /// each test needs its own directory.
    fn scratch_paths_for(label: &str) -> Paths {
        let dir = std::env::temp_dir().join(format!(
            "rustrouter-app-test-{}-{label}",
            std::process::id()
        ));
        let paths = Paths::new(dir);
        paths.ensure_dirs().expect("create scratch data dir");
        paths
    }

    /// Building the router must not panic.
    ///
    /// `Router::route` panics when a path+method is registered twice, and that
    /// panic only fires when the router is assembled — a unit test on a handler
    /// never catches it. This test exists because a duplicated `/api/v1/*` alias
    /// block shipped a binary that aborted on startup.
    #[test]
    fn router_builds_without_duplicate_routes() {
        let paths = scratch_paths_for("builds");
        let db = Db::open(&paths.data_file, 1).expect("open scratch db");
        let state = AppState::new(db, paths).expect("build app state");
        let _router = router(state);
    }

    /// Every LLM prefix must reach a handler. The `/v1/v1` spelling is the one
    /// Claude Code actually sends: the CLI-tool writer stores a base URL ending
    /// in `/v1` and the CLI appends `/v1/messages`, so a missing alias surfaces
    /// as "the selected model may not exist" instead of a 404.
    #[tokio::test]
    async fn llm_aliases_answer_on_every_prefix() {
        use axum::body::Body;
        use axum::http::{Request, StatusCode};
        use tower::ServiceExt;

        let paths = scratch_paths_for("llm-aliases");
        let db = Db::open(&paths.data_file, 1).expect("open scratch db");
        let state = AppState::new(db, paths).expect("build app state");

        for prefix in ["/v1", "/v1/v1", "/api/v1"] {
            // `count_tokens` answers locally, so the test never touches a
            // provider. The guard reads `ConnectInfo`, which only exists once
            // the server is bound; insert it so the request reaches the route.
            let peer: std::net::SocketAddr = "127.0.0.1:5000".parse().unwrap();
            let mut request = Request::builder()
                .method("POST")
                .uri(format!("{prefix}/messages/count_tokens"))
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"messages":[{"role":"user","content":"hi"}]}"#,
                ))
                .unwrap();
            request
                .extensions_mut()
                .insert(axum::extract::ConnectInfo(peer));

            let response = router(state.clone())
                .oneshot(request)
                .await
                .expect("router answers");
            assert_eq!(
                response.status(),
                StatusCode::OK,
                "prefix {prefix} did not reach the handler"
            );
        }
    }
}
