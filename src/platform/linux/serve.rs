use std::{
    convert::Infallible,
    io::{self, BufWriter, Write},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use http_body_util::{BodyExt, Full, Limited};
use hyper::{
    Method, Request, Response, StatusCode,
    body::{Body, Bytes, Incoming},
    header,
    server::conn::http1,
    service::service_fn,
};
use hyper_util::rt::TokioIo;
use wholesym::{SymbolManager, SymbolManagerConfig};

use crate::cli::ServeArgs;
use crate::platform::linux::serve_gallery::{self, Source};

const MAX_REQUEST_BYTES: u64 = 8 * 1024 * 1024;
const MAX_PROFILE_BYTES: u64 = 512 * 1024 * 1024;
const MAX_RESPONSE_BYTES: usize = 32 * 1024 * 1024;
const MAX_CONNECTIONS: usize = 32;
const MAX_GALLERY_JOBS: usize = 1;
const SYMBOL_QUERY_TIMEOUT: Duration = Duration::from_secs(30);
const CONNECTION_TIMEOUT: Duration = Duration::from_secs(5 * 60);

pub fn run(args: ServeArgs) -> Result<()> {
    if let Some(profile) = args.profile.as_ref() {
        if !profile.is_file() {
            bail!(
                "profile {} does not exist or is not a file",
                profile.display()
            );
        }
        let profile_size = profile.metadata()?.len();
        if profile_size > MAX_PROFILE_BYTES {
            bail!(
                "profile {} is {} bytes, exceeding the {} byte serve limit",
                profile.display(),
                profile_size,
                MAX_PROFILE_BYTES
            );
        }
    }
    if let Some(directory) = args.directory.as_ref()
        && !directory.is_dir()
    {
        bail!(
            "directory {} does not exist or is not a directory",
            directory.display()
        );
    }
    if !args.listen.ip().is_loopback() && args.bearer_token.as_deref().is_none_or(str::is_empty) {
        bail!("--bearer-token is required when --listen is not loopback");
    }
    tokio::runtime::Runtime::new()
        .context("failed to start serve runtime")?
        .block_on(run_async(args))
}

async fn run_async(args: ServeArgs) -> Result<()> {
    let cache = tempfile::tempdir().context("failed to create symbol cache")?;
    let mut config = SymbolManagerConfig::new()
        .use_debuginfod(false)
        .debuginfod_cache_dir_if_not_installed(cache.path().join("debuginfod"));
    for directory in args.symbols.symbol_dirs {
        config = config.extra_symbol_directory(directory);
    }
    if let Some(url) = args.symbols.debuginfod {
        config = config.extra_debuginfod_server(url, cache.path().join("explicit-debuginfod"));
    }
    let symbols = Arc::new(SymbolManager::with_config(config));
    let source = Arc::new(match (&args.profile, &args.directory) {
        (Some(profile), None) => Source::Profile(profile.clone()),
        (None, Some(directory)) => Source::Directory(directory.clone()),
        _ => bail!("exactly one of --profile or --directory is required"),
    });
    let legacy_profile = if let Some(profile) = args.profile.as_ref() {
        Some((
            profile.clone(),
            profile
                .extension()
                .is_some_and(|extension| extension == "gz"),
            profile.to_string_lossy().ends_with(".jslb.gz"),
        ))
    } else {
        None
    };
    let source_name = args
        .profile
        .as_ref()
        .or(args.directory.as_ref())
        .expect("validated source")
        .display()
        .to_string();
    let token = Arc::new(args.bearer_token);
    let cors_origin = Arc::new(
        args.cors_origin
            .as_deref()
            .map(header::HeaderValue::from_str)
            .transpose()
            .context("--cors-origin is not a valid HTTP header value")?,
    );
    let connections = Arc::new(tokio::sync::Semaphore::new(MAX_CONNECTIONS));
    let gallery_jobs = Arc::new(tokio::sync::Semaphore::new(MAX_GALLERY_JOBS));
    let listener = tokio::net::TcpListener::bind(args.listen)
        .await
        .with_context(|| format!("failed to listen on {}", args.listen))?;
    let address = listener.local_addr()?;
    println!("serving {source_name} on http://{address}");

    loop {
        let (stream, _) = listener.accept().await?;
        let symbols = Arc::clone(&symbols);
        let Ok(permit) = Arc::clone(&connections).try_acquire_owned() else {
            continue;
        };
        let source = Arc::clone(&source);
        let legacy_profile = legacy_profile.clone();
        let token = Arc::clone(&token);
        let cors_origin = Arc::clone(&cors_origin);
        let gallery_jobs = Arc::clone(&gallery_jobs);
        tokio::spawn(async move {
            let _permit = permit;
            let service = service_fn(move |request| {
                handle(
                    request,
                    Arc::clone(&symbols),
                    Arc::clone(&source),
                    legacy_profile.clone(),
                    Arc::clone(&token),
                    Arc::clone(&cors_origin),
                    Arc::clone(&gallery_jobs),
                )
            });
            match tokio::time::timeout(
                CONNECTION_TIMEOUT,
                http1::Builder::new().serve_connection(TokioIo::new(stream), service),
            )
            .await
            {
                Ok(Ok(())) => {}
                Ok(Err(error)) => eprintln!("serve connection failed: {error}"),
                Err(_) => eprintln!("serve connection reached its time limit"),
            }
        });
    }
}

async fn handle(
    request: Request<Incoming>,
    symbols: Arc<SymbolManager>,
    source: Arc<Source>,
    legacy_profile: Option<(std::path::PathBuf, bool, bool)>,
    token: Arc<Option<String>>,
    cors_origin: Arc<Option<header::HeaderValue>>,
    gallery_jobs: Arc<tokio::sync::Semaphore>,
) -> std::result::Result<Response<Full<Bytes>>, Infallible> {
    let mut response = Response::new(Full::new(Bytes::new()));
    response.headers_mut().insert(
        header::X_CONTENT_TYPE_OPTIONS,
        header::HeaderValue::from_static("nosniff"),
    );
    response.headers_mut().insert(
        header::HeaderName::from_static("referrer-policy"),
        header::HeaderValue::from_static("no-referrer"),
    );
    response.headers_mut().insert(
        header::HeaderName::from_static("content-security-policy"),
        header::HeaderValue::from_static(
            "default-src 'self'; connect-src 'self'; img-src 'none'; object-src 'none'; frame-ancestors 'none'; style-src 'unsafe-inline'; script-src 'unsafe-inline'",
        ),
    );
    if let Some(origin) = cors_origin.as_ref() {
        response
            .headers_mut()
            .insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin.clone());
    }
    if request.method() == Method::OPTIONS {
        if cors_origin.is_some() {
            *response.status_mut() = StatusCode::NO_CONTENT;
            response.headers_mut().insert(
                header::ACCESS_CONTROL_ALLOW_METHODS,
                header::HeaderValue::from_static("GET, POST, OPTIONS"),
            );
            response.headers_mut().insert(
                header::ACCESS_CONTROL_ALLOW_HEADERS,
                header::HeaderValue::from_static("authorization, content-type"),
            );
        } else {
            *response.status_mut() = StatusCode::METHOD_NOT_ALLOWED;
        }
        return Ok(response);
    }
    if !authorized(&request, token.as_deref()) {
        *response.status_mut() = StatusCode::UNAUTHORIZED;
        return Ok(response);
    }
    let path = request.uri().path().to_owned();
    match (request.method(), path.as_str()) {
        (&Method::GET, "/") => {
            *response.body_mut() =
                Full::new(Bytes::from_static(serve_gallery::VIEWER_HTML.as_bytes()));
            response.headers_mut().insert(
                header::CONTENT_TYPE,
                header::HeaderValue::from_static("text/html; charset=utf-8"),
            );
            response.headers_mut().insert(
                header::CACHE_CONTROL,
                header::HeaderValue::from_static("no-store"),
            );
        }
        (&Method::GET, "/healthz") => {
            *response.body_mut() = Full::new(Bytes::from_static(b"ok\n"));
            response.headers_mut().insert(
                header::CONTENT_TYPE,
                header::HeaderValue::from_static("text/plain; charset=utf-8"),
            );
        }
        (&Method::GET, "/profile.json") => {
            if let Some((path, profile_is_gzip, profile_is_jslb)) = legacy_profile {
                let Ok(permit) = Arc::clone(&gallery_jobs).try_acquire_owned() else {
                    set_busy(&mut response);
                    return Ok(response);
                };
                match tokio::task::spawn_blocking(move || {
                    let _permit = permit;
                    std::fs::read(path)
                })
                .await
                {
                    Ok(Ok(profile)) => {
                        *response.body_mut() = Full::new(Bytes::from(profile));
                        response.headers_mut().insert(
                            header::CONTENT_TYPE,
                            header::HeaderValue::from_static(if profile_is_jslb {
                                "application/octet-stream"
                            } else {
                                "application/json"
                            }),
                        );
                        if profile_is_gzip {
                            response.headers_mut().insert(
                                header::CONTENT_ENCODING,
                                header::HeaderValue::from_static("gzip"),
                            );
                        }
                    }
                    Ok(Err(error)) => {
                        set_error(&mut response, StatusCode::INTERNAL_SERVER_ERROR, &error)
                    }
                    Err(error) => {
                        set_error(&mut response, StatusCode::INTERNAL_SERVER_ERROR, &error)
                    }
                }
            } else {
                *response.status_mut() = StatusCode::NOT_FOUND;
            }
        }
        (&Method::GET, "/api/profiles") => {
            let Ok(permit) = Arc::clone(&gallery_jobs).try_acquire_owned() else {
                set_busy(&mut response);
                return Ok(response);
            };
            let source = Arc::clone(&source);
            match tokio::task::spawn_blocking(move || {
                let _permit = permit;
                serve_gallery::entries(&source)
            })
            .await
            {
                Ok(Ok(entries)) => set_json(&mut response, &entries),
                Ok(Err(error)) => {
                    set_error(&mut response, StatusCode::INTERNAL_SERVER_ERROR, &error)
                }
                Err(error) => set_error(&mut response, StatusCode::INTERNAL_SERVER_ERROR, &error),
            }
        }
        (&Method::GET, path) if path.starts_with("/api/profile/") => {
            let id = &path["/api/profile/".len()..];
            let Ok(permit) = Arc::clone(&gallery_jobs).try_acquire_owned() else {
                set_busy(&mut response);
                return Ok(response);
            };
            let source = Arc::clone(&source);
            let id = id.to_owned();
            match tokio::task::spawn_blocking(move || {
                let _permit = permit;
                serve_gallery::find(&source, &id)?
                    .map(serve_gallery::decode)
                    .transpose()
            })
            .await
            {
                Ok(Ok(Some(profile))) => set_json(&mut response, &profile),
                Ok(Ok(None)) => *response.status_mut() = StatusCode::NOT_FOUND,
                Ok(Err(error)) => {
                    set_error(&mut response, StatusCode::UNPROCESSABLE_ENTITY, &error)
                }
                Err(error) => set_error(&mut response, StatusCode::INTERNAL_SERVER_ERROR, &error),
            }
        }
        (&Method::POST, path) => {
            if request
                .body()
                .size_hint()
                .upper()
                .is_some_and(|size| size > MAX_REQUEST_BYTES)
            {
                *response.status_mut() = StatusCode::PAYLOAD_TOO_LARGE;
                return Ok(response);
            }
            let path = path.to_owned();
            match Limited::new(request.into_body(), MAX_REQUEST_BYTES as usize)
                .collect()
                .await
            {
                Ok(body) => {
                    let body = body.to_bytes();
                    if body.len() as u64 > MAX_REQUEST_BYTES {
                        *response.status_mut() = StatusCode::PAYLOAD_TOO_LARGE;
                    } else if let Ok(body) = std::str::from_utf8(&body) {
                        match tokio::time::timeout(
                            SYMBOL_QUERY_TIMEOUT,
                            symbols.query_json_api(&path, body),
                        )
                        .await
                        {
                            Ok(result) => {
                                let mut output = BoundedBuffer::new(MAX_RESPONSE_BYTES);
                                let serialized = {
                                    let mut writer = BufWriter::new(&mut output);
                                    serde_json::to_writer(&mut writer, &result).and_then(|()| {
                                        writer.flush().map_err(serde_json::Error::io)
                                    })
                                };
                                if serialized.is_ok() {
                                    *response.body_mut() = Full::new(Bytes::from(output.bytes));
                                    response.headers_mut().insert(
                                        header::CONTENT_TYPE,
                                        header::HeaderValue::from_static("application/json"),
                                    );
                                } else {
                                    *response.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
                                }
                            }
                            Err(_) => *response.status_mut() = StatusCode::GATEWAY_TIMEOUT,
                        }
                    } else {
                        *response.status_mut() = StatusCode::BAD_REQUEST;
                    }
                }
                Err(_) => *response.status_mut() = StatusCode::PAYLOAD_TOO_LARGE,
            }
        }
        _ => *response.status_mut() = StatusCode::NOT_FOUND,
    }
    Ok(response)
}

fn set_json<T: serde::Serialize>(response: &mut Response<Full<Bytes>>, value: &T) {
    let mut output = BoundedBuffer::new(MAX_RESPONSE_BYTES);
    let result = serde_json::to_writer(&mut output, value);
    if result.is_ok() {
        *response.body_mut() = Full::new(Bytes::from(output.bytes));
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            header::HeaderValue::from_static("application/json"),
        );
        response.headers_mut().insert(
            header::CACHE_CONTROL,
            header::HeaderValue::from_static("no-store"),
        );
    } else {
        *response.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
    }
}

fn set_error(
    response: &mut Response<Full<Bytes>>,
    status: StatusCode,
    error: &dyn std::fmt::Display,
) {
    *response.status_mut() = status;
    *response.body_mut() = Full::new(Bytes::from(format!("{error:#}\n")));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("text/plain; charset=utf-8"),
    );
}

fn set_busy(response: &mut Response<Full<Bytes>>) {
    *response.status_mut() = StatusCode::SERVICE_UNAVAILABLE;
    *response.body_mut() = Full::new(Bytes::from_static(b"profile decoder is busy\n"));
    response
        .headers_mut()
        .insert(header::RETRY_AFTER, header::HeaderValue::from_static("1"));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("text/plain; charset=utf-8"),
    );
}

struct BoundedBuffer {
    bytes: Vec<u8>,
    limit: usize,
}

impl BoundedBuffer {
    fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::new(),
            limit,
        }
    }
}

impl Write for BoundedBuffer {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if self.bytes.len().saturating_add(buffer.len()) > self.limit {
            return Err(io::Error::other("symbol response exceeds configured limit"));
        }
        self.bytes.extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn authorized(request: &Request<Incoming>, token: Option<&str>) -> bool {
    let Some(token) = token else {
        return true;
    };
    request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value == format!("Bearer {token}"))
}
