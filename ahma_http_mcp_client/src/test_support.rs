//! Helpers shared by this crate's unit tests.

use ahma_common::local_socket::LocalListener;

/// Serve every connection on `listener` with `app`, the way the bridge does.
pub(crate) fn serve_on_local_socket(listener: LocalListener, app: axum::Router) {
    tokio::spawn(async move {
        while let Ok(stream) = listener.accept().await {
            let app = app.clone();
            tokio::spawn(async move {
                let svc = hyper::service::service_fn(
                    move |req: hyper::Request<hyper::body::Incoming>| {
                        let mut app = app.clone();
                        async move {
                            use tower_service::Service;
                            app.call(req.map(axum::body::Body::new)).await
                        }
                    },
                );
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(hyper_util::rt::TokioIo::new(stream), svc)
                    .await;
            });
        }
    });
}
