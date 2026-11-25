//! Netplay inputs as datagrams: WebTransport (HTTP/3 over QUIC, on UDP)
//! between each browser and the server, next to the room's WebSocket.
//!
//! Over the WebSocket (TCP), a lost packet holds back everything after it
//! until it is sent again: tens to hundreds of milliseconds, a stall in a
//! game run in step. Datagrams are not sent again, nor held back: each
//! member sends one a frame carrying its last few inputs, so that one lost
//! is covered by the next. The server forwards each to the room's other
//! members that have a session (`netplay::Rooms::forward_datagram`). The
//! WebSocket still carries everything, inputs included: datagrams only
//! bring them sooner, and a browser without WebTransport, or a network
//! that blocks UDP, plays as before.
//!
//! The endpoint needs a certificate browsers accept for the name they
//! reach Texel at (as the HTTPS one: e.g. cert-manager's), read from PEM
//! files and read again when they change. A member opens its session with
//! the token its room's `welcome` gave it; an administrators' setting
//! (`settings`) turns the whole on.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use anyhow::Context;
use wtransport::endpoint::{IncomingSession, SessionRequest};
use wtransport::{Endpoint, Identity, ServerConfig};

use crate::netplay::Rooms;
use crate::state::AppState;

/// The path sessions open (`?token=` follows).
pub const PATH: &str = "/netplay/datagrams";
/// How often the certificate's files are checked for a new one.
const RELOAD_EVERY: Duration = Duration::from_secs(600);

/// Where to listen, with what certificate, and the port browsers reach it
/// at (which a load balancer may map).
pub struct Setup {
    pub listen: SocketAddr,
    pub cert: PathBuf,
    pub key: PathBuf,
    pub public_port: u16,
}

async fn server_config(setup: &Setup) -> anyhow::Result<ServerConfig> {
    let identity = Identity::load_pemfiles(&setup.cert, &setup.key)
        .await
        .with_context(|| format!("reading {} and {}", setup.cert.display(), setup.key.display()))?;
    Ok(ServerConfig::builder()
        .with_bind_address(setup.listen)
        .with_identity(identity)
        .keep_alive_interval(Some(Duration::from_secs(5)))
        .max_idle_timeout(Some(Duration::from_secs(30)))
        .context("the idle timeout")?
        .build())
}

fn modified(paths: [&Path; 2]) -> Option<[SystemTime; 2]> {
    let m = |p: &Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
    Some([m(paths[0])?, m(paths[1])?])
}

/// Starts the endpoint (an error when it cannot: the certificate, the
/// port), then serves sessions in the background.
pub async fn start(state: Arc<AppState>, setup: Setup) -> anyhow::Result<()> {
    let endpoint =
        Endpoint::server(server_config(&setup).await?).with_context(|| format!("listening on UDP {}", setup.listen))?;
    let endpoint = Arc::new(endpoint);
    state.datagram_port.set(setup.public_port).ok();
    tracing::info!(listen = %setup.listen, port = setup.public_port, "serving WebTransport");

    // A renewed certificate, for new sessions (those open keep theirs).
    let reloading = endpoint.clone();
    tokio::spawn(async move {
        let mut seen = modified([&setup.cert, &setup.key]);
        let mut every = tokio::time::interval(RELOAD_EVERY);
        every.tick().await;
        loop {
            every.tick().await;
            let now = modified([&setup.cert, &setup.key]);
            if now.is_none() || now == seen {
                continue;
            }
            match server_config(&setup).await {
                Ok(config) => match reloading.reload_config(config, false) {
                    Ok(()) => {
                        seen = now;
                        tracing::info!("WebTransport certificate reloaded");
                    }
                    Err(e) => tracing::warn!(error = %e, "reloading the WebTransport certificate failed"),
                },
                // Half written, maybe: next time.
                Err(e) => tracing::warn!(error = %e, "reading the WebTransport certificate failed"),
            }
        }
    });

    tokio::spawn(async move {
        loop {
            let incoming = endpoint.accept().await;
            tokio::spawn(session(state.clone(), incoming));
        }
    });
    Ok(())
}

/// The token in a session's path, if it is ours.
fn token(path: &str) -> Option<&str> {
    let query = path.strip_prefix(PATH)?.strip_prefix('?')?;
    query.split('&').find_map(|kv| kv.strip_prefix("token="))
}

/// A member's session: forwards its datagrams to its room until either
/// goes away.
async fn session(state: Arc<AppState>, incoming: IncomingSession) {
    let Ok(request) = incoming.await else { return };
    let on = state.settings.read().await.netplay_datagrams;
    serve(&state.rooms, on, request).await;
}

/// Serves a session request (`on`: datagrams are turned on).
async fn serve(rooms: &Rooms, on: bool, request: SessionRequest) {
    let Some(token) = token(request.path()).map(str::to_owned) else {
        request.not_found().await;
        return;
    };
    let Some((code, id)) = rooms.member_by_token(&token).filter(|_| on) else {
        request.forbidden().await;
        return;
    };
    let Ok(connection) = request.accept().await else { return };
    if !rooms.attach_datagrams(&code, id, connection.clone()) {
        return;
    }
    tracing::info!(%code, id, "netplay member's datagrams open");
    while let Ok(datagram) = connection.receive_datagram().await {
        if !rooms.forward_datagram(&code, id, &datagram.payload()) {
            break;
        }
    }
    rooms.detach_datagrams(&code, id, connection.stable_id());
    tracing::info!(%code, id, "netplay member's datagrams closed");
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use wtransport::{ClientConfig, Endpoint, Identity, ServerConfig};

    use crate::netplay::Rooms;

    /// Two members' sessions: a datagram from one reaches the other,
    /// stamped with its sender; a token that is no member's is refused.
    #[tokio::test]
    async fn datagrams_go_to_the_other_members() {
        let rooms = Arc::new(Rooms::default());
        let tokens = rooms.test_room("ABCDEF", 3);
        let identity = Identity::self_signed(["localhost"]).expect("a certificate");
        let config = ServerConfig::builder()
            .with_bind_address("127.0.0.1:0".parse().expect("an address"))
            .with_identity(identity)
            .build();
        let server = Endpoint::server(config).expect("an endpoint");
        let port = server.local_addr().expect("its address").port();
        let serving = rooms.clone();
        tokio::spawn(async move {
            loop {
                let incoming = server.accept().await;
                let rooms = serving.clone();
                tokio::spawn(async move {
                    if let Ok(request) = incoming.await {
                        super::serve(&rooms, true, request).await;
                    }
                });
            }
        });
        let connect = |token: String| async move {
            let client = Endpoint::client(
                ClientConfig::builder()
                    .with_bind_default()
                    .with_no_cert_validation()
                    .build(),
            )
            .expect("a client");
            client
                .connect(format!("https://127.0.0.1:{port}{}?token={token}", super::PATH))
                .await
        };
        let a = connect(tokens[0].clone()).await.expect("member 0's session");
        let b = connect(tokens[1].clone()).await.expect("member 1's session");
        assert!(connect("nobody".into()).await.is_err());
        // The server attaches each session as it accepts it: until both are,
        // datagrams go nowhere.
        let got = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                a.send_datagram([255, 9, 4, 1, 2, 3]).expect("sent");
                if let Ok(Ok(d)) = tokio::time::timeout(Duration::from_millis(100), b.receive_datagram()).await {
                    break d.payload();
                }
            }
        })
        .await
        .expect("a datagram");
        assert_eq!(&got[..], &[255, 0, 4, 1, 2, 3]);
        // Not to one's self, nor addressed to one member.
        b.send_datagram([0, 9, 4]).expect("sent");
        assert!(
            tokio::time::timeout(Duration::from_millis(300), a.receive_datagram())
                .await
                .is_err()
        );
    }

    #[test]
    fn tokens_in_paths() {
        assert_eq!(super::token("/netplay/datagrams?token=ab12"), Some("ab12"));
        assert_eq!(super::token("/netplay/datagrams?x=1&token=ab12"), Some("ab12"));
        assert_eq!(super::token("/netplay/datagrams"), None);
        assert_eq!(super::token("/elsewhere?token=ab12"), None);
    }
}
