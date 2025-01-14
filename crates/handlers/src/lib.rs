// Copyright 2024, 2025 New Vector Ltd.
// Copyright 2021-2024 The Matrix.org Foundation C.I.C.
//
// SPDX-License-Identifier: AGPL-3.0-only
// Please see LICENSE in the repository root for full details.

#![deny(clippy::future_not_send)]
#![allow(
    // Some axum handlers need that
    clippy::unused_async,
    // Because of how axum handlers work, we sometime have take many arguments
    clippy::too_many_arguments,
    // Code generated by tracing::instrument trigger this when returning an `impl Trait`
    // See https://github.com/tokio-rs/tracing/issues/2613
    clippy::let_with_type_underscore,
)]

use std::{convert::Infallible, sync::LazyLock, time::Duration};

use axum::{
    extract::{FromRef, FromRequestParts, OriginalUri, RawQuery, State},
    http::Method,
    response::{Html, IntoResponse},
    routing::{get, post},
    Extension, Router,
};
use headers::HeaderName;
use hyper::{
    header::{
        ACCEPT, ACCEPT_LANGUAGE, AUTHORIZATION, CONTENT_LANGUAGE, CONTENT_LENGTH, CONTENT_TYPE,
    },
    StatusCode, Version,
};
use mas_axum_utils::{cookies::CookieJar, FancyError};
use mas_data_model::SiteConfig;
use mas_http::CorsLayerExt;
use mas_keystore::{Encrypter, Keystore};
use mas_matrix::BoxHomeserverConnection;
use mas_policy::Policy;
use mas_router::{Route, UrlBuilder};
use mas_storage::{BoxClock, BoxRepository, BoxRng};
use mas_templates::{ErrorContext, NotFoundContext, TemplateContext, Templates};
use opentelemetry::metrics::Meter;
use sqlx::PgPool;
use tower::util::AndThenLayer;
use tower_http::cors::{Any, CorsLayer};

use self::{graphql::ExtraRouterParameters, passwords::PasswordManager};

mod admin;
mod compat;
mod graphql;
mod health;
mod oauth2;
pub mod passwords;
pub mod upstream_oauth2;
mod views;

mod activity_tracker;
mod captcha;
mod preferred_language;
mod rate_limit;
#[cfg(test)]
mod test_utils;

static METER: LazyLock<Meter> = LazyLock::new(|| {
    let scope = opentelemetry::InstrumentationScope::builder(env!("CARGO_PKG_NAME"))
        .with_version(env!("CARGO_PKG_VERSION"))
        .with_schema_url(opentelemetry_semantic_conventions::SCHEMA_URL)
        .build();

    opentelemetry::global::meter_with_scope(scope)
});

/// Implement `From<E>` for `RouteError`, for "internal server error" kind of
/// errors.
#[macro_export]
macro_rules! impl_from_error_for_route {
    ($route_error:ty : $error:ty) => {
        impl From<$error> for $route_error {
            fn from(e: $error) -> Self {
                Self::Internal(Box::new(e))
            }
        }
    };
    ($error:ty) => {
        impl_from_error_for_route!(self::RouteError: $error);
    };
}

pub use mas_axum_utils::{cookies::CookieManager, ErrorWrapper};

pub use self::{
    activity_tracker::{ActivityTracker, Bound as BoundActivityTracker},
    admin::router as admin_api_router,
    graphql::{
        schema as graphql_schema, schema_builder as graphql_schema_builder, Schema as GraphQLSchema,
    },
    preferred_language::PreferredLanguage,
    rate_limit::{Limiter, RequesterFingerprint},
    upstream_oauth2::cache::MetadataCache,
};

pub fn healthcheck_router<S>() -> Router<S>
where
    S: Clone + Send + Sync + 'static,
    PgPool: FromRef<S>,
{
    Router::new().route(mas_router::Healthcheck::route(), get(self::health::get))
}

pub fn graphql_router<S>(playground: bool, undocumented_oauth2_access: bool) -> Router<S>
where
    S: Clone + Send + Sync + 'static,
    graphql::Schema: FromRef<S>,
    BoundActivityTracker: FromRequestParts<S>,
    BoxRepository: FromRequestParts<S>,
    BoxClock: FromRequestParts<S>,
    Encrypter: FromRef<S>,
    CookieJar: FromRequestParts<S>,
    Limiter: FromRef<S>,
    RequesterFingerprint: FromRequestParts<S>,
{
    let mut router = Router::new()
        .route(
            mas_router::GraphQL::route(),
            get(self::graphql::get).post(self::graphql::post),
        )
        // Pass the undocumented_oauth2_access parameter through the request extension, as it is
        // per-listener
        .layer(Extension(ExtraRouterParameters {
            undocumented_oauth2_access,
        }))
        .layer(
            CorsLayer::new()
                .allow_origin(Any)
                .allow_methods(Any)
                .allow_otel_headers([
                    AUTHORIZATION,
                    ACCEPT,
                    ACCEPT_LANGUAGE,
                    CONTENT_LANGUAGE,
                    CONTENT_TYPE,
                ]),
        );

    if playground {
        router = router.route(
            mas_router::GraphQLPlayground::route(),
            get(self::graphql::playground),
        );
    }

    router
}

pub fn discovery_router<S>() -> Router<S>
where
    S: Clone + Send + Sync + 'static,
    Keystore: FromRef<S>,
    SiteConfig: FromRef<S>,
    UrlBuilder: FromRef<S>,
    BoxClock: FromRequestParts<S>,
    BoxRng: FromRequestParts<S>,
{
    Router::new()
        .route(
            mas_router::OidcConfiguration::route(),
            get(self::oauth2::discovery::get),
        )
        .route(
            mas_router::Webfinger::route(),
            get(self::oauth2::webfinger::get),
        )
        .layer(
            CorsLayer::new()
                .allow_origin(Any)
                .allow_methods(Any)
                .allow_otel_headers([
                    AUTHORIZATION,
                    ACCEPT,
                    ACCEPT_LANGUAGE,
                    CONTENT_LANGUAGE,
                    CONTENT_TYPE,
                ])
                .max_age(Duration::from_secs(60 * 60)),
        )
}

pub fn api_router<S>() -> Router<S>
where
    S: Clone + Send + Sync + 'static,
    Keystore: FromRef<S>,
    UrlBuilder: FromRef<S>,
    BoxRepository: FromRequestParts<S>,
    ActivityTracker: FromRequestParts<S>,
    BoundActivityTracker: FromRequestParts<S>,
    Encrypter: FromRef<S>,
    reqwest::Client: FromRef<S>,
    SiteConfig: FromRef<S>,
    BoxHomeserverConnection: FromRef<S>,
    BoxClock: FromRequestParts<S>,
    BoxRng: FromRequestParts<S>,
    Policy: FromRequestParts<S>,
{
    // All those routes are API-like, with a common CORS layer
    Router::new()
        .route(
            mas_router::OAuth2Keys::route(),
            get(self::oauth2::keys::get),
        )
        .route(
            mas_router::OidcUserinfo::route(),
            get(self::oauth2::userinfo::get).post(self::oauth2::userinfo::get),
        )
        .route(
            mas_router::OAuth2Introspection::route(),
            post(self::oauth2::introspection::post),
        )
        .route(
            mas_router::OAuth2Revocation::route(),
            post(self::oauth2::revoke::post),
        )
        .route(
            mas_router::OAuth2TokenEndpoint::route(),
            post(self::oauth2::token::post),
        )
        .route(
            mas_router::OAuth2RegistrationEndpoint::route(),
            post(self::oauth2::registration::post),
        )
        .route(
            mas_router::OAuth2DeviceAuthorizationEndpoint::route(),
            post(self::oauth2::device::authorize::post),
        )
        .layer(
            CorsLayer::new()
                .allow_origin(Any)
                .allow_methods(Any)
                .allow_otel_headers([
                    AUTHORIZATION,
                    ACCEPT,
                    ACCEPT_LANGUAGE,
                    CONTENT_LANGUAGE,
                    CONTENT_TYPE,
                ])
                .max_age(Duration::from_secs(60 * 60)),
        )
}

#[allow(clippy::trait_duplication_in_bounds)]
pub fn compat_router<S>() -> Router<S>
where
    S: Clone + Send + Sync + 'static,
    UrlBuilder: FromRef<S>,
    SiteConfig: FromRef<S>,
    BoxHomeserverConnection: FromRef<S>,
    PasswordManager: FromRef<S>,
    Limiter: FromRef<S>,
    BoundActivityTracker: FromRequestParts<S>,
    RequesterFingerprint: FromRequestParts<S>,
    BoxRepository: FromRequestParts<S>,
    BoxClock: FromRequestParts<S>,
    BoxRng: FromRequestParts<S>,
{
    Router::new()
        .route(
            mas_router::CompatLogin::route(),
            get(self::compat::login::get).post(self::compat::login::post),
        )
        .route(
            mas_router::CompatLogout::route(),
            post(self::compat::logout::post),
        )
        .route(
            mas_router::CompatRefresh::route(),
            post(self::compat::refresh::post),
        )
        .route(
            mas_router::CompatLoginSsoRedirect::route(),
            get(self::compat::login_sso_redirect::get),
        )
        .route(
            mas_router::CompatLoginSsoRedirectIdp::route(),
            get(self::compat::login_sso_redirect::get),
        )
        .route(
            mas_router::CompatLoginSsoRedirectSlash::route(),
            get(self::compat::login_sso_redirect::get),
        )
        .layer(
            CorsLayer::new()
                .allow_origin(Any)
                .allow_methods(Any)
                .allow_otel_headers([
                    AUTHORIZATION,
                    ACCEPT,
                    ACCEPT_LANGUAGE,
                    CONTENT_LANGUAGE,
                    CONTENT_TYPE,
                    HeaderName::from_static("x-requested-with"),
                ])
                .max_age(Duration::from_secs(60 * 60)),
        )
}

#[allow(clippy::too_many_lines)]
pub fn human_router<S>(templates: Templates) -> Router<S>
where
    S: Clone + Send + Sync + 'static,
    UrlBuilder: FromRef<S>,
    PreferredLanguage: FromRequestParts<S>,
    BoxRepository: FromRequestParts<S>,
    CookieJar: FromRequestParts<S>,
    BoundActivityTracker: FromRequestParts<S>,
    RequesterFingerprint: FromRequestParts<S>,
    Encrypter: FromRef<S>,
    Templates: FromRef<S>,
    Keystore: FromRef<S>,
    PasswordManager: FromRef<S>,
    MetadataCache: FromRef<S>,
    SiteConfig: FromRef<S>,
    Limiter: FromRef<S>,
    reqwest::Client: FromRef<S>,
    BoxHomeserverConnection: FromRef<S>,
    BoxClock: FromRequestParts<S>,
    BoxRng: FromRequestParts<S>,
    Policy: FromRequestParts<S>,
{
    Router::new()
        // XXX: hard-coded redirect from /account to /account/
        .route(
            "/account",
            get(
                |State(url_builder): State<UrlBuilder>, RawQuery(query): RawQuery| async move {
                    let prefix = url_builder.prefix().unwrap_or_default();
                    let route = mas_router::Account::route();
                    let destination = if let Some(query) = query {
                        format!("{prefix}{route}?{query}")
                    } else {
                        format!("{prefix}{route}")
                    };

                    axum::response::Redirect::to(&destination)
                },
            ),
        )
        .route(mas_router::Account::route(), get(self::views::app::get))
        .route(
            mas_router::AccountWildcard::route(),
            get(self::views::app::get),
        )
        .route(
            mas_router::AccountRecoveryFinish::route(),
            get(self::views::app::get_anonymous),
        )
        .route(
            mas_router::ChangePasswordDiscovery::route(),
            get(|State(url_builder): State<UrlBuilder>| async move {
                url_builder.redirect(&mas_router::AccountPasswordChange)
            }),
        )
        .route(mas_router::Index::route(), get(self::views::index::get))
        .route(
            mas_router::Login::route(),
            get(self::views::login::get).post(self::views::login::post),
        )
        .route(mas_router::Logout::route(), post(self::views::logout::post))
        .route(
            mas_router::Reauth::route(),
            get(self::views::reauth::get).post(self::views::reauth::post),
        )
        .route(
            mas_router::Register::route(),
            get(self::views::register::get),
        )
        .route(
            mas_router::PasswordRegister::route(),
            get(self::views::register::password::get).post(self::views::register::password::post),
        )
        .route(
            mas_router::RegisterVerifyEmail::route(),
            get(self::views::register::steps::verify_email::get)
                .post(self::views::register::steps::verify_email::post),
        )
        .route(
            mas_router::AccountRecoveryStart::route(),
            get(self::views::recovery::start::get).post(self::views::recovery::start::post),
        )
        .route(
            mas_router::AccountRecoveryProgress::route(),
            get(self::views::recovery::progress::get).post(self::views::recovery::progress::post),
        )
        .route(
            mas_router::OAuth2AuthorizationEndpoint::route(),
            get(self::oauth2::authorization::get),
        )
        .route(
            mas_router::ContinueAuthorizationGrant::route(),
            get(self::oauth2::authorization::complete::get),
        )
        .route(
            mas_router::Consent::route(),
            get(self::oauth2::consent::get).post(self::oauth2::consent::post),
        )
        .route(
            mas_router::CompatLoginSsoComplete::route(),
            get(self::compat::login_sso_complete::get).post(self::compat::login_sso_complete::post),
        )
        .route(
            mas_router::UpstreamOAuth2Authorize::route(),
            get(self::upstream_oauth2::authorize::get),
        )
        .route(
            mas_router::UpstreamOAuth2Callback::route(),
            get(self::upstream_oauth2::callback::handler)
                .post(self::upstream_oauth2::callback::handler),
        )
        .route(
            mas_router::UpstreamOAuth2Link::route(),
            get(self::upstream_oauth2::link::get).post(self::upstream_oauth2::link::post),
        )
        .route(
            mas_router::DeviceCodeLink::route(),
            get(self::oauth2::device::link::get),
        )
        .route(
            mas_router::DeviceCodeConsent::route(),
            get(self::oauth2::device::consent::get).post(self::oauth2::device::consent::post),
        )
        .layer(AndThenLayer::new(
            move |response: axum::response::Response| async move {
                if response.status().is_server_error() {
                    // Error responses should have an ErrorContext attached to them
                    let ext = response.extensions().get::<ErrorContext>();
                    if let Some(ctx) = ext {
                        if let Ok(res) = templates.render_error(ctx) {
                            let (mut parts, _original_body) = response.into_parts();
                            parts.headers.remove(CONTENT_TYPE);
                            parts.headers.remove(CONTENT_LENGTH);
                            return Ok((parts, Html(res)).into_response());
                        }
                    }
                }

                Ok::<_, Infallible>(response)
            },
        ))
}

/// The fallback handler for all routes that don't match anything else.
///
/// # Errors
///
/// Returns an error if the template rendering fails.
pub async fn fallback(
    State(templates): State<Templates>,
    OriginalUri(uri): OriginalUri,
    method: Method,
    version: Version,
    PreferredLanguage(locale): PreferredLanguage,
) -> Result<impl IntoResponse, FancyError> {
    let ctx = NotFoundContext::new(&method, version, &uri).with_language(locale);
    // XXX: this should look at the Accept header and return JSON if requested

    let res = templates.render_not_found(&ctx)?;

    Ok((StatusCode::NOT_FOUND, Html(res)))
}
