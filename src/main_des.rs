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

/// Representa una cámara extraída de la BD.
#[derive(Debug, Serialize, sqlx::FromRow)]
struct Camara {
    id: i32,
    link: String,
}

/// Información del stream activo de una cámara.
struct StreamInfo {
    ffmpeg_process: Child,
    ws_url: String,
    broadcaster: broadcast::Sender<Vec<u8>>,
}

/// Estado compartido de la aplicación.
#[derive(Clone)]
struct AppState {
    pool: MySqlPool,
    active_streams: Arc<Mutex<HashMap<i32, StreamInfo>>>,
}

/// Configuración para WebSocket: se usa el mismo puerto del servidor HTTP.
#[derive(Clone)]
struct WsConfig {
    server_host: String,
    server_port: u16,
}

/// Endpoint: Obtiene todas las cámaras.
async fn get_camaras(data: web::Data<AppState>) -> impl Responder {
    let rows = sqlx::query_as::<_, Camara>("SELECT id, link FROM camaras")
        .fetch_all(&data.pool)
        .await;
    match rows {
        Ok(camaras) => HttpResponse::Ok().json(camaras),
        Err(e) => {
            println!("Error en DB: {:?}", e);
            HttpResponse::InternalServerError().body("Error al consultar la base de datos")
        }
    }
}

/// Función auxiliar para obtener una cámara por id.
async fn get_camara_by_id(pool: &MySqlPool, id: i32) -> Result<Option<Camara>, sqlx::Error> {
    sqlx::query_as::<_, Camara>("SELECT id, link FROM camaras WHERE id = ?")
        .bind(id)
        .fetch_optional(pool)
        .await
}

fn create_stream(camara: &Camara, server_host: &str, http_port: u16) -> std::io::Result<Child> {
    let ffmpeg_args = vec![
        // Usar UDP puede reducir la latencia, aunque se pierde cierta confiabilidad
        "-rtsp_transport", "udp",
        "-i", &camara.link,
        // Reducir el tiempo de análisis para una inicialización más rápida
        "-analyzeduration", "500000",
        "-probesize", "32",
        "-r", "30",
        "-b:v", "2500k",
        "-maxrate", "3500k",
        // Reducir el buffer para minimizar la latencia
        "-bufsize", "1000k",
        // Usar un GOP más corto para enviar key frames con mayor frecuencia
        "-g", "30",
        "-pix_fmt", "yuv420p",
        "-preset", "veryfast",
        // Agregar tune zerolatency si es compatible con el encoder (nota: puede no tener efecto en mpeg1video)
        "-tune", "zerolatency",
        "-vcodec", "mpeg1video",
        "-acodec", "mp2",
        "-ar", "44100",
        "-ac", "2",
        "-f", "mpegts",
        "pipe:1",
    ];

    println!(
        "Iniciando stream para la cámara id {} en ws://{}:{}/ws/stream/{}",
        camara.id, server_host, http_port, camara.id
    );
    let child = Command::new("ffmpeg")
        .args(&ffmpeg_args)
        .stdout(Stdio::piped())
        .spawn()?;
    Ok(child)
}


/// Endpoint: Obtiene (o crea si no existe) el stream de una cámara.
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
            // Si ya existe un stream activo, retorna la URL del WebSocket.
            {
                let streams = active_streams.lock().unwrap();
                if let Some(info) = streams.get(&camara.id) {
                    println!("Stream ya activo para la cámara id {}.", camara.id);
                    return HttpResponse::Ok().json(serde_json::json!({
                        "id": camara.id,
                        "link": camara.link,
                        "wsUrl": info.ws_url,
                    }));
                }
            }
            // Sino, se crea el stream on demand.
            let server_host = &ws_config.server_host;
            let http_port = ws_config.server_port;
            let mut child = match create_stream(&camara, server_host, http_port) {
                Ok(child) => child,
                Err(e) => {
                    println!("Error al iniciar ffmpeg: {:?}", e);
                    return HttpResponse::InternalServerError().body("Error al iniciar el stream");
                }
            };

            // Crear un canal broadcast para distribuir los datos.
            let (tx, _) = broadcast::channel(16);
            // Clonar el sender para usarlo dentro del hilo sin mover el original.
            let tx_thread = tx.clone();

            // Se crea un hilo que lee de stdout y envía bloques de datos al canal.
            if let Some(mut stdout) = child.stdout.take() {
                std::thread::spawn(move || {
                    let mut buffer = [0u8; 8192];
                    loop {
                        match stdout.read(&mut buffer) {
                            Ok(n) if n > 0 => {
                                // Ignoramos errores de envío.
                                let _ = tx_thread.send(buffer[..n].to_vec());
                            }
                            _ => break,
                        }
                    }
                    println!("Finaliza lectura de stdout");
                });
            } else {
                println!("No se pudo obtener stdout de ffmpeg");
            }

            let ws_url = format!("ws://{}:{}/ws/stream/{}", server_host, http_port, camara.id);
            let info = StreamInfo {
                ffmpeg_process: child,
                ws_url: ws_url.clone(),
                broadcaster: tx,
            };
            {
                let mut streams = active_streams.lock().unwrap();
                streams.insert(camara.id, info);
            }
            HttpResponse::Ok().json(serde_json::json!({
                "id": camara.id,
                "link": camara.link,
                "wsUrl": ws_url,
            }))
        }
        Ok(None) => HttpResponse::NotFound().json(serde_json::json!({ "error": "Cámara no encontrada" })),
        Err(e) => {
            println!("Error en DB: {:?}", e);
            HttpResponse::InternalServerError().body("Error al consultar la base de datos")
        }
    }
}

/// Endpoint: Detiene un stream activo.
async fn delete_stream(path: web::Path<i32>, data: web::Data<AppState>) -> impl Responder {
    let camara_id = path.into_inner();
    let mut streams = data.active_streams.lock().unwrap();
    if let Some(mut info) = streams.remove(&camara_id) {
        let _ = info.ffmpeg_process.kill();
        println!("Stream de la cámara id {} detenido", camara_id);
        HttpResponse::Ok().json(serde_json::json!({
            "message": format!("Stream de la cámara id {} detenido", camara_id)
        }))
    } else {
        HttpResponse::NotFound().json(serde_json::json!({ "error": "Stream no activo" }))
    }
}

/// Mensaje para transmitir datos del stream.
struct StreamChunk(Vec<u8>);
impl Message for StreamChunk {
    type Result = ();
}

/// Actor WebSocket para gestionar la conexión con el cliente y enviar el flujo de video.
struct MyWebSocket {
    camara_id: i32,
    hb: Instant,
    rx: broadcast::Receiver<Vec<u8>>,
}

impl MyWebSocket {
    fn new(camara_id: i32, rx: broadcast::Receiver<Vec<u8>>) -> Self {
        Self {
            camara_id,
            hb: Instant::now(),
            rx,
        }
    }

    fn start_heartbeat(&self, ctx: &mut ws::WebsocketContext<Self>) {
        ctx.run_interval(Duration::from_secs(5), |_, ctx| {
            ctx.ping(b"");
        });
    }
}

impl Actor for MyWebSocket {
    type Context = ws::WebsocketContext<Self>;

    fn started(&mut self, ctx: &mut Self::Context) {
        self.start_heartbeat(ctx);

        // Se clona el receptor y se crea una tarea asíncrona para reenviar los datos.
        let mut rx = self.rx.resubscribe();
        let addr = ctx.address();
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(chunk) => {
                        addr.do_send(StreamChunk(chunk));
                    }
                    Err(e) => {
                        println!("Error en broadcast: {:?}", e);
                        break;
                    }
                }
            }
        });

        println!("Cliente conectado al WebSocket para cámara id {}", self.camara_id);
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
            Ok(ws::Message::Text(text)) => println!("Mensaje de texto: {}", text),
            Ok(ws::Message::Binary(bin)) => ctx.binary(bin),
            Ok(ws::Message::Close(_)) => {
                println!("Cliente desconectado del WebSocket de cámara id {}", self.camara_id);
                ctx.stop();
            }
            _ => (),
        }
    }
}

/// Endpoint WebSocket: se conecta al stream de una cámara.
async fn ws_stream(
    req: HttpRequest,
    stream: web::Payload,
    path: web::Path<(i32,)>,
    data: web::Data<AppState>,
) -> Result<HttpResponse, Error> {
    let camara_id = path.into_inner().0;
    let streams = data.active_streams.lock().unwrap();
    if let Some(info) = streams.get(&camara_id) {
        let rx = info.broadcaster.subscribe();
        ws::start(MyWebSocket::new(camara_id, rx), &req, stream)
    } else {
        Err(actix_web::error::ErrorNotFound("Stream not found"))
    }
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    // Configuración: host del servidor y puerto HTTP (y WebSocket).
    let server_host = "192.168.0.6".to_string();
    let http_port: u16 = 3000;

    // Conexión a la base de datos MySQL.
    let database_url = "mysql://root:*Royner123123*@fuchibol.ddns.net/carrera";
    let pool = MySqlPool::connect(&database_url)
        .await
        .expect("Error conectando a la base de datos");

    let app_state = AppState {
        pool,
        active_streams: Arc::new(Mutex::new(HashMap::new())),
    };

    // Configuración para WebSocket usando el mismo puerto que HTTP.
    let ws_config = WsConfig {
        server_host: server_host.clone(),
        server_port: http_port,
    };

    println!("Servidor API iniciado en http://{}:{}", server_host, http_port);
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
