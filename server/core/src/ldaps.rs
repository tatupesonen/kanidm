use std::marker::Unpin;
use std::net;
use std::str::FromStr;

use crate::actors::v1_read::QueryServerReadV1;
use futures_util::sink::SinkExt;
use futures_util::stream::StreamExt;
use kanidmd_lib::idm::ldap::{LdapBoundToken, LdapResponseState};
use kanidmd_lib::prelude::*;
use ldap3_proto::proto::LdapMsg;
use ldap3_proto::LdapCodec;
use openssl::ssl::{Ssl, SslAcceptor, SslAcceptorBuilder};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpListener;
use tokio_openssl::SslStream;
use tokio_util::codec::{FramedRead, FramedWrite};

use crate::CoreAction;
use tokio::sync::broadcast;

struct LdapSession {
    uat: Option<LdapBoundToken>,
}

impl LdapSession {
    fn new() -> Self {
        LdapSession {
            // We start un-authenticated
            uat: None,
        }
    }
}

#[instrument(name = "ldap-request", skip(client_address, qe_r_ref))]
async fn client_process_msg(
    uat: Option<LdapBoundToken>,
    client_address: net::SocketAddr,
    protomsg: LdapMsg,
    qe_r_ref: &'static QueryServerReadV1,
) -> Option<LdapResponseState> {
    let eventid = sketching::tracing_forest::id();
    security_info!(
        client_ip = %client_address.ip(),
        client_port = %client_address.port(),
        "LDAP client"
    );
    qe_r_ref.handle_ldaprequest(eventid, protomsg, uat).await
}

async fn client_process<W: AsyncWrite + Unpin, R: AsyncRead + Unpin>(
    mut r: FramedRead<R, LdapCodec>,
    mut w: FramedWrite<W, LdapCodec>,
    client_address: net::SocketAddr,
    qe_r_ref: &'static QueryServerReadV1,
) {
    // This is a connected client session. we need to associate some state to the session
    let mut session = LdapSession::new();
    // Now that we have the session we begin an event loop to process input OR we return.
    while let Some(Ok(protomsg)) = r.next().await {
        // Start the event
        let uat = session.uat.clone();
        let caddr = client_address;

        match client_process_msg(uat, caddr, protomsg, qe_r_ref).await {
            // I'd really have liked to have put this near the [LdapResponseState::Bind] but due
            // to the handing of `audit` it isn't possible due to borrows, etc.
            Some(LdapResponseState::Unbind) => return,
            Some(LdapResponseState::Disconnect(rmsg)) => {
                if w.send(rmsg).await.is_err() {
                    break;
                }
                break;
            }
            Some(LdapResponseState::Bind(uat, rmsg)) => {
                session.uat = Some(uat);
                if w.send(rmsg).await.is_err() {
                    break;
                }
            }
            Some(LdapResponseState::Respond(rmsg)) => {
                if w.send(rmsg).await.is_err() {
                    break;
                }
            }
            Some(LdapResponseState::MultiPartResponse(v)) => {
                for rmsg in v.into_iter() {
                    if w.send(rmsg).await.is_err() {
                        break;
                    }
                }
            }
            Some(LdapResponseState::BindMultiPartResponse(uat, v)) => {
                session.uat = Some(uat);
                for rmsg in v.into_iter() {
                    if w.send(rmsg).await.is_err() {
                        break;
                    }
                }
            }
            None => {
                error!("Internal server error");
                break;
            }
        };
    }
}

/// TLS LDAP Listener, hands off to [client_process]
async fn tls_acceptor(
    listener: TcpListener,
    tls_parms: SslAcceptor,
    qe_r_ref: &'static QueryServerReadV1,
    mut rx: broadcast::Receiver<CoreAction>,
) {
    loop {
        tokio::select! {
            Ok(action) = rx.recv() => {
                match action {
                    CoreAction::Shutdown => break,
                }
            }
            accept_result = listener.accept() => {
                match accept_result {
                    Ok((tcpstream, client_socket_addr)) => {
                        // Start the event
                        // From the parameters we need to create an SslContext.
                        let mut tlsstream = match Ssl::new(tls_parms.context())
                            .and_then(|tls_obj| SslStream::new(tls_obj, tcpstream))
                        {
                            Ok(ta) => ta,
                            Err(e) => {
                                error!("LDAP TLS setup error, continuing -> {:?}", e);
                                continue;
                            }
                        };
                        if let Err(e) = SslStream::accept(Pin::new(&mut tlsstream)).await {
                            error!("LDAP TLS accept error, continuing -> {:?}", e);
                            continue;
                        };
                        let (r, w) = tokio::io::split(tlsstream);
                        let r = FramedRead::new(r, LdapCodec);
                        let w = FramedWrite::new(w, LdapCodec);
                        tokio::spawn(client_process(r, w, client_socket_addr, qe_r_ref));
                    }
                    Err(e) => {
                        error!("LDAP acceptor error, continuing -> {:?}", e);
                    }
                }
            }
        }
    }
    info!("Stopped LdapAcceptorActor");
}

pub(crate) async fn create_ldap_server(
    address: &str,
    opt_tls_params: Option<SslAcceptorBuilder>,
    qe_r_ref: &'static QueryServerReadV1,
    rx: broadcast::Receiver<CoreAction>,
) -> Result<tokio::task::JoinHandle<()>, ()> {
    if address.starts_with(":::") {
        // takes :::xxxx to xxxx
        let port = address.replacen(":::", "", 1);
        error!("Address '{}' looks like an attempt to wildcard bind with IPv6 on port {} - please try using ldapbindaddress = '[::]:{}'", address, port, port);
    };

    let addr = net::SocketAddr::from_str(address).map_err(|e| {
        error!("Could not parse LDAP server address {} -> {:?}", address, e);
    })?;

    let listener = TcpListener::bind(&addr).await.map_err(|e| {
        error!(
            "Could not bind to LDAP server address {} -> {:?}",
            address, e
        );
    })?;

    let ldap_acceptor_handle = match opt_tls_params {
        Some(tls_params) => {
            info!("Starting LDAPS interface ldaps://{} ...", address);
            let tls_parms = tls_params.build();
            tokio::spawn(tls_acceptor(listener, tls_parms, qe_r_ref, rx))
        }
        None => {
            error!("The server won't run without TLS!");
            return Err(());
        }
    };

    info!("Created LDAP interface");
    Ok(ldap_acceptor_handle)
}
