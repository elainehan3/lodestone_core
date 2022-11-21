#![allow(clippy::comparison_chain, clippy::type_complexity)]

use crate::{
    handlers::{
        checks::get_checks_routes, client_info::get_client_info_routes, events::get_events_routes,
        global_fs::get_global_fs_routes, instance::*, instance_config::get_instance_config_routes,
        instance_fs::get_instance_fs_routes, instance_macro::get_instance_macro_routes,
        instance_manifest::get_instance_manifest_routes,
        instance_players::get_instance_players_routes, instance_server::get_instance_server_routes,
        instance_setup_configs::get_instance_setup_config_routes, monitor::get_monitor_routes,
        setup::get_setup_route, system::get_system_routes, users::get_user_routes,
    },
    prelude::{LODESTONE_PATH, PATH_TO_BINARIES, PATH_TO_STORES, PATH_TO_USERS},
    traits::Error,
    util::{download_file, rand_alphanumeric},
};
use auth::user::User;
use axum::{Extension, Router};
use events::Event;
use implementations::minecraft;
use log::{debug, error, info, warn};
use port_allocator::PortAllocator;
use reqwest::{header, Method};
use ringbuffer::{AllocRingBuffer, RingBufferWrite};
use serde_json::Value;
use stateful::Stateful;
use std::{
    collections::{HashMap, HashSet},
    net::SocketAddr,
    path::Path,
    sync::{atomic::AtomicBool, Arc},
    time::Duration,
};
use sysinfo::SystemExt;
use tokio::{
    fs::create_dir_all,
    process::Command,
    select,
    sync::{
        broadcast::{self, error::RecvError, Receiver, Sender},
        Mutex,
    },
};
use tower_http::cors::{Any, CorsLayer};
use traits::{t_configurable::TConfigurable, t_server::MonitorReport, TInstance};
use util::list_dir;
use uuid::Uuid;
mod auth;
mod events;
mod handlers;
mod implementations;
pub mod macro_executor;
mod port_allocator;
pub mod prelude;
mod stateful;
mod traits;
mod util;
mod output_types;

#[derive(Clone)]
pub struct AppState {
    instances: Arc<Mutex<HashMap<String, Arc<Mutex<dyn TInstance>>>>>,
    users: Arc<Mutex<Stateful<HashMap<String, User>>>>,
    events_buffer: Arc<Mutex<Stateful<AllocRingBuffer<Event>>>>,
    console_out_buffer: Arc<Mutex<Stateful<HashMap<String, AllocRingBuffer<Event>>>>>,
    monitor_buffer: Arc<Mutex<HashMap<String, AllocRingBuffer<MonitorReport>>>>,
    event_broadcaster: Sender<Event>,
    is_setup: Arc<AtomicBool>,
    uuid: String,
    client_name: Arc<Mutex<String>>,
    up_since: i64,
    system: Arc<Mutex<sysinfo::System>>,
    port_allocator: Arc<Mutex<PortAllocator>>,
    first_time_setup_key: Arc<Mutex<Option<String>>>,
}

async fn restore_instances(
    lodestone_path: &Path,
    event_broadcaster: &Sender<Event>,
) -> HashMap<String, Arc<Mutex<dyn TInstance>>> {
    let mut ret: HashMap<String, Arc<Mutex<dyn TInstance>>> = HashMap::new();

    for instance_future in list_dir(&lodestone_path.join("instances"), Some(true))
        .await
        .unwrap()
        .iter()
        .filter(|path| {
            debug!("{}", path.display());
            path.join(".lodestone_config").is_file()
        })
        .map(|path| {
            // read config as json
            let config: Value = serde_json::from_reader(
                std::fs::File::open(path.join(".lodestone_config")).unwrap(),
            )
            .unwrap();
            config
        })
        .map(|config| {
            match config["game_type"]
                .as_str()
                .unwrap()
                .to_ascii_lowercase()
                .as_str()
            {
                "minecraft" => {
                    debug!(
                        "Restoring Minecraft instance {}",
                        config["name"].as_str().unwrap()
                    );
                    minecraft::Instance::restore(
                        serde_json::from_value(config).unwrap(),
                        event_broadcaster.clone(),
                    )
                }
                _ => unimplemented!(),
            }
        })
    {
        let instance = instance_future.await;
        ret.insert(
            instance.uuid().await.to_string(),
            Arc::new(Mutex::new(instance)),
        );
    }
    ret
}

async fn restore_users(path_to_user_json: &Path) -> HashMap<String, User> {
    // create user file if it doesn't exist
    if tokio::fs::OpenOptions::new()
        .read(true)
        .create(true)
        .write(true)
        .open(path_to_user_json)
        .await
        .unwrap()
        .metadata()
        .await
        .unwrap()
        .len()
        == 0
    {
        return HashMap::new();
    }
    let users: HashMap<String, User> = serde_json::from_reader(
        tokio::fs::File::open(path_to_user_json)
            .await
            .unwrap()
            .into_std()
            .await,
    )
    .unwrap();
    users
}

async fn download_dependencies() -> Result<(), Error> {
    let arch = if std::env::consts::ARCH == "x86_64" {
        "x64"
    } else {
        std::env::consts::ARCH
    };

    let os = std::env::consts::OS;
    let _7zip_name = format!("7z_{}_{}", os, arch);
    let path_to_7z = PATH_TO_BINARIES.with(|v| v.join("7zip"));
    // check if 7z is already downloaded
    if !path_to_7z.join(&_7zip_name).exists() {
        info!("Downloading 7z");
        let _7z = download_file(
            format!(
                "https://github.com/Lodestone-Team/dependencies/raw/main/7z_{}_{}",
                os, arch
            )
            .as_str(),
            path_to_7z.as_ref(),
            Some(_7zip_name.as_str()),
            &|_| {},
            false,
        )
        .await?;
    } else {
        info!("7z already downloaded");
    }
    if os != "windows" {
        Command::new("chmod")
            .arg("+x")
            .arg(path_to_7z.join(&_7zip_name))
            .output()
            .await
            .unwrap();
    }
    Ok(())
}

pub async fn run() {
    env_logger::builder()
        .filter_level(log::LevelFilter::Debug)
        .format_module_path(false)
        // .format_timestamp(None)
        .format_target(false)
        .init();
    let lodestone_path = LODESTONE_PATH.with(|path| path.clone());
    create_dir_all(&lodestone_path).await.unwrap();
    std::env::set_current_dir(&lodestone_path).expect("Failed to set current dir");

    create_dir_all(PATH_TO_BINARIES.with(|path| path.clone()))
        .await
        .unwrap();

    create_dir_all(PATH_TO_STORES.with(|path| path.clone()))
        .await
        .unwrap();

    let web_path = lodestone_path.join("web");
    let path_to_intances = lodestone_path.join("instances");
    create_dir_all(&web_path).await.unwrap();
    create_dir_all(&path_to_intances).await.unwrap();
    info!("Lodestone path: {}", lodestone_path.display());

    download_dependencies().await.unwrap();

    let (tx, _rx): (Sender<Event>, Receiver<Event>) = broadcast::channel(256);

    let stateful_users = Stateful::new(
        restore_users(&PATH_TO_USERS.with(|v| v.to_owned())).await,
        {
            Box::new(move |users, _| {
                serde_json::to_writer(
                    std::fs::File::create(&PATH_TO_USERS.with(|v| v.to_owned())).unwrap(),
                    users,
                )
                .unwrap();
                Ok(())
            })
        },
        {
            Box::new(move |users, _| {
                serde_json::to_writer(
                    std::fs::File::create(&PATH_TO_USERS.with(|v| v.to_owned())).unwrap(),
                    users,
                )
                .unwrap();
                Ok(())
            })
        },
    );

    let stateful_event_buffer = Stateful::new(
        AllocRingBuffer::with_capacity(512),
        Box::new(|_, _| Ok(())),
        Box::new(|_event_buffer, _| {
            // todo: write to persistent storage
            Ok(())
        }),
    );

    let stateful_console_out_buffer = Stateful::new(
        HashMap::new(),
        Box::new(|_, _| Ok(())),
        Box::new(|_event_buffer, _| {
            // todo: write to persistent storage
            Ok(())
        }),
    );

    let first_time_setup_key = if !stateful_users
        .get_ref()
        .iter()
        .any(|(_, user)| user.is_owner)
    {
        let key = rand_alphanumeric(16);
        // log the first time setup key in green so it's easy to find
        info!("\x1b[32mFirst time setup key: {}\x1b[0m", key);
        Some(key)
    } else {
        None
    };
    let instances = restore_instances(&lodestone_path, &tx).await;
    for instance in instances.values() {
        let mut instance = instance.lock().await;
        if instance.auto_start().await {
            info!("Auto starting instance {}", instance.name().await);
            if let Err(e) = instance.start().await {
                error!(
                    "Failed to start instance {}: {:?}",
                    instance.name().await,
                    e
                );
            }
        }
    }
    let mut allocated_ports = HashSet::new();
    for (_, instance) in instances.iter() {
        let instance = instance.lock().await;
        allocated_ports.insert(instance.port().await);
    }
    let shared_state = AppState {
        instances: Arc::new(Mutex::new(instances)),
        users: Arc::new(Mutex::new(stateful_users)),
        events_buffer: Arc::new(Mutex::new(stateful_event_buffer)),
        console_out_buffer: Arc::new(Mutex::new(stateful_console_out_buffer)),
        monitor_buffer: Arc::new(Mutex::new(HashMap::new())),
        event_broadcaster: tx.clone(),
        is_setup: Arc::new(AtomicBool::new(false)),
        uuid: Uuid::new_v4().to_string(),
        client_name: Arc::new(Mutex::new(format!(
            "{}'s Lodestone client",
            whoami::realname()
        ))),
        up_since: chrono::Utc::now().timestamp(),
        port_allocator: Arc::new(Mutex::new(PortAllocator::new(allocated_ports))),
        first_time_setup_key: Arc::new(Mutex::new(first_time_setup_key)),
        system: Arc::new(Mutex::new(sysinfo::System::new_all())),
    };

    let event_buffer_task = {
        let event_buffer = shared_state.events_buffer.clone();
        let console_out_buffer = shared_state.console_out_buffer.clone();
        let mut event_receiver = tx.subscribe();
        async move {
            loop {
                let result = event_receiver.recv().await;
                if let Err(error) = result.as_ref() {
                    match error {
                        RecvError::Lagged(_) => {
                            warn!("Event buffer lagged");
                            continue;
                        }
                        RecvError::Closed => {
                            warn!("Event buffer closed");
                            break;
                        }
                    }
                }
                let event = result.unwrap();
                if event.is_event_console_message() {
                    console_out_buffer
                        .lock()
                        .await
                        .transform(Box::new(move |buffer| -> Result<(), Error> {
                            buffer
                                .entry(event.get_instance_uuid().unwrap())
                                .or_insert_with(|| AllocRingBuffer::with_capacity(512))
                                .push(event.clone());
                            Ok(())
                        }))
                        .unwrap();
                } else {
                    event_buffer
                        .lock()
                        .await
                        .transform(Box::new(move |buffer| -> Result<(), Error> {
                            buffer.push(event.clone());
                            Ok(())
                        }))
                        .unwrap();
                }
            }
        }
    };

    let monitor_report_task = {
        let monitor_buffer = shared_state.monitor_buffer.clone();
        let instances = shared_state.instances.clone();
        async move {
            let mut interval = tokio::time::interval(Duration::from_secs(1));
            loop {
                for (uuid, instance) in instances.lock().await.iter() {
                    let report = instance.lock().await.monitor().await;
                    monitor_buffer
                        .lock()
                        .await
                        .entry(uuid.to_owned())
                        .or_insert_with(|| AllocRingBuffer::with_capacity(64))
                        .push(report);
                }
                interval.tick().await;
            }
        }
    };

    let cors = CorsLayer::new()
        .allow_methods([
            Method::GET,
            Method::POST,
            Method::PUT,
            Method::OPTIONS,
            Method::PATCH,
            Method::DELETE,
        ])
        .allow_headers([header::ORIGIN, header::CONTENT_TYPE, header::AUTHORIZATION]) // Note I can't find X-Auth-Token but it was in the original rocket version, hope it's fine
        .allow_origin(Any);

    let api_routes = Router::new()
        .merge(get_events_routes())
        .merge(get_instance_setup_config_routes())
        .merge(get_instance_manifest_routes())
        .merge(get_instance_server_routes())
        .merge(get_instance_config_routes())
        .merge(get_instance_players_routes())
        .merge(get_instance_routes())
        .merge(get_system_routes())
        .merge(get_checks_routes())
        .merge(get_user_routes())
        .merge(get_client_info_routes())
        .merge(get_setup_route())
        .merge(get_monitor_routes())
        .merge(get_instance_macro_routes())
        .merge(get_instance_fs_routes())
        .merge(get_global_fs_routes())
        .layer(Extension(shared_state.clone()))
        .layer(cors);
    let app = Router::new().nest("/api/v1", api_routes);
    let addr = SocketAddr::from(([0, 0, 0, 0], 16_662));
    select! {
        _ = event_buffer_task => info!("Event buffer task exited"),
        _ = monitor_report_task => info!("Monitor report task exited"),
        _ = axum::Server::bind(&addr)
        .serve(app.into_make_service()) => info!("Server exited"),
        _ = tokio::signal::ctrl_c() => info!("Ctrl+C received"),
    }
    // cleanup
    let instances = shared_state.instances.lock().await;
    for (_, instance) in instances.iter() {
        let mut instance = instance.lock().await;
        let _ = instance.stop().await;
    }
}
