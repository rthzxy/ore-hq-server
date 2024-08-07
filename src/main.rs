use std::{collections::{HashMap, HashSet}, net::SocketAddr, ops::{ControlFlow}, path::Path, sync::Arc, time::Duration};

use axum::{extract::{ws::{Message, WebSocket}, ConnectInfo, State, WebSocketUpgrade}, response::IntoResponse, routing::get, Extension, Router};
use drillx::{Solution};
use futures::{stream::SplitSink, SinkExt, StreamExt};
use ore_api::state::Proof;
use ore_utils::{get_auth_ix, get_cutoff, get_mine_ix, get_proof, get_register_ix, ORE_TOKEN_DECIMALS};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::{commitment_config::CommitmentConfig, compute_budget::ComputeBudgetInstruction, native_token::LAMPORTS_PER_SOL, signature::read_keypair_file, signer::Signer, transaction::Transaction};
use tokio::sync::{mpsc::{UnboundedReceiver, UnboundedSender}, Mutex};
use tower_http::trace::{DefaultMakeSpan, TraceLayer};
use tracing::{error, info};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

struct AppState {
    sockets: HashMap<SocketAddr, SplitSink<WebSocket, Message>>
}

#[derive(Debug)]
pub enum ClientMessage {
    Ready(SocketAddr),
    Mining(SocketAddr),
    BestSolution(SocketAddr, Solution)
}

pub struct BestHash {
    solution: Option<Solution>,
    difficulty: u32,
}

mod ore_utils;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    dotenv::dotenv().ok();
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "ore_hq_server=debug,tower_http=debug".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    // load envs
    let wallet_path_str = std::env::var("WALLET_PATH").expect("WALLET_PATH must be set.");
    let rpc_url = std::env::var("RPC_URL").expect("RPC_URL must be set.");

    // load wallet
    let wallet_path = Path::new(&wallet_path_str);

    if !wallet_path.exists() {
        tracing::error!("Failed to load wallet at: {}", wallet_path_str);
        return Err("Failed to find wallet path.".into());
    }

    let wallet = read_keypair_file(wallet_path).expect("Failed to load keypair from file: {wallet_path_str}");
    println!("loaded wallet {}", wallet.pubkey().to_string());

    println!("establishing rpc connection...");
    let rpc_client = RpcClient::new_with_commitment(rpc_url, CommitmentConfig::confirmed());

    println!("loading sol balance...");
    let balance = if let Ok(balance) = rpc_client.get_balance(&wallet.pubkey()).await {
        balance
    } else {
        return Err("Failed to load balance".into());
    };

    println!("Balance: {:.2}", balance as f64 / LAMPORTS_PER_SOL as f64);

    if balance < 1_000_000 {
        return Err("Sol balance is too low!".into());
    }

    let proof = if let Ok(loaded_proof) = get_proof(&rpc_client, wallet.pubkey()).await {
        loaded_proof
    } else {
        println!("Failed to load proof.");
        println!("Creating proof account...");

        let ix = get_register_ix(wallet.pubkey());

        if let Ok((hash, _slot)) = rpc_client
            .get_latest_blockhash_with_commitment(rpc_client.commitment()).await {
            let mut tx = Transaction::new_with_payer(&[ix], Some(&wallet.pubkey()));

            tx.sign(&[&wallet], hash);

            let result = rpc_client
                .send_and_confirm_transaction_with_spinner_and_commitment(
                    &tx, rpc_client.commitment()
                ).await;

            if let Ok(sig) = result {
                println!("Sig: {}", sig.to_string());
            } else {
                return Err("Failed to create proof account".into());
            }
        }
        let proof = if let Ok(loaded_proof) = get_proof(&rpc_client, wallet.pubkey()).await {
            loaded_proof
        } else {
            return Err("Failed to get newly created proof".into());
        };
        proof
    };

    let best_hash = Arc::new(Mutex::new(BestHash {
        solution: None,
        difficulty: 0
    }));

    let wallet_extension = Arc::new(wallet);
    let proof_ext = Arc::new(Mutex::new(proof));
    let nonce_ext = Arc::new(Mutex::new(0u64));

    let shared_state = Arc::new(Mutex::new(AppState {
        sockets: HashMap::new(),
    }));
    let ready_clients = Arc::new(Mutex::new(HashSet::new()));

    let (client_message_sender, client_message_receiver) = tokio::sync::mpsc::unbounded_channel::<ClientMessage>();

    let app_shared_state = shared_state.clone();
    let app_ready_clients = ready_clients.clone();
    let app_proof = proof_ext.clone();
    let app_best_hash = best_hash.clone();
    tokio::spawn(async move {
        client_message_handler_system(client_message_receiver, &app_shared_state, app_ready_clients, app_proof, app_best_hash).await;
    });

    // Handle ready clients
    let app_shared_state = shared_state.clone();
    let app_proof = proof_ext.clone();
    let app_best_hash = best_hash.clone();
    let app_nonce = nonce_ext.clone();
    tokio::spawn(async move {
        loop {

            let mut clients = Vec::new();
            {
                let ready_clients_lock = ready_clients.lock().await;
                for ready_client in ready_clients_lock.iter() {
                    clients.push(ready_client.clone());
                }
            };

            let proof = {
                app_proof.lock().await.clone()
            };

            let cutoff = get_cutoff(proof, 5);
            let mut should_mine = true;
            let cutoff = if cutoff <= 0 {
                let solution = {
                    app_best_hash.lock().await.solution
                };
                if solution.is_some() {
                    should_mine = false;
                }
                0
            } else {
                cutoff
            };

            if should_mine {
                let challenge = proof.challenge;

                for client in clients {
                    let nonce_range = {
                        let mut nonce = app_nonce.lock().await;
                        let start = *nonce;
                        // max hashes possible in 60s for a single client
                        *nonce += 2_000_000;
                        let end = *nonce;
                        start..end
                    };
                    {
                        let mut shared_state = app_shared_state.lock().await;
                        // message type is 8 bytes = 1 u8
                        // challenge is 256 bytes = 32 u8
                        // cutoff is 64 bytes = 8 u8
                        // nonce_range is 128 bytes, start is 64 bytes, end is 64 bytes = 16 u8
                        let mut bin_data = [0; 57];
                        bin_data[00..1].copy_from_slice(&0u8.to_le_bytes());
                        bin_data[01..33].copy_from_slice(&challenge);
                        bin_data[33..41].copy_from_slice(&cutoff.to_le_bytes());
                        bin_data[41..49].copy_from_slice(&nonce_range.start.to_le_bytes());
                        bin_data[49..57].copy_from_slice(&nonce_range.end.to_le_bytes());


                        if let Some(sender) = shared_state.sockets.get_mut(&client) {
                            let _ = sender.send(Message::Binary(bin_data.to_vec())).await;
                            {
                                // waiting for a lock inside a lock feels bad...
                                let mut ready_clients_lock = ready_clients.lock().await;
                                ready_clients_lock.remove(&client);
                            }
                        }
                    }
                }
            }

            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    });

    let rpc_client = Arc::new(rpc_client);
    let app_proof = proof_ext.clone();
    let app_best_hash = best_hash.clone();
    let app_wallet = wallet_extension.clone();
    let app_nonce = nonce_ext.clone();
    tokio::spawn(async move {
        loop {
            let proof = {
                app_proof.lock().await.clone()
            };

            let cutoff = get_cutoff(proof, 0);
            if cutoff <= 0 {
                // process solutions
                let solution = {
                    app_best_hash.lock().await.solution.clone()
                };
                if let Some(solution) = solution {
                    let signer = app_wallet.clone();
                    let mut ixs = vec![];
                    // TODO: set cu's
                    let cu_limit_ix = ComputeBudgetInstruction::set_compute_unit_limit(480_000);
                    ixs.push(cu_limit_ix);

                    let prio_fee_ix = ComputeBudgetInstruction::set_compute_unit_price(100_000);
                    ixs.push(prio_fee_ix);

                    let noop_ix = get_auth_ix(signer.pubkey());
                    ixs.push(noop_ix);

                    // TODO: choose a bus
                    let bus = 4;

                    let ix_mine = get_mine_ix(signer.pubkey(), solution, bus);
                    ixs.push(ix_mine);
                    info!("Starting mine submission attempts.");
                    if let Ok((hash, _slot)) = rpc_client.get_latest_blockhash_with_commitment(rpc_client.commitment()).await {
                        let mut tx = Transaction::new_with_payer(&ixs, Some(&signer.pubkey()));

                        tx.sign(&[&signer], hash);
                        
                        for i in 0..3 {
                            info!("Sending signed tx...");
                            info!("attempt: {}", i + 1);
                            let sig = rpc_client.send_and_confirm_transaction(&tx).await;
                            if let Ok(sig) = sig {
                                // success
                                info!("Success!!");
                                info!("Sig: {}", sig);
                                // update proof
                                loop {
                                    if let Ok(loaded_proof) = get_proof(&rpc_client, signer.pubkey()).await {
                                        if proof != loaded_proof {
                                            info!("Got new proof.");
                                            let balance = (loaded_proof.balance as f64) / 10f64.powf(ORE_TOKEN_DECIMALS as f64);
                                            info!("New balance: {}", balance);
                                            let rewards = loaded_proof.balance - proof.balance;
                                            let rewards = (rewards as f64) / 10f64.powf(ORE_TOKEN_DECIMALS as f64);
                                            info!("Earned: {} ORE", rewards);
                                            {
                                                let mut mut_proof = app_proof.lock().await;
                                                *mut_proof = loaded_proof;
                                                break;
                                            }

                                        }
                                    } else {
                                        tokio::time::sleep(Duration::from_millis(500)).await;
                                    }
                                }
                                // reset nonce
                                {
                                    let mut nonce = app_nonce.lock().await;
                                    *nonce = 0;
                                }
                                // reset best hash
                                {
                                    info!("reset best hash");
                                    let mut mut_best_hash = app_best_hash.lock().await;
                                    mut_best_hash.solution = None;
                                    mut_best_hash.difficulty = 0;
                                }
                                break;
                            } else {
                                // sent error
                                if i >= 2 {
                                    info!("Failed to send after 3 attempts. Discarding and refreshing data.");
                                    // reset nonce
                                    {
                                        let mut nonce = app_nonce.lock().await;
                                        *nonce = 0;
                                    }
                                    // reset best hash
                                    {
                                        info!("reset best hash");
                                        let mut mut_best_hash = app_best_hash.lock().await;
                                        mut_best_hash.solution = None;
                                        mut_best_hash.difficulty = 0;
                                    }
                                    break;
                                }
                            }
                            tokio::time::sleep(Duration::from_millis(500)).await;
                        }
                    } else {
                        error!("Failed to get latest blockhash. retrying...");
                        tokio::time::sleep(Duration::from_millis(1000)).await;
                    }
                }
            } else {
                tokio::time::sleep(Duration::from_secs(cutoff as u64)).await;
            };
        }
    });

    let client_channel = client_message_sender.clone();
    let app_shared_state = shared_state.clone();
    let app = Router::new()
        .route("/", get(ws_handler))
        .with_state(app_shared_state)
        .layer(Extension(wallet_extension))
        .layer(Extension(client_channel))
        // Logging
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(DefaultMakeSpan::default().include_headers(true))
        );


    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000")
        .await
        .unwrap();

    tracing::debug!("listening on {}", listener.local_addr().unwrap());

    let app_shared_state = shared_state.clone();
    tokio::spawn(async move {
        ping_check_system(&app_shared_state).await;
    });
    
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>()
    ).await
    .unwrap();

    Ok(())
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    State(app_state): State<Arc<Mutex<AppState>>>,
    Extension(client_channel): Extension<UnboundedSender<ClientMessage>>
) -> impl IntoResponse {

    println!("Client: {addr} connected.");


    ws.on_upgrade(move |socket| handle_socket(socket, addr, app_state, client_channel))
}

async fn handle_socket(mut socket: WebSocket, who: SocketAddr, app_state: Arc<Mutex<AppState>>, client_channel: UnboundedSender<ClientMessage>) {
    if socket.send(axum::extract::ws::Message::Ping(vec![1, 2, 3])).await.is_ok() {
        println!("Pinged {who}...");
    } else {
        println!("could not ping {who}");

        // if we can't ping we can't do anything, return to close the connection
        return;
    }

    let (sender, mut receiver) = socket.split();
    {
        let mut app_state = app_state.lock().await;
        if app_state.sockets.contains_key(&who) {
            println!("Socket addr: {who} already has an active connection");
        } else {
            app_state.sockets.insert(who, sender);
        }
    }

    let _ = tokio::spawn(async move {
        while let Some(Ok(msg)) = receiver.next().await {
            if process_message(msg, who, client_channel.clone()).is_break() {
                break;
            }
        }
    }).await;

    println!("Client: {who} disconnected!");
}

fn process_message(msg: Message, who: SocketAddr, client_channel: UnboundedSender<ClientMessage>) -> ControlFlow<(), ()> {
    match msg {
        Message::Text(t) => {
            println!(">>> {who} sent str: {t:?}");
        },
        Message::Binary(d) => {
            // first 8 bytes are message type
            let message_type = d[0];
            match message_type {
                0 => {
                    let msg = ClientMessage::Ready(who);
                    let _ = client_channel.send(msg);
                },
                1 => {
                    let msg = ClientMessage::Mining(who);
                    let _ = client_channel.send(msg);
                },
                2 => {
                    // parse solution from message data
                    let mut solution_bytes = [0u8; 16];
                    // extract (16 u8's) from data for hash digest
                    let mut b_index = 1;
                    for i in 0..16 {
                        solution_bytes[i] = d[i + b_index];
                    }
                    b_index += 16;

                    // extract 64 bytes (8 u8's)
                    let mut nonce = [0u8; 8];
                    for i in 0..8 {
                        nonce[i] = d[i + b_index];
                    }

                    let solution = Solution::new(solution_bytes, nonce);

                    let msg = ClientMessage::BestSolution(who, solution);
                    let _ = client_channel.send(msg);
                },
                _ => {
                    println!(">>> {} sent an invalid message", who);
                }
            }

        },
        Message::Close(c) => {
            if let Some(cf) = c {
                println!(
                    ">>> {} sent close with code {} and reason `{}`",
                    who, cf.code, cf.reason
                );
            } else {
                println!(">>> {who} somehow sent close message without CloseFrame");
            }
            return ControlFlow::Break(())
        },
        Message::Pong(v) => {
            //println!(">>> {who} sent pong with {v:?}");
        },
        Message::Ping(v) => {
            //println!(">>> {who} sent ping with {v:?}");
        },
    }

    ControlFlow::Continue(())
}

async fn client_message_handler_system(
    mut receiver_channel: UnboundedReceiver<ClientMessage>,
    shared_state: &Arc<Mutex<AppState>>,
    ready_clients: Arc<Mutex<HashSet<SocketAddr>>>,
    proof: Arc<Mutex<Proof>>,
    best_hash: Arc<Mutex<BestHash>>
) {
    while let Some(client_message) = receiver_channel.recv().await {
        match client_message {
            ClientMessage::Ready(addr) => {
                println!("Client {} is ready!", addr.to_string());
                {
                    let mut locked = shared_state.lock().await;
                    if let Some(sender) = locked.sockets.get_mut(&addr) {
                        {
                            let mut ready_clients = ready_clients.lock().await;
                            ready_clients.insert(addr);
                        }

                        if let Ok(_) = sender.send(Message::Text(String::from("Client successfully added."))).await {
                        } else {
                            println!("Failed to send start mining message!");
                        }
                    }
                }
            },
            ClientMessage::Mining(addr) => {
                println!("Client {} has started mining!", addr.to_string());
            },
            ClientMessage::BestSolution(addr, solution) => {
                println!("Client {} found a solution.", addr);
                let challenge = {
                    let proof = proof.lock().await;
                    proof.challenge
                };

                if solution.is_valid(&challenge) {
                    let diff = solution.to_hash().difficulty();
                    println!("{} found diff: {}", addr, diff);
                    if diff > 3 {
                        {
                            let mut best_hash = best_hash.lock().await;
                            if diff > best_hash.difficulty {
                                best_hash.difficulty = diff;
                                best_hash.solution = Some(solution);
                            }
                        }
                    } else {
                        println!("Diff to low, skipping");
                    }
                } else {
                    println!("{} returned an invalid solution!", addr);
                }
            }
        }
    }
}

async fn ping_check_system(
    shared_state: &Arc<Mutex<AppState>>,
) {
    loop {
        // send ping to all sockets
        {
            let mut failed_sockets = Vec::new();
            let mut app_state = shared_state.lock().await;
            // I don't like doing all this work while holding this lock...
            for (who, socket) in app_state.sockets.iter_mut() {
                if socket.send(Message::Ping(vec![1, 2, 3])).await.is_ok() {
                    //println!("Pinged: {who}...");
                } else {
                    failed_sockets.push(who.clone());
                }
            }

            // remove any sockets where ping failed
            for address in failed_sockets {
                 app_state.sockets.remove(&address);
            }
        }

        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}