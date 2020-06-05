//! Cargo redoc reloads your docs in a browser when they change
use futures_util::future;
use http::{header, response::Builder as ResponseBuilder, StatusCode};
use hyper::{
    service::{make_service_fn, service_fn},
    Body, Request, Response, Server,
};
use hyper_staticfile::Static;
use log::{debug, error, info};
use lol_html::{
    element, errors::RewritingError, html_content::ContentType, rewrite_str, RewriteStrSettings,
};
use notify::{DebouncedEvent::*, RecursiveMode::*, Watcher};
use std::{
    io,
    path::Path,
    process::{Command, Output},
    sync::mpsc::channel,
    thread::sleep,
    time::{Duration, Instant},
};
use structopt::StructOpt;
use tokio::sync::broadcast;

/// A hot-reloading version of `cargo doc`
#[derive(StructOpt)]
struct Opts {
    /// Package to document
    #[structopt(long, short)]
    package: Option<String>,
}

fn script() -> &'static str {
    "<script async>new EventSource('/_').onmessage = _ => location.reload();</script>"
}

fn inject_script(html: &str) -> Result<String, RewritingError> {
    rewrite_str(
        html,
        RewriteStrSettings {
            element_content_handlers: vec![element!("body", |el| {
                el.append(script(), ContentType::Html);

                Ok(())
            })],
            ..RewriteStrSettings::default()
        },
    )
}

async fn handle_request<B>(
    req: Request<B>,
    static_: Static,
    changes: broadcast::Receiver<String>,
) -> Result<Response<Body>, Box<dyn std::error::Error + 'static + Send + Sync>> {
    match req.uri().path() {
        "/" => {
            let res = ResponseBuilder::new()
                .status(StatusCode::MOVED_PERMANENTLY)
                .header(
                    header::LOCATION,
                    format!("/{}/", env!("CARGO_PKG_NAME")).replace("-", "_"),
                )
                .body(Body::empty())?;
            Ok(res)
        }
        "/_" => {
            let res = ResponseBuilder::new()
                .status(StatusCode::OK)
                .header(header::CACHE_CONTROL, "no-cache")
                .header(header::CONTENT_TYPE, "text/event-stream")
                .header("Access-Control-Allow-Origin", "*")
                .body(Body::wrap_stream(changes.into_stream()))?;
            Ok(res)
        }
        html if html.ends_with(".html") || html.ends_with('/') => {
            info!("injecting script into {}", html);
            let mut response = static_.clone().serve(req).await?;
            use hyper::body::Buf;
            let injected = inject_script(std::str::from_utf8(
                hyper::body::aggregate(response.body_mut()).await?.bytes(),
            )?)
            .expect("failed to rewrite body");
            response
                .headers_mut()
                .insert("Content-Length", injected.len().to_string().parse()?);
            let rewritten = Body::from(injected);

            *response.body_mut() = rewritten;

            Ok(response)
        }
        _ => Ok(static_.clone().serve(req).await?),
    }
}

fn rustdoc(package: &Option<String>) -> io::Result<Output> {
    let mut cmd = Command::new("cargo");
    cmd.args(&["doc", "--no-deps"]);
    if let Some(p) = package {
        cmd.args(&["-p", p.as_ref()]);
    }
    cmd.output()
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    pretty_env_logger::init();
    let Opts { package } = Opts::from_args();
    let start = Instant::now();
    rustdoc(&package)?;
    // send file change notice to handlers
    let (ctx, _) = broadcast::channel::<String>(100);
    let sse = ctx.clone();
    let static_ = Static::new(Path::new("target/doc/"));

    let addr = ([127, 0, 0, 1], 3000).into();
    let server = Server::bind(&addr).serve(make_service_fn(move |_| {
        let static_ = static_.clone();
        let sse = sse.clone();
        future::ok::<_, hyper::Error>(service_fn(move |req| {
            handle_request(req, static_.clone(), sse.subscribe())
        }))
    }));

    // watch sources
    let (tx, rx) = channel();

    let mut watcher = match notify::watcher(tx, Duration::from_secs(1)) {
        Ok(w) => w,
        Err(e) => {
            error!("Error while trying to watch the files:\n\n\t{}", e);
            std::process::exit(1)
        }
    };

    // watch sources to know when to trigger rustdoc
    if let Err(e) = watcher.watch("src", Recursive) {
        error!("Error while watching src:\n    {:?}", e);
        std::process::exit(1);
    };

    // watch reloadables to know when to reload browser
    if let Err(e) = watcher.watch("target/doc", Recursive) {
        error!("Error while watching target/docs:\n    {:?}", e);
        std::process::exit(1);
    };

    // watch out
    std::thread::spawn(move || loop {
        sleep(Duration::from_millis(1000));
        let paths = rx
            .try_iter()
            .filter_map(|event| {
                debug!("Received filesystem event: {:?}", event);
                match event {
                    Create(path) | Write(path) | Remove(path) | Rename(_, path) => Some(path),
                    _ => None,
                }
            })
            .collect::<Vec<_>>();
        let (reloadables, srcs) = paths
            .iter()
            .partition::<Vec<_>, _>(|path| path.to_string_lossy().contains("target/doc"));
        if !srcs.is_empty() {
            info!("src changed {:#?}", paths);
            let doc_start = Instant::now();
            if let Err(e) = rustdoc(&package) {
                error!("cargo doc run failed: {}", e);
            }
            debug!("finished rustdoc in {:?}", Instant::now() - doc_start);
        } else if !reloadables.is_empty() {
            info!("Reloading browsers");
            debug!("files changed {:#?}", reloadables);
            if let Err(e) = ctx.send(format!(
                "data:{}\n\n",
                reloadables
                    .into_iter()
                    .map(|path| path.to_string_lossy().to_string())
                    .collect::<Vec<_>>()
                    .join(",")
            )) {
                info!("Failed to notify browsers that docs changed {:?}", e);
            };
        }
    });

    info!("Doc server running on http://{}/", addr);
    debug!("Started in {:?}", Instant::now() - start);
    opener::open(format!("http://{}/", addr.to_string()))?;
    server.await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn script_is_injected() -> Result<(), Box<dyn std::error::Error>> {
        let html = inject_script(r#"<html><body><p>hi</p><body></html>"#)?;

        assert_eq!(
            html,
            r#"<html><body><p>hi</p><body><script async>new EventSource('/_').onmessage = _ => location.reload();</script></html>"#
        );
        Ok(())
    }
}
