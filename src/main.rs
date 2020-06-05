//! Cargo redoc reloads your docs in a browser when they change
use futures_util::future;
use http::response::Builder as ResponseBuilder;
use http::{header, StatusCode};
use hyper::service::{make_service_fn, service_fn};
use hyper::{Body, Request, Response};
use hyper_staticfile::Static;
use log::{debug, error, info};
use lol_html::errors::RewritingError;
use lol_html::html_content::ContentType;
use lol_html::{element, rewrite_str, RewriteStrSettings};
use notify::DebouncedEvent::*;
use notify::RecursiveMode::*;
use notify::Watcher;
use std::io::Error as IoError;
use std::path::Path;
use std::process::Command;
use std::sync::mpsc::channel;
use std::thread::sleep;
use std::time::Duration;
use std::time::Instant;
use tokio::sync::broadcast;
use structopt::StructOpt;

#[derive(StructOpt)]
struct Opts {
    #[structopt(long, short)]
    package: Option<String>
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
        html if html.ends_with(".html") || html.ends_with("/") => {
            info!("injecting script into {}", html);
            let mut response = static_.clone().serve(req).await?;
            use hyper::body::Buf;
            let injected = inject_script(std::str::from_utf8(
                hyper::body::aggregate(response.body_mut()).await?.bytes(),
            )?).expect("failed to rewrite body");
            response.headers_mut().insert("Content-Length", injected.len().to_string().parse()?);
            let rewritten = Body::from(injected);
            
            *response.body_mut() = rewritten;
            
            Ok(response)
        }
        _ => Ok(static_.clone().serve(req).await?),
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    pretty_env_logger::init();
    let Opts { package } = Opts::from_args();
    let start = Instant::now();
    let doc = Command::new("cargo").args(&["doc", "--no-deps"]).spawn()?;
    let (ctx, crx) = broadcast::channel::<String>(100);
    let sse = ctx.clone();
    let static_ = Static::new(Path::new("target/doc/"));
    let make_service = make_service_fn(move |_| {
        let static_ = static_.clone();
        let sse = sse.clone();
        future::ok::<_, hyper::Error>(service_fn(move |req| {
            handle_request(req, static_.clone(), sse.subscribe())
        }))
    });

    let addr = ([127, 0, 0, 1], 3000).into();
    let server = hyper::Server::bind(&addr).serve(make_service);

    let _ = doc.wait_with_output()?;

    let (tx, rx) = channel();

    let mut watcher = match notify::watcher(tx, Duration::from_secs(1)) {
        Ok(w) => w,
        Err(e) => {
            error!("Error while trying to watch the files:\n\n\t{:?}", e);
            std::process::exit(1)
        }
    };

    // Add the source directory to the watcher
    if let Err(e) = watcher.watch("src", Recursive) {
        error!("Error while watching {:?}:\n    {:?}", "src", e);
        std::process::exit(1);
    };

    if let Err(e) = watcher.watch("target/doc", Recursive) {
        error!("Error while watching {:?}:\n    {:?}", "target/docs", e);
        std::process::exit(1);
    };

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
        let (docs, srcs) = paths
            .iter()
            .partition::<Vec<_>, _>(|path| path.to_string_lossy().contains("target/doc"));
        if !srcs.is_empty() {
            info!("src changed {:#?}", paths);
            let doc_start = Instant::now();
            let _ = Command::new("cargo").args(&["doc", "--no-deps"]).output();
            debug!("finished rustdoc in {:?}", Instant::now() - doc_start);
        } else if !docs.is_empty() {
            info!("docs changed {:#?}", paths);
            if let Err(e) = ctx.send(format!(
                "data:{}\n\n",
                docs.into_iter()
                    .map(|path| path.to_string_lossy().to_string())
                    .collect::<Vec<_>>()
                    .join(",")
            )) {
                info!("failed to notify docs changed {:?}", e);
            };
        }
    });

    info!("Doc server running on http://{}/", addr);
    debug!("started in {:?}", Instant::now() - start);
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
