//! The sshx server, which coordinates terminal sharing.
//!
//! Requests are communicated to the server via gRPC (for command-line sharing
//! clients) and WebSocket connections (for web listeners). The server is built
//! using a hybrid Hyper service, split between a Tonic gRPC handler and an Axum
//! web listener.
//!
//! Most web requests are routed directly to static files located in the `dist/`
//! folder relative to where this binary is running, allowing the frontend to be
//! separately developed from the server.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::{error::Error as StdError, future::Future, net::SocketAddr};

use anyhow::{anyhow, Result};
use axum::{body::HttpBody, http::uri::Scheme};
use grpc::GrpcServer;
use hyper::{
    header::{CONTENT_TYPE, HOST},
    server::{conn::AddrIncoming, Builder, Server},
    service::make_service_fn,
    Body, Request,
};
use sshx_core::proto::{sshx_service_server::SshxServiceServer, FILE_DESCRIPTOR_SET};
use tonic::transport::Server as TonicServer;
use tower::{service_fn, steer::Steer, ServiceBuilder, ServiceExt};
use tower_http::{services::Redirect, trace::TraceLayer};
use tracing::info;

use crate::session::SessionStore;

pub mod grpc;
pub mod session;
pub mod web;

/// Make the combined HTTP/gRPC application server, on a given listener.
pub async fn make_server(
    builder: Builder<AddrIncoming>,
    signal: impl Future<Output = ()>,
) -> Result<()> {
    type BoxError = Box<dyn StdError + Send + Sync>;

    let store = SessionStore::default();

    let http_service = web::app(store.clone())
        .layer(TraceLayer::new_for_http())
        .map_response(|r| r.map(|b| b.map_err(BoxError::from).boxed_unsync()))
        .map_err(BoxError::from)
        .boxed_clone();

    let grpc_service = TonicServer::builder()
        .add_service(SshxServiceServer::new(GrpcServer::new(store)))
        .add_service(
            tonic_reflection::server::Builder::configure()
                .register_encoded_file_descriptor_set(FILE_DESCRIPTOR_SET)
                .build()?,
        )
        .into_service();

    let grpc_service = ServiceBuilder::new()
        .layer(TraceLayer::new_for_grpc())
        .service(grpc_service)
        .map_response(|r| r.map(|b| b.map_err(BoxError::from).boxed_unsync()))
        .boxed_clone();

    let tls_redirect_service = service_fn(|req: Request<Body>| async {
        let uri = req.uri();
        info!(method = ?req.method(), %uri, "redirecting to https");
        let mut parts = uri.clone().into_parts();
        parts.scheme = Some(Scheme::HTTPS);
        parts.authority = Some(
            req.headers()
                .get(HOST)
                .ok_or_else(|| anyhow!("tls redirect missing host"))?
                .to_str()?
                .parse()?,
        );
        Ok(Redirect::permanent(parts.try_into()?).oneshot(req).await?)
    })
    .boxed_clone();

    let svc = Steer::new(
        [http_service, grpc_service, tls_redirect_service],
        |req: &Request<Body>, _services: &[_]| {
            let headers = req.headers();
            match (headers.get("x-forwarded-proto"), headers.get(CONTENT_TYPE)) {
                // Redirect proxied HTTP to HTTPS, see here for details:
                // https://fly.io/blog/always-be-connecting-with-https/
                (Some(proto), _) if proto == "http" => 2,
                (_, Some(content)) if content == "application/grpc" => 1,
                _ => 0,
            }
        },
    );
    let make_svc = make_service_fn(move |_| {
        let svc = svc.clone();
        async { Ok::<_, std::convert::Infallible>(svc) }
    });

    builder
        .serve(make_svc)
        .with_graceful_shutdown(signal)
        .await?;

    Ok(())
}

/// Convenience function to call [`make_server`] bound to a TCP address.
pub async fn make_server_bind(addr: &SocketAddr, signal: impl Future<Output = ()>) -> Result<()> {
    make_server(Server::try_bind(addr)?, signal).await
}
