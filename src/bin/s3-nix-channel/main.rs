use std::{path::PathBuf, sync::Arc, time::Duration};

use anyhow::{anyhow, Context, Result};
use arc_swap::ArcSwap;
use axum::{
    extract::{Path, Request, State},
    http::{header::LINK, HeaderMap, HeaderValue, Method, StatusCode},
    middleware::{self, Next},
    response::{self, IntoResponse, Redirect},
    routing::get,
    Router,
};
use clap::Parser;
use jsonwebtoken::{Algorithm, DecodingKey, Validation};
use tokio::time::interval;
use tower_http::trace::TraceLayer;
use tracing::{debug, error, info, warn};

use s3_nix_channel::{error::RequestError, persistent::ChannelsConfig};

/// A program to serve a S3 bucket via the Nix Lockable Tarball Protocol.
#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    /// The S3 bucket to serve the content from.
    #[arg(long)]
    bucket: String,

    /// The base URL of the service.
    ///
    /// If you want to serve objects from
    /// https://foo.com/permanent/123.tar.xz, you need to specify
    /// https://foo.com here.
    #[arg(long)]
    base_url: String,

    /// The interval in seconds for updating the configuration from
    /// the config.json file in the bucket.
    #[arg(long, default_value_t = 3600)]
    config_update_seconds: u64,

    /// What IP and port to listen on. Specify as <IP>:<port>, for
    /// example: 0.0.0.0:3000
    ///
    /// If this option is not specified, we'll try to get a listening
    /// socket from systemd.
    #[arg(long)]
    listen: Option<String>,

    /// Enable authentication using JWT by specifying the public key
    /// for token verification.
    #[arg(long)]
    jwt_pem: Option<PathBuf>,
}

struct Config {
    s3_client: s3_nix_channel::persistent::Client,
    base_url: String,
    update_interval: Duration,
    channels: ArcSwap<ChannelsConfig>,
}

/// Redirect to the latest tarball of the requested channel.
async fn handle_channel(
    method: Method,
    Path(path): Path<String>,
    State(config): State<Arc<Config>>,
) -> Result<impl IntoResponse, RequestError> {
    let channel_config = {
        let channels_config = config.channels.load();

        // TODO Needlessly inefficient. But we cannot use split_filename here,
        // because channel names often include periods, such as foobar-24.05.
        // :-(
        let (_, channel_config) = channels_config
            .channels()
            .find(|(k, v)| {
                std::iter::once(*k)
                    .chain(v.aliases.iter().map(|s| s.as_str()))
                    .find(|name| {
                        let candidate = format!("{name}{}", v.file_extension);
                        path == candidate
                    })
                    .is_some()
            })
            .ok_or_else(|| RequestError::NoSuchChannel {
                file_name: path.clone(),
            })?;
        channel_config.clone()
    };

    let latest_object = &channel_config
        .latest
        // We could have a specific error here, but this only happens
        // when a channel is initially set up. It becomes visible once
        // it has content.
        .ok_or_else(|| RequestError::NoSuchChannel {
            file_name: path.clone(),
        })?;

    let mut headers = HeaderMap::new();

    // The Lockable HTTP Tarball Protocol. See:
    // https://nix.dev/manual/nix/2.25/protocols/tarball-fetcher
    headers.insert(
        LINK,
        HeaderValue::from_str(&format!(
            "<{}/permanent/{latest_object}{}>; rel=\"immutable\"",
            config.base_url, channel_config.file_extension
        ))
        .map_err(|_e| RequestError::Unknown)?,
    );

    Ok((
        headers,
        Redirect::temporary(
            &config
                .s3_client
                .sign_request(
                    method,
                    &format!("{latest_object}{}", channel_config.file_extension),
                )
                .await?,
        ),
    ))
}

#[derive(Debug, serde::Deserialize)]
struct Claims {
    // We need nothing.
}

/// Extract the HTTP Basic Authorization password.
fn extract_auth_password(headers: &HeaderMap) -> Option<String> {
    use base64::prelude::*;

    // Get the Authorization header value
    let header = headers.get("Authorization")?;
    let header_value = header.to_str().ok()?;

    let credentials = header_value.strip_prefix("Basic ")?.to_owned();
    let credentials = String::from_utf8(BASE64_STANDARD.decode(&credentials).ok()?).ok()?;

    let pw = credentials
        .split_once(':')
        .map(|(_user, password)| password.to_owned());

    pw
}

/// If a JWT public key is available, make sure that each request is authorized.
async fn auth_middleware(
    State(decoding_key): State<DecodingKey>,
    request: Request,
    next: Next,
) -> response::Response {
    let mut validation = Validation::new(Algorithm::RS256);
    validation.validate_nbf = true;

    // TODO What we validate in the claims should be configurable. For
    // now we just check whether the token is signed and valid.
    validation.validate_aud = false;
    validation.set_required_spec_claims(&["exp"]);

    match extract_auth_password(request.headers())
        .ok_or_else(|| RequestError::InvalidToken {
            reason: "Missing Authorization header".to_owned(),
        })
        .and_then(|jwt_str| {
            jsonwebtoken::decode::<Claims>(&jwt_str, &decoding_key, &validation).map_err(|e| {
                RequestError::InvalidToken {
                    reason: e.to_string(),
                }
            })
        }) {
        Ok(claim) => {
            debug!("Claim {:?}", claim)
        }
        Err(e) => {
            info!("JWT validation error: {e}");
            return StatusCode::UNAUTHORIZED.into_response();
        }
    }

    next.run(request).await
}

async fn log_request_middleware(req: Request, next: Next) -> response::Response {
    let path = req.uri().path().to_owned();
    let method = req.method().clone();

    let start = std::time::Instant::now();
    let response = next.run(req).await;
    let duration = start.elapsed();

    let status = response.status();

    info!(
        "Request: {} {} -> {} ({:?})",
        method,
        path,
        status.as_u16(),
        duration
    );

    response
}

/// Forward a request to the backing store.
async fn handle_persistent(
    method: Method,
    Path(path): Path<String>,
    State(config): State<Arc<Config>>,
) -> Result<impl IntoResponse, RequestError> {
    // TODO This is very unfortunate. We basically allow the client to grab
    // everything from the bucket here. This would include our config files as
    // well. There are no secrets in them, but it's still not great. We make a
    // half-hearted attempt at preventing it here...
    if path.ends_with(".json") {
        return Err(RequestError::NoSuchChannel { file_name: path });
    }

    Ok(Redirect::temporary(
        &config.s3_client.sign_request(method, &path).await?,
    ))
}

/// Poll the bucket for changes of the configuration.
async fn poll_config_file(state: &Config) {
    let mut interval = interval(state.update_interval);

    // The first tick completes immediately, so we skip it.
    interval.tick().await;

    loop {
        interval.tick().await;

        let new_channels = match state.s3_client.load_channels_config().await {
            Ok(channels) => channels,
            Err(e) => {
                error!("Failed to load new config (will try again later): {e}");
                continue;
            }
        };

        state.channels.store(Arc::new(new_channels));
        info!("Successfully refreshed channel state.")
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::builder().parse("info,s3-nix-channel=debug")?,
        )
        .init();

    let s3_client = s3_nix_channel::persistent::Client::new_from_env(&args.bucket).await?;

    let channels = s3_client.load_channels_config().await?;
    let jwt_public_key = args
        .jwt_pem
        .map(|pem_file| {
            std::fs::read(&pem_file).with_context(|| {
                format!("Failed to read public key PEM from {}", pem_file.display())
            })
        })
        // Be sure to handle the I/O error, so we don't accidentally
        // misinterpret "couldn't read file" as "there is no public
        // key", which would make the service accessible without
        // authentication.
        .transpose()?
        .map(|pem_data| DecodingKey::from_rsa_pem(&pem_data).context("Failed to decode public key"))
        .transpose()?;

    let config = Arc::new(Config {
        s3_client,
        base_url: args.base_url,
        update_interval: Duration::from_secs(args.config_update_seconds),
        channels: ArcSwap::new(Arc::new(channels)),
    });

    // Reload the config periodically.
    let update_state = config.clone();
    tokio::spawn(async move {
        poll_config_file(&update_state).await;
    });

    // TODO Add proper logging of requests.
    let mut app = Router::new()
        .route("/channel/{*path}", get(handle_channel))
        .route("/permanent/{*path}", get(handle_persistent))
        .with_state(config);

    if let Some(jwt_public_key) = jwt_public_key {
        let auth_layer = middleware::from_fn_with_state(jwt_public_key, auth_middleware);

        app = app.layer(auth_layer);
    }

    // Layer logging last, so we can see authentication failures as well.
    app = app
	.layer(middleware::from_fn(log_request_middleware))
        .layer(
            TraceLayer::new_for_http().make_span_with(|request: &Request| {
                tracing::debug_span!("request", method = %request.method(), uri = %request.uri())
            }),
        );

    let listener = match args.listen {
        Some(listen_str) => {
            info!("Listening on {}", &listen_str);
            tokio::net::TcpListener::bind(&listen_str).await?
        }
        None => {
            use std::os::fd::FromRawFd;
            // Accept socket from systemd.
            let socket_fd = sd_notify::listen_fds()
                .context("Failed to get listening socket from systemd")?
                .next()
                .ok_or_else(|| anyhow!("Got 0 file descriptors from systemd?"))?;
            // SAFETY: Systemd guarantees that this is an open
            // listening socket.
            let std_listener = unsafe { std::net::TcpListener::from_raw_fd(socket_fd) };

            info!("Got listening socket from systemd.");
            tokio::net::TcpListener::from_std(std_listener)?
        }
    };

    tokio::task::spawn_blocking(|| {
        use sd_notify::{notify, NotifyState};
        if let Err(e) = notify(&[NotifyState::Ready]) {
            warn!("Failed to notify systemd: {e}");
        } else {
            debug!("Notified systemd that we are ready to serve!");
        }
    })
    .await?;

    axum::serve(listener, app).await?;

    Ok(())
}
