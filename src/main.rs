use actix::prelude::*;
use actix_cors::Cors;
use actix_web::{web, App, Error, HttpRequest, HttpResponse, HttpServer, Responder};
use actix_web_actors::ws;
use serde::Serialize;
use sqlx::MySqlPool;
use std::collections::HashMap;
use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::broadcast;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Debug, Serialize, sqlx::FromRow)]
struct Camara {
    id: i32,
    link: String,
}

struct StreamInfo {
    ffmpeg_process: Child,
    ws_url: String,
    broadcaster: broadcast::Sender<Vec<u8>>,
    active_clients: Arc<AtomicUsize>,
}

#[derive(Clone)]
struct AppState {
    pool: MySqlPool,
    active_streams: Arc<Mutex<HashMap<i32, StreamInfo>>>,
}

#[derive(Clone)]
struct WsConfig {
    server_host: String,
    server_port: u16,
}

async fn get_camaras(data: web::Data<AppState>) -> impl Responder {
    match sqlx::query_as::<_, Camara>("SELECT id, link FROM camaras")
        .fetch_all(&data.pool)
        .await
    {
        Ok(camaras) => HttpResponse::Ok().json(camaras),
        Err(e) => {
            eprintln!("DB error: {:?}", e);
            HttpResponse::InternalServerError().body("DB error")
        }
    }
}

async fn get_camara_by_id(pool: &MySqlPool, id: i32) -> Result<Option<Camara>, sqlx::Error> {
    sqlx::query_as::<_, Camara>("SELECT id, link FROM camaras WHERE id = ?")
        .bind(id)
        .fetch_optional(pool)
        .await
}

fn create_stream(camara: &Camara, server_host: &str, http_port: u16) -> std::io::Result<Child> {
    let args = vec![
        "-rtsp_transport", "udp",
        "-i", &camara.link,
        "-analyzeduration", "500000",
        "-probesize", "32",
        "-r", "30",
        "-b:v", "2500k",
        "-maxrate", "3500k",
        "-bufsize", "1000k",
        "-g", "30",
        "-pix_fmt", "yuv420p",
        "-preset", "veryfast",
        "-tune", "zerolatency",
        "-vcodec", "mpeg1video",
        "-acodec", "mp2",
        "-ar", "44100",
        "-ac", "2",
        "-f", "mpegts",
        "pipe:1",
    ];
    Command::new("ffmpeg")
        .args(&args)
        .stdout(Stdio::piped())
        .spawn()
}

async fn get_stream(
    path: web::Path<i32>,
    data: web::Data<AppState>,
    ws_config: web::Data<WsConfig>,
) -> impl Responder {
    let camara_id = path.into_inner();
    let pool = &data.pool;
    let active_streams = &data.active_streams;

    match get_camara_by_id(pool, camara_id).await {
        Ok(Some(camara)) => {
            // Si ya existe un stream activo para la cámara, se devuelve su URL.
            if let Some(info) = active_streams.lock().unwrap().get(&camara.id) {
                return HttpResponse::Ok().json(serde_json::json!({
                    "id": camara.id,
                    "link": camara.link,
                    "wsUrl": info.ws_url,
                }));
            }
            // Crear el proceso ffmpeg para el stream.
            let server_host = &ws_config.server_host;
            let http_port = ws_config.server_port;
            let mut child = match create_stream(&camara, server_host, http_port) {
                Ok(child) => child,
                Err(e) => {
                    eprintln!("Error launching ffmpeg: {:?}", e);
                    return HttpResponse::InternalServerError().body("Error launching stream");
                }
            };

            // Canal para transmitir datos y contador de clientes activos.
            let (tx, _) = broadcast::channel(16);
            let active_clients = Arc::new(AtomicUsize::new(0));
            let tx_thread = tx.clone();
            if let Some(mut stdout) = child.stdout.take() {
                std::thread::spawn(move || {
                    let mut buf = [0u8; 8192];
                    while let Ok(n) = stdout.read(&mut buf) {
                        if n > 0 {
                            let _ = tx_thread.send(buf[..n].to_vec());
                        } else {
                            break;
                        }
                    }
                });
            }
            let ws_url = format!("ws://{}:{}/ws/stream/{}", server_host, http_port, camara.id);
            let info = StreamInfo {
                ffmpeg_process: child,
                ws_url: ws_url.clone(),
                broadcaster: tx,
                active_clients: active_clients.clone(),
            };
            active_streams.lock().unwrap().insert(camara.id, info);

            // Monitorear: esperar 5 segundos y, si no hay clientes conectados, detener la transmisión.
            let camara_id_clone = camara.id;
            let active_streams_clone = data.active_streams.clone();
            let active_clients_clone = active_clients.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(5)).await;
                if active_clients_clone.load(Ordering::SeqCst) == 0 {
                    let mut streams = active_streams_clone.lock().unwrap();
                    if let Some(mut info) = streams.remove(&camara_id_clone) {
                        let _ = info.ffmpeg_process.kill();
                        println!("No hay clientes conectados. Stream {} detenido.", camara_id_clone);
                    }
                }
            });

            HttpResponse::Ok().json(serde_json::json!({
                "id": camara.id,
                "link": camara.link,
                "wsUrl": ws_url,
            }))
        }
        Ok(None) => HttpResponse::NotFound().json(serde_json::json!({ "error": "Camera not found" })),
        Err(e) => {
            eprintln!("DB error: {:?}", e);
            HttpResponse::InternalServerError().body("DB error")
        }
    }
}

async fn delete_stream(path: web::Path<i32>, data: web::Data<AppState>) -> impl Responder {
    let camara_id = path.into_inner();
    let mut streams = data.active_streams.lock().unwrap();
    if let Some(mut info) = streams.remove(&camara_id) {
        let _ = info.ffmpeg_process.kill();
        HttpResponse::Ok().json(serde_json::json!({
            "message": format!("Stream de la cámara id {} detenido", camara_id)
        }))
    } else {
        HttpResponse::NotFound().json(serde_json::json!({ "error": "Stream no activo" }))
    }
}

struct StreamChunk(Vec<u8>);
impl Message for StreamChunk {
    type Result = ();
}

struct MyWebSocket {
    camara_id: i32,
    hb: Instant,
    rx: broadcast::Receiver<Vec<u8>>,
    active_clients: Arc<AtomicUsize>,
}

impl MyWebSocket {
    fn new(
        camara_id: i32,
        rx: broadcast::Receiver<Vec<u8>>,
        active_clients: Arc<AtomicUsize>,
    ) -> Self {
        Self {
            camara_id,
            hb: Instant::now(),
            rx,
            active_clients,
        }
    }
    fn start_heartbeat(&self, ctx: &mut ws::WebsocketContext<Self>) {
        ctx.run_interval(Duration::from_secs(5), |_, ctx| ctx.ping(b""));
    }
}

impl Actor for MyWebSocket {
    type Context = ws::WebsocketContext<Self>;
    fn started(&mut self, ctx: &mut Self::Context) {
        self.start_heartbeat(ctx);
        let mut rx = self.rx.resubscribe();
        let addr = ctx.address();
        tokio::spawn(async move {
            while let Ok(chunk) = rx.recv().await {
                addr.do_send(StreamChunk(chunk));
            }
        });
    }
    fn stopped(&mut self, _ctx: &mut Self::Context) {
        // Al cerrar la conexión, se decrementa el contador de clientes.
        self.active_clients.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Handler<StreamChunk> for MyWebSocket {
    type Result = ();
    fn handle(&mut self, msg: StreamChunk, ctx: &mut Self::Context) {
        ctx.binary(msg.0);
    }
}

impl StreamHandler<Result<ws::Message, ws::ProtocolError>> for MyWebSocket {
    fn handle(&mut self, msg: Result<ws::Message, ws::ProtocolError>, ctx: &mut Self::Context) {
        match msg {
            Ok(ws::Message::Ping(msg)) => ctx.pong(&msg),
            Ok(ws::Message::Close(_)) => ctx.stop(),
            _ => (),
        }
    }
}

async fn ws_stream(
    req: HttpRequest,
    stream: web::Payload,
    path: web::Path<(i32,)>,
    data: web::Data<AppState>,
) -> Result<HttpResponse, Error> {
    let camara_id = path.into_inner().0;
    let streams = data.active_streams.lock().unwrap();
    if let Some(info) = streams.get(&camara_id) {
        // Al conectar un cliente se incrementa el contador.
        info.active_clients.fetch_add(1, Ordering::SeqCst);
        let rx = info.broadcaster.subscribe();
        ws::start(MyWebSocket::new(camara_id, rx, info.active_clients.clone()), &req, stream)
    } else {
        Err(actix_web::error::ErrorNotFound("Stream not found"))
    }
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    let server_host = "192.168.0.3".to_string();
    let http_port: u16 = 3001;
    let database_url = "mysql://root:*Royner123123*@fuchibol.ddns.net/carrera";
    let pool = MySqlPool::connect(&database_url)
        .await
        .expect("DB connection error");
    let app_state = AppState {
        pool,
        active_streams: Arc::new(Mutex::new(HashMap::new())),
    };
    let ws_config = WsConfig {
        server_host: server_host.clone(),
        server_port: http_port,
    };

    HttpServer::new(move || {
        App::new()
            .wrap(
                Cors::default()
                    .allow_any_origin()
                    .allow_any_method()
                    .allow_any_header()
                    .max_age(3600),
            )
            .app_data(web::Data::new(app_state.clone()))
            .app_data(web::Data::new(ws_config.clone()))
            .route("/api/camaras", web::get().to(get_camaras))
            .route("/api/stream/{id}", web::get().to(get_stream))
            .route("/api/stream/{id}", web::delete().to(delete_stream))
            .route("/ws/stream/{id}", web::get().to(ws_stream))
    })
    .bind(("0.0.0.0", http_port))?
    .run()
    .await
}
