use std::{future::Future, net::SocketAddr, path::PathBuf, pin::Pin};

use anyhow::{anyhow, bail};
use http_body_util::BodyExt;
use hyper::{server::conn::http1, service::Service};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;
use tracing::{debug, error, info};
use tracing_subscriber::fmt::format::FmtSpan;
use wasmtime::{component::Component, component::Linker, Config, Engine, Store, StoreLimits};
use wasmtime_wasi::preview2::{self, command, Table, WasiCtx, WasiCtxBuilder, WasiView};
pub use wasmtime_wasi_http::types::{HostIncomingRequest, HostResponseOutparam};
use wasmtime_wasi_http::{
    body::HyperOutgoingBody, hyper_response_error, WasiHttpCtx, WasiHttpView,
};

// Generates WhackHandler
wasmtime::component::bindgen!({
    path: "wit",
    async: true,
    with: {
        "wasi:io/poll": preview2::bindings::io::poll,
        "wasi:io/error": preview2::bindings::io::error,
        "wasi:io/streams": preview2::bindings::io::streams,

        "wasi:clocks/monotonic-clock": preview2::bindings::clocks::monotonic_clock,
        "wasi:http/types": wasmtime_wasi_http::bindings::http::types,
    },
});

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    tracing_subscriber::fmt()
        .with_target(false)
        .with_max_level(tracing::Level::INFO)
        .with_span_events(FmtSpan::CLOSE)
        .init();

    let addr = SocketAddr::from(([127, 0, 0, 1], 3000));

    let listener = TcpListener::bind(addr).await?;

    let mut config = Config::default();
    config.wasm_component_model(true);
    config.async_support(true);

    let engine = Engine::new(&config)?;
    let server = Server {
        root: PathBuf::from("../components"),
        engine,
    };

    loop {
        let (stream, _) = listener.accept().await?;

        // Use an adapter to access something implementing `tokio::io` traits as if they implement
        // `hyper::rt` IO traits.
        let io = TokioIo::new(stream);

        // Spawn a tokio task to serve multiple connections concurrently
        tokio::task::spawn({
            let server = server.clone();

            async move {
                // Finally, we bind the incoming connection to our `hello` service
                if let Err(err) = http1::Builder::new()
                    // `service_fn` converts our function in a `Service`
                    .serve_connection(io, server)
                    .await
                {
                    println!("Error serving connection: {:?}", err);
                }
            }
        });
    }
}

#[derive(Clone)]
struct Server {
    root: PathBuf,
    engine: Engine,
}

struct Host {
    resource_table: Table,
    context: WasiCtx,
    http_context: WasiHttpCtx,
    store_limits: StoreLimits, // todo
}

impl WasiView for Host {
    fn table(&self) -> &Table {
        &self.resource_table
    }

    fn table_mut(&mut self) -> &mut Table {
        &mut self.resource_table
    }

    fn ctx(&self) -> &WasiCtx {
        &self.context
    }

    fn ctx_mut(&mut self) -> &mut WasiCtx {
        &mut self.context
    }
}

impl WasiHttpView for Host {
    fn ctx(&mut self) -> &mut WasiHttpCtx {
        &mut self.http_context
    }

    fn table(&mut self) -> &mut Table {
        &mut self.resource_table
    }
}

impl Server {
    fn store(&self, engine: &Engine) -> anyhow::Result<Store<Host>> {
        let mut context_builder = WasiCtxBuilder::new();

        let host = Host {
            resource_table: Table::new(),
            context: context_builder.build(),
            http_context: WasiHttpCtx,
            store_limits: StoreLimits::default(), // TODO revisit limits
        };

        let store = Store::new(engine, host);

        Ok(store)
    }
}

type Request = hyper::Request<hyper::body::Incoming>;
type Response = hyper::Response<HyperOutgoingBody>;

impl Service<Request> for Server {
    type Response = Response;
    type Error = anyhow::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    // #[tracing::instrument(skip(self, req))]
    fn call(&self, req: Request) -> Self::Future {
        info!(
            "Handling request: {} with root: {:?}",
            req.uri().path(),
            self.root
        );

        // Create a channel to receive the response on
        let (sender, receiver) = tokio::sync::oneshot::channel();

        // Start processing the request
        tokio::task::spawn({
            let this = self.clone();

            async move {
                let component_path = req
                    .uri()
                    .path()
                    .strip_prefix('/')
                    .ok_or(anyhow::anyhow!("Strange URI: {:?}", req.uri()))?;
                let component_file = this.root.join(component_path).with_extension("wasm");

                info!("Loading component from file: {:?}", component_file);
                let component =
                    Component::from_file(&this.engine, component_file).map_err(|e| {
                        error!("Loading component faild: {:?}", e);

                        e
                    })?;

                info!("Component loaded. Linking...");

                let linker = Linker::new(&this.engine);
                //command::add_to_linker(&mut linker)?; // Do I want this?

                info!("Creating a store...");
                let mut store = this.store(&this.engine)?;

                let (parts, body) = req.into_parts();
                let req =
                    hyper::Request::from_parts(parts, body.map_err(hyper_response_error).boxed());

                info!("Creating request and response resources...");
                // Create the resources for incoming handler arguments
                let request_handle = store.data_mut().new_incoming_request(req)?;
                let response_handle = store.data_mut().new_response_outparam(sender)?;

                // Create a handler instance
                info!("Instantiating component");
                let (handler, _) = WhackHandler::instantiate_async(&mut store, &component, &linker)
                    .await
                    .map_err(|e| {
                        error!("Instantiating component failed: {:?}", e);

                        e
                    })?;

                // Call it
                info!("Calling component");
                if let Err(e) = handler
                    .wasi_http_incoming_handler()
                    .call_handle(store, request_handle, response_handle)
                    .await
                {
                    error!("Error calling component! {}", e);
                    return Err(e);
                }

                Ok(())
            }
        });

        Box::pin(async move {
            match receiver.await {
                Ok(Ok(resp)) => Ok(resp),
                Ok(Err(e)) => bail!("component execution failed: {}", e.to_string()),
                Err(_) => bail!("guest never invoked `response-outparam::set` method"),
            }
        })
    }
}
