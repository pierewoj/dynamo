// SPDX-FileCopyrightText: Copyright (c) 2024-2025 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use clap::Parser;

use dynamo_llm::discovery::{ModelWatcher, MODEL_ROOT_PATH};
use dynamo_llm::http::service::service_v2::HttpService;
use dynamo_runtime::{
    logging, pipeline::RouterMode, transports::etcd::PrefixWatcher, DistributedRuntime, Result,
    Runtime, Worker,
};

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Host for the HTTP service
    #[arg(long, default_value = "0.0.0.0")]
    host: String,

    /// Port number for the HTTP service
    #[arg(short, long, default_value = "8080")]
    port: u16,

    /// Namespace for the distributed component
    #[arg(long, default_value = "public")]
    namespace: String,

    /// Component name for the service
    #[arg(long, default_value = "http")]
    component: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    logging::init();
    let worker = Worker::from_current()?;
    worker.execute_async(app).await
}

async fn app(runtime: Runtime) -> Result<()> {
    let distributed = DistributedRuntime::from_settings(runtime.clone()).await?;
    let args = Args::parse();

    let http_service = HttpService::builder()
        .port(args.port)
        .host(args.host)
        .build()?;
    let manager = http_service.state().manager_clone();

    // todo - use the IntoComponent trait to register the component
    // todo - start a service
    // todo - we want the service to create an entry and register component definition
    // todo - the component definition should be the type of component and it's config
    // in this example we will have an HttpServiceComponentDefinition object which will be
    // written to etcd
    // the cli when operating on an `http` component will validate the namespace.component is
    // registered with HttpServiceComponentDefinition

    let watch_obj = ModelWatcher::new(distributed.clone(), manager, RouterMode::Random);

    if let Some(etcd_client) = distributed.etcd_client() {
        let models_watcher: PrefixWatcher =
            etcd_client.kv_get_and_watch_prefix(MODEL_ROOT_PATH).await?;

        let (_prefix, _watcher, receiver) = models_watcher.dissolve();
        tokio::spawn(async move {
            watch_obj.watch(receiver).await;
        });
    }

    // Run the service
    http_service.run(runtime.child_token()).await
}
