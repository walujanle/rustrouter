//! `GET /api/skills/{id}/SKILL.md` — the agent-skill markdown, served by the app.
//!
//! Serving these files from an upstream GitHub raw URL would make the link
//! depend on a repository this project does not control. The markdown is
//! bundled into the binary with `include_str!` — the same embedding the crate
//! already uses for `catalog.json` and `registry.json` — and the dashboard
//! builds the URL from its own origin.
//!
//! The content is public documentation, so the route is on the guard's
//! `PUBLIC_API_PATHS` allow-list: a pasted link has to resolve for an AI agent
//! that holds no session cookie.

use axum::extract::Path;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};

use crate::error::ApiError;

/// The kept skills, in the order the dashboard lists them. Paths are relative to
/// this file, so a renamed or missing file fails the build rather than 404ing at
/// runtime — the same contract as `catalog.json`.
static SKILLS: &[(&str, &str)] = &[
    ("9router", include_str!("../skills/9router/SKILL.md")),
    (
        "9router-chat",
        include_str!("../skills/9router-chat/SKILL.md"),
    ),
    (
        "9router-embeddings",
        include_str!("../skills/9router-embeddings/SKILL.md"),
    ),
    (
        "9router-web-search",
        include_str!("../skills/9router-web-search/SKILL.md"),
    ),
    (
        "9router-web-fetch",
        include_str!("../skills/9router-web-fetch/SKILL.md"),
    ),
];

/// `GET /api/skills/{id}/SKILL.md`.
pub async fn get(Path(id): Path<String>) -> Result<Response, ApiError> {
    match SKILLS.iter().find(|(slug, _)| *slug == id) {
        Some((_, body)) => Ok((
            StatusCode::OK,
            [(header::CONTENT_TYPE, "text/markdown; charset=utf-8")],
            *body,
        )
            .into_response()),
        None => Err(ApiError::not_found(format!("Unknown skill: {id}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;

    async fn body_of(response: Response) -> String {
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    #[tokio::test]
    async fn known_skill_returns_markdown() {
        let response = get(Path("9router".to_string())).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "text/markdown; charset=utf-8"
        );
        let body = body_of(response).await;
        assert!(body.starts_with("---"), "frontmatter comes first");
        assert!(body.contains("NINEROUTER_URL"));
        // The skill docs point at this server's own port, not 20128.
        assert!(body.contains("localhost:20129"));
        assert!(!body.contains("localhost:20128"));
    }

    #[tokio::test]
    async fn every_listed_skill_resolves() {
        for (id, body) in SKILLS {
            let response = get(Path((*id).to_string())).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{id} resolves");
            assert!(!body.is_empty(), "{id} has content");
        }
    }

    #[tokio::test]
    async fn unknown_skill_is_404() {
        let error = get(Path("nope".to_string())).await.unwrap_err();
        assert_eq!(error.status, StatusCode::NOT_FOUND);
    }
}
