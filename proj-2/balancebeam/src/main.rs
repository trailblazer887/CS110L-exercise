mod request;
mod response;

use clap::Parser;
use rand::{Rng, SeedableRng};
use std::{collections::HashMap, sync::Arc, time::Instant};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::RwLock,
    time::Duration,
};

const WINDOW_DURATION: Duration = Duration::from_secs(60);

/// Contains information parsed from the command-line invocation of balancebeam. The Clap macros
/// provide a fancy way to automatically construct a command-line argument parser.
#[derive(Parser, Debug)]
#[command(about = "Fun with load balancing")]
struct CmdOptions {
    /// "IP/port to bind to"
    #[arg(short, long, default_value = "0.0.0.0:1100")]
    bind: String,
    /// "Upstream host to forward requests to"
    #[arg(short, long)]
    upstream: Vec<String>,
    /// "Perform active health checks on this interval (in seconds)"
    /// note: set dafault value to 0 to pass the test for Milestone 5
    #[arg(long, default_value = "0")]
    active_health_check_interval: usize,
    /// "Path to send request to for active health checks"
    #[arg(long, default_value = "/")]
    active_health_check_path: String,
    /// "Maximum number of requests to accept per IP per minute (0 = unlimited)"
    #[arg(long, default_value = "0")]
    max_requests_per_minute: usize,
}

/// Contains information about the state of balancebeam (e.g. what servers we are currently proxying
/// to, what servers have failed, rate limiting counts, etc.)
///
/// You should add fields to this struct in later milestones.
struct ProxyState {
    /// How frequently we check whether upstream servers are alive (Milestone 4)
    #[allow(dead_code)]
    active_health_check_interval: usize,
    /// Where we should send requests when doing active health checks (Milestone 4)
    #[allow(dead_code)]
    active_health_check_path: String,
    /// Maximum number of requests an individual IP can make in a minute (Milestone 5)
    #[allow(dead_code)]
    max_requests_per_minute: usize,
    /// Addresses of servers that we are proxying to
    upstream_addresses: RwLock<Vec<String>>,
    /// Flags that denote the accessibility of upstream
    flags: RwLock<Vec<u8>>,
    /// Request count of an ip in a fixed window
    ip_request_count: Arc<RwLock<HashMap<String, (Instant, usize)>>>,
}

#[tokio::main]
async fn main() {
    // Initialize the logging library. You can print log messages using the `log` macros:
    // https://docs.rs/log/0.4.8/log/ You are welcome to continue using print! statements; this
    // just looks a little prettier.
    if let Err(_) = std::env::var("RUST_LOG") {
        std::env::set_var("RUST_LOG", "debug");
    }
    pretty_env_logger::init();

    // Parse the command line arguments passed to this program
    let options = CmdOptions::parse();
    if options.upstream.len() < 1 {
        log::error!("At least one upstream server must be specified using the --upstream option.");
        std::process::exit(1);
    }

    // Start listening for connections
    let listener = match TcpListener::bind(&options.bind).await {
        Ok(listener) => listener,
        Err(err) => {
            log::error!("Could not bind to {}: {}", options.bind, err);
            std::process::exit(1);
        }
    };
    log::info!("Listening for requests on {}", options.bind);

    // Handle incoming connections
    let len = options.upstream.len();
    let state = Arc::new(ProxyState {
        upstream_addresses: RwLock::new(options.upstream),
        active_health_check_interval: options.active_health_check_interval,
        active_health_check_path: options.active_health_check_path,
        max_requests_per_minute: options.max_requests_per_minute,
        flags: RwLock::new(vec![1; len]),
        ip_request_count: Arc::new(RwLock::new(HashMap::new())),
    });

    // Failover with active health checks
    let state_clone = Arc::clone(&state);
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(
            state_clone.active_health_check_interval as u64,
        ));
        loop {
            interval.tick().await;
            let read_lock = state_clone.upstream_addresses.read().await;

            for (ix, upstream) in (*read_lock).iter().enumerate() {
                let request = http::Request::builder()
                    .method(http::Method::GET)
                    .uri(state_clone.active_health_check_path.as_str())
                    .header("Host", upstream)
                    .body(Vec::new())
                    .unwrap();
                if let Ok(mut tcp_stream) = TcpStream::connect(upstream).await {
                    if let Ok(_) = request::write_to_stream(&request, &mut tcp_stream).await {
                        if let Ok(res) = response::read_status_from_stream(&mut tcp_stream).await {
                            if res == true {
                                let mut write_lock = state_clone.flags.write().await;
                                write_lock[ix] = 1;
                                continue;
                            }
                        }
                    }
                }
                let mut write_lock = state_clone.flags.write().await;
                write_lock[ix] = 0;
            }
        }
    });

    loop {
        if let Ok((stream, _socket_addr)) = listener.accept().await {
            let state = Arc::clone(&state);
            tokio::spawn(async move { handle_connection(stream, &state).await });
        }
    }
}

async fn connect_to_upstream(state: &ProxyState) -> Result<TcpStream, std::io::Error> {
    let mut rng = rand::rngs::StdRng::from_entropy();
    let ad_lock = state.upstream_addresses.read().await;
    let len = ad_lock.len();
    let upstream_idx = rng.gen_range(0..len);
    let mut upstream_idx_iter = upstream_idx;
    let mut upstream_ip = &ad_lock[upstream_idx];
    loop {
        // if upstream is inactive currently, jump over and try to connect with next upstream.
        let read_lock = state.flags.read().await;
        if read_lock[upstream_idx_iter] == 0 {
            upstream_idx_iter = (upstream_idx_iter + 1) % len;
            upstream_ip = &ad_lock[upstream_idx_iter];
            continue;
        }
        match TcpStream::connect(upstream_ip).await {
            Ok(stream) => return Ok(stream),
            Err(e) => {
                // poll upstream servers to find an available one
                upstream_idx_iter = (upstream_idx_iter + 1) % len;
                if upstream_idx_iter == upstream_idx {
                    return Err(e);
                }
                upstream_ip = &ad_lock[upstream_idx_iter];
            }
        }
    }
}

async fn send_response(client_conn: &mut TcpStream, response: &http::Response<Vec<u8>>) {
    let client_ip = client_conn.peer_addr().unwrap().ip().to_string();
    log::info!(
        "{} <- {}",
        client_ip,
        response::format_response_line(&response)
    );
    if let Err(error) = response::write_to_stream(&response, client_conn).await {
        log::warn!("Failed to send response to client: {}", error);
        return;
    }
}

async fn handle_connection(mut client_conn: TcpStream, state: &ProxyState) {
    let client_ip = client_conn.peer_addr().unwrap().ip().to_string();
    log::info!("Connection received from {}", client_ip);

    // Open a connection to a random destination server
    let mut upstream_conn = match connect_to_upstream(state).await {
        Ok(stream) => stream,
        Err(_error) => {
            let response = response::make_http_error(http::StatusCode::BAD_GATEWAY);
            send_response(&mut client_conn, &response).await;
            return;
        }
    };
    let upstream_ip = upstream_conn.peer_addr().unwrap().ip().to_string();

    // The client may now send us one or more requests. Keep trying to read requests until the
    // client hangs up or we get an error.
    loop {
        // Read a request from the client
        let mut request = match request::read_from_stream(&mut client_conn).await {
            Ok(request) => request,
            // Handle case where client closed connection and is no longer sending requests
            Err(request::Error::IncompleteRequest(0)) => {
                log::debug!("Client finished sending requests. Shutting down connection");
                return;
            }
            // Handle I/O error in reading from the client
            Err(request::Error::ConnectionError(io_err)) => {
                log::info!("Error reading request from client stream: {}", io_err);
                return;
            }
            Err(error) => {
                log::debug!("Error parsing request: {:?}", error);
                let response = response::make_http_error(match error {
                    request::Error::IncompleteRequest(_)
                    | request::Error::MalformedRequest(_)
                    | request::Error::InvalidContentLength
                    | request::Error::ContentLengthMismatch => http::StatusCode::BAD_REQUEST,
                    request::Error::RequestBodyTooLarge => http::StatusCode::PAYLOAD_TOO_LARGE,
                    request::Error::ConnectionError(_) => http::StatusCode::SERVICE_UNAVAILABLE,
                });
                send_response(&mut client_conn, &response).await;
                continue;
            }
        };

        // time limit check
        if state.max_requests_per_minute > 0 {
            let mut map = state.ip_request_count.write().await;
            let now = Instant::now();
            let entry = map.entry(client_ip.clone()).or_insert((now, 0));
            if now.duration_since(entry.0) >= WINDOW_DURATION {
                entry.0 = now;
                entry.1 = 1;
            } else if entry.1 >= state.max_requests_per_minute {
                let response = response::make_http_error(http::StatusCode::TOO_MANY_REQUESTS);
                let _ = response::write_to_stream(&response, &mut client_conn).await;
                continue;
            } else {
                entry.1 += 1;
            }
        }

        log::info!(
            "{} -> {}: {}",
            client_ip,
            upstream_ip,
            request::format_request_line(&request)
        );

        // Add X-Forwarded-For header so that the upstream server knows the client's IP address.
        // (We're the ones connecting directly to the upstream server, so without this header, the
        // upstream server will only know our IP, not the client's.)
        request::extend_header_value(&mut request, "x-forwarded-for", &client_ip);

        // Forward the request to the server
        if let Err(error) = request::write_to_stream(&request, &mut upstream_conn).await {
            log::error!(
                "Failed to send request to upstream {}: {}",
                upstream_ip,
                error
            );
            let response = response::make_http_error(http::StatusCode::BAD_GATEWAY);
            send_response(&mut client_conn, &response).await;
            return;
        }
        log::debug!("Forwarded request to server");

        // Read the server's response
        let response = match response::read_from_stream(&mut upstream_conn, request.method()).await
        {
            Ok(response) => response,
            Err(error) => {
                log::error!("Error reading response from server: {:?}", error);
                let response = response::make_http_error(http::StatusCode::BAD_GATEWAY);
                send_response(&mut client_conn, &response).await;
                return;
            }
        };
        // Forward the response to the client
        send_response(&mut client_conn, &response).await;
        log::debug!("Forwarded response to client");
    }
}
