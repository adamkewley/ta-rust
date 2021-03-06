use std::path::{Path, PathBuf};
use std::fs::{self, File};
use std::io::{self, ErrorKind, Read, Write};
use std::sync::{Arc};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::collections::{HashMap};
use std::net::{SocketAddr};
use std::process::{Command, Child, Stdio};
use std::thread::{self, JoinHandle};
use std::sync::mpsc;
use std::time::Instant;
use std::mem;

use actix::{Actor, Running, Message, Handler, StreamHandler, ActorContext, AsyncContext};
use actix_web::{web, http, App, HttpRequest, HttpResponse, HttpServer, Responder};
use actix_web_actors::ws;
use bytes::Bytes;
use serde_derive::{Deserialize, Serialize};
use clap;
use toml;
use rust_embed::RustEmbed;
#[macro_use] extern crate log;

#[derive(Serialize, Deserialize)]
struct GamesResponse {
    games: Vec<GameResponse>,
}

#[derive(Clone, Serialize, Deserialize)]
struct GameResponse {
    id: String,
    name: String,
    author: String,
    description: String,
    play: GamePlayDetailsResponse,
}

#[derive(Clone, Serialize, Deserialize)]
struct GamePlayDetailsResponse {
    url: String,
}

struct ServerState {
    next_game_id: AtomicUsize,
    configs: Arc<dyn GamesLoader>,
    games_dir: PathBuf,
}

#[derive(Clone, Deserialize)]
struct GameConfig {
    name: String,
    description: String,
    author: String,
    application: String,
    args: Vec<String>,
}

fn to_game_response(config: &(String, GameConfig)) -> GameResponse {
    GameResponse {
        id: config.0.clone(),
        name: config.1.name.clone(),
        author: config.1.author.clone(),
        description: config.1.description.clone(),
        play: GamePlayDetailsResponse {
            url: format!("/play/{}", config.0),
        }
    }
}

fn to_games_json(configs: &Vec<(String, GameConfig)>) -> Result<Vec<u8>, serde_json::error::Error> {
    let resp = GamesResponse {
        games: configs.iter().map(|cfg| to_game_response(cfg)).collect(),
    };

    serde_json::to_vec_pretty(&resp)
}

fn should_ignore(path: &Path) -> io::Result<bool> {
    let game_dirname = &path
        .file_name()
        .ok_or(io::Error::new(ErrorKind::Other, format!("{:?}: invalid path in games dir", path)))?
        .to_str()
        .unwrap();

    if let Some(c) = game_dirname.chars().next() {
        match c {
            '.' => Ok(true),
            '#' => Ok(true),
            _ => Ok(false),
        }
    } else {
        Ok(false)
    }
}

fn load_config(config_path: &Path) -> std::io::Result<GameConfig> {
    let mut fd = File::open(config_path)?;
    let mut s = String::new();
    fd.read_to_string(&mut s)?;

    Ok(toml::from_str(&s)?)
}

fn load_game_configs(games_dir: &Path) -> std::io::Result<Vec<(String, GameConfig)>> {
    if !games_dir.exists() {
        let msg = format!("{}: no such file or directory", games_dir.display());
        return Err(io::Error::new(ErrorKind::NotFound, msg));
    }

    if !games_dir.is_dir() {
        let msg = format!("{}: not a directory", games_dir.display());
        return Err(io::Error::new(ErrorKind::InvalidInput, msg));
    }

    let mut configs = Vec::<(String, GameConfig)>::new();

    for entry in fs::read_dir(games_dir)? {
        let entry = entry?;
        let game_id = entry.file_name().into_string().map_err(|os_str| io::Error::new(io::ErrorKind::InvalidInput, format!("{:?}: not a valid number", os_str)))?;
        let path = entry.path();

        if should_ignore(&path)? {
            continue;
        }

        if !entry.metadata()?.is_dir() {
            continue;
        }

        let cfg_path = path.join("textadventure.toml");

        if cfg_path.is_file() {
            let cfg = load_config(&cfg_path)?;
            configs.push((game_id, cfg));
        }
    }

    configs.sort_by(|(_, conf1), (_, conf2)| conf1.name.cmp(&conf2.name));

    Ok(configs)
}

async fn games(st: web::Data<ServerState>) -> impl Responder {
    match st.configs.load_games_list_as_json() {
        Ok(json) => {
            HttpResponse::Ok()
                .header(http::header::CONTENT_TYPE, "application/json")
                .body(json)
        },
        Err(e) => {
            error!("error loading games: {:?}", e);
            HttpResponse::InternalServerError().finish()
        },
    }
}

#[derive(RustEmbed)]
#[folder = "static"]
struct StaticAsset;

async fn get_static_asset(path: web::Path<(String,)>) -> impl Responder {
    match StaticAsset::get(&path.0) {
        Some(cow) => HttpResponse::Ok().body(cow.into_owned()),
        None => HttpResponse::NotFound().finish()
    }
}

async fn get_index() -> impl Responder {
    let t = (String::from("index.html"),);
    get_static_asset(web::Path::from(t)).await
}

enum ProcessSt {
    Booting,
    Booted(BootedProcess),
    Exited,
}

enum StdinMessage {
    Data(Vec<u8>),
    Eof,
}

struct StdoutStats {
    bytes_written: usize,
}

struct StderrStats {
    bytes_written: usize,
}

struct StdinStats {
    bytes_read: usize,
}

struct BootedProcess {
    process: Child,
    stdin: mpsc::Sender<StdinMessage>,

    started_at: Instant,
    stdout_thr: JoinHandle<StdoutStats>,
    stderr_thr: JoinHandle<StderrStats>,
    stdin_thr: JoinHandle<StdinStats>,
}

struct GameActor {
    id: usize,
    config: Arc<GameConfig>,
    game_dir: PathBuf,
    process_st: ProcessSt,
}

struct StdoutMsg {
    bytes: Bytes,
}

impl Message for StdoutMsg {
    type Result = ();
}

struct ExitMsg();

impl Message for ExitMsg {
    type Result = ();
}

impl GameActor {
    fn new(id: usize, game_dir: PathBuf, config: Arc<GameConfig>) -> Self {
        Self {
            id: id,
            game_dir: game_dir,
            config: config,
            process_st: ProcessSt::Booting,
        }
    }
}

impl Actor for GameActor {
    type Context = ws::WebsocketContext<Self>;

    fn stopping(&mut self, _ctx: &mut Self::Context) -> Running {
        let st = mem::replace(&mut self.process_st, ProcessSt::Exited);

        if let ProcessSt::Booted(p) = st {
            let BootedProcess  {
                mut process,
                stdin,
                started_at: _,
                stdin_thr,
                stdout_thr,
                stderr_thr
            } = p;

            if let Err(_) = process.kill() {
                error!("game double-killed (id = {}): this is a developer error", self.id);
            };

            let exit_code = match process.wait() {
                // TODO: Handle signal exit code (rather than coercing it to 1)
                Ok(exit_code) => exit_code.code().unwrap_or(1),
                Err(e) => {
                    error!("error waiting for game (id = {}): it probably didn't start running: {:?}", self.id, e);
                    1
                }
            };

            let exited_at = Instant::now();
            let run_time = exited_at.duration_since(p.started_at);

            // The stdin thread might be waiting on input still, so
            // ensure that it receives at least one EOF
            let _ = stdin.send(StdinMessage::Eof);
            let stdin_result = stdin_thr.join().unwrap();

            // These threads block on reading data from the
            // subprocess. The reads will fail for an exited process,
            // so this is ok.
            let stdout_result = stdout_thr.join().unwrap();
            let stderr_result = stderr_thr.join().unwrap();

            info!("game exited (id = {}): exit_code = {}, run_time = {} ms, stdin_bytes = {:?}, stdout_bytes = {:?}, stderr_bytes = {:?}",
                  self.id,
                  exit_code,
                  run_time.as_millis(),
                  stdin_result.bytes_read,
                  stdout_result.bytes_written,
                  stderr_result.bytes_written);
        }

        Running::Stop
    }
}

impl StreamHandler<Result<ws::Message, ws::ProtocolError>> for GameActor {

    fn started(&mut self, ctx: &mut Self::Context) {
        let mut b = Command::new(&self.config.application);
        b.args(self.config.args.iter());
        b.stdin(Stdio::piped());
        b.stdout(Stdio::piped());
        b.stderr(Stdio::piped());
        b.current_dir(&self.game_dir);

        info!("game booting (id = {}): application = {}, args = {:?}", self.id, self.config.application, self.config.args);
        match b.spawn() {
            Ok(mut proc) => {
                let started_at = Instant::now();
                let id = self.id;
                let should_send_exit = Arc::new(AtomicBool::new(false));

                let stdout_thread_name = format!("game_id {}: stdout reader", id);
                let stdout_addr = ctx.address();
                let mut stdout = proc.stdout.take().unwrap();
                let stdout_should_send_exit = should_send_exit.clone();
                let stdout_thr = thread::Builder::new().name(stdout_thread_name).spawn(move || {
                    let addr = stdout_addr;
                    let mut buf: [u8; 512] = [0; 512];
                    let mut bytes_written = 0;

                    loop {
                        let sz = stdout.read(&mut buf).unwrap_or(0);

                        if sz > 0 {
                            bytes_written += sz;
                            let bytes = Bytes::copy_from_slice(&buf[0..sz]);
                            addr.do_send(StdoutMsg { bytes: bytes });
                        } else {
                            if stdout_should_send_exit.swap(true, Ordering::Relaxed) {
                                addr.do_send(ExitMsg{});
                            }
                            break;
                        }
                    }
                    return StdoutStats { bytes_written: bytes_written };
                }).unwrap();

                let stderr_addr = ctx.address();
                let mut stderr = proc.stderr.take().unwrap();
                let stderr_thread_name = format!("game_id {}: stderr reader", id);
                let stderr_should_send_exit = should_send_exit;
                let stderr_thr = thread::Builder::new().name(stderr_thread_name).spawn(move || {
                    let addr = stderr_addr;
                    let mut buf: [u8; 512] = [0; 512];
                    let mut bytes_written = 0;

                    loop {
                        let sz = stderr.read(&mut buf).unwrap_or(0);

                        if sz > 0 {
                            bytes_written += sz;
                            let bytes = Bytes::copy_from_slice(&buf[0..sz]);
                            addr.do_send(StdoutMsg { bytes: bytes });
                        } else {
                            if stderr_should_send_exit.swap(true, Ordering::Relaxed) {
                                addr.do_send(ExitMsg {});
                            }
                            break;
                        }
                    }

                    return StderrStats { bytes_written: bytes_written };
                }).unwrap();

                let mut stdin = proc.stdin.take().unwrap();
                let (tx, rx) = mpsc::channel::<StdinMessage>();
                let stdin_thread_name = format!("game_id {}: stdin writer", id);
                let stdin_thr = thread::Builder::new().name(stdin_thread_name).spawn(move || {
                    let mut bytes_read = 0;
                    loop {
                        match rx.recv() {
                            Ok(msg) => {
                                match msg {
                                    StdinMessage::Data(buf) => {
                                        match stdin.write(&buf) {
                                            Ok(_) => {
                                                bytes_read += buf.len();
                                                stdin.flush().unwrap();
                                            },
                                            Err(_) => break,  // TODO
                                        }
                                    },
                                    StdinMessage::Eof => break,
                                }

                            },
                            Err(_) => break,  // TODO: error propagation
                        }
                    }

                    return StdinStats { bytes_read: bytes_read };
                }).unwrap();

                self.process_st = ProcessSt::Booted(BootedProcess {
                    process: proc,
                    stdin: tx,
                    started_at: started_at,
                    stdout_thr: stdout_thr,
                    stderr_thr: stderr_thr,
                    stdin_thr: stdin_thr,
                });
            },
            Err(e) => {
                error!("game could not be spawned (id = {}): {:?}", self.id, e);
                ctx.stop();
            },
        }
    }

    fn handle(
        &mut self,
        msg: Result<ws::Message, ws::ProtocolError>,
        ctx: &mut Self::Context,
    ) {
        match msg {
            Ok(ws::Message::Ping(msg)) => ctx.pong(&msg),
            Ok(ws::Message::Text(text)) => {
                if let ProcessSt::Booted(p) = &mut self.process_st {
                    if let Err(e) = p.stdin.send(StdinMessage::Data(text.into_bytes())) {
                        error!("error sending text to game process stdin (id = {}): {:?}", self.id, e);
                    };
                }
            },
            Ok(ws::Message::Binary(bin)) => {
                if let ProcessSt::Booted(p) = &mut self.process_st {
                    if let Err(e) = p.stdin.send(StdinMessage::Data(bin.to_vec())) {
                        error!("error sending binary to game process stdin (id = {}): {:?}", self.id, e);
                    };
                }
            },
            Ok(ws::Message::Close(_)) => {
                // Firefox sends this *before* closing the TCP
                // channel. Can cause leak-like behavior in Firefox
                // because it'll keep the TCP channel open even after
                // closing tabs etc.
                ctx.close(Option::None);
                ctx.stop();
            },
            _ => (),
        }
    }

    fn finished(&mut self, ctx: &mut Self::Context) {
        ctx.stop();
    }
}

impl Handler<StdoutMsg> for GameActor {
    type Result = ();

    fn handle(&mut self, msg: StdoutMsg, ctx: &mut Self::Context) -> Self::Result {
        ctx.binary(msg.bytes);
    }
}

impl Handler<ExitMsg> for GameActor {
    type Result = ();

    fn handle(&mut self, _msg: ExitMsg, ctx: &mut Self::Context) -> Self::Result {
        ctx.stop();
    }
}

async fn play(st: web::Data<ServerState>,
              req: HttpRequest,
              stream: web::Payload) -> Result<HttpResponse, actix_web::Error> {

    let gameid = req.match_info().query("gameid");
    match st.configs.get_game_by_id(gameid) {
        Ok(cfg) => {
            let game_dir = st.games_dir.join(gameid);
            let game_id = st.next_game_id.fetch_add(1, Ordering::Relaxed);
            let actor = GameActor::new(game_id, game_dir, cfg);
            ws::start(actor, &req, stream)
        },
        Err(e) => {
            match e.kind() {
                ErrorKind::NotFound => {
                    Ok(HttpResponse::NotFound().finish())
                },
                _ => {
                    Ok(HttpResponse::InternalServerError().finish())
                },
            }
        }
    }
}

struct CliArgs {
    games_dir: String,
    socket_addr: SocketAddr,
    workers: usize,
    cache_game_configs: bool,
}

fn parse_cli_args() -> io::Result<CliArgs> {
    let matches = clap::App::new("Text")
        .author("Adam Kewley <adamk117@gmail.com>")
        .about("Boots Textadventurer webserver daemon")
        .arg(clap::Arg::with_name("GAMES_DIR")
             .help("Directory containing game definitions")
             .required(true))
        .arg(clap::Arg::with_name("port")
             .short("p")
             .long("port")
             .value_name("PORT_NUMBER")
             .help("TCP port the daemon should listen on")
             .default_value("8080")
             .takes_value(true))
        .arg(clap::Arg::with_name("binding")
             .short("b")
             .long("binding")
             .value_name("IP")
             .help("Binds server to the specified IP")
             .default_value("0.0.0.0")
             .takes_value(true))
        .arg(clap::Arg::with_name("workers")
             .short("w")
             .long("workers")
             .value_name("NUM")
             .help("Number of webserver workers handling connections")
             .default_value("1")
             .takes_value(true))
        .arg(clap::Arg::with_name("cache-games")
             .short("c")
             .help("Cache game configurations when the server initially boots, rather than loading them for on each request.")
             .takes_value(false))
        .get_matches();

    let games_dir: String = matches.value_of("GAMES_DIR").unwrap().to_string();

    let port: u16 = {
        let port_str = matches.value_of("port").unwrap();
        let maybe_port_num = match port_str.parse::<u16>() {
            Ok(port) => Ok(port),
            Err(_) => Err(io::Error::new(io::ErrorKind::InvalidInput, format!("{}: not a valid number", port_str))),
        };
        maybe_port_num?
    };

    let ip = {
        let ip_str = matches.value_of("binding").unwrap();
        ip_str.parse().map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, format!("{}: not a valid IPv4 address", ip_str)))?
    };

    let socket_addr = SocketAddr::new(ip, port);

    let workers: &str = matches.value_of("workers").unwrap();
    let workers: usize = workers.parse::<usize>().map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, format!("{:?}: not a valid number", workers)))?;

    Ok(CliArgs{
        games_dir: games_dir,
        socket_addr: socket_addr,
        workers: workers,
        cache_game_configs: matches.is_present("cache-configs"),
    })
}

trait GamesLoader: Send + Sync {
    fn load_games_list_as_json(&self) -> std::io::Result<Bytes>;
    fn get_game_by_id(&self, id: &str) -> std::io::Result<Arc<GameConfig>>;
}

struct StaticGamesLoader {
    json: Bytes,
    configs_by_id: HashMap<String, Arc<GameConfig>>,
}

impl StaticGamesLoader {
    fn new(games_dir: &Path) -> io::Result<Self> {
        info!("cache enabled: preloading game configs by iterating over subdirs in {:?}", games_dir);
        let games_configs = load_game_configs(&games_dir)?;
        for (k, cfg) in &games_configs {
            info!("loaded game: id = {}, name = {}, application = {}", k, cfg.name, cfg.application);
        }
        let games_response_json = Bytes::from(to_games_json(&games_configs)?);
        let configs_by_id = games_configs.iter().map(|(k, v)| (k.clone(), Arc::new(v.clone()))).collect();
        Ok(StaticGamesLoader { json: games_response_json, configs_by_id })
    }
}

impl GamesLoader for StaticGamesLoader {
    fn load_games_list_as_json(&self) -> std::io::Result<Bytes> {
        Ok(self.json.clone())
    }

    fn get_game_by_id(&self, id: &str) -> std::io::Result<Arc<GameConfig>> {
        let cfg = self.configs_by_id.get(id).ok_or(io::Error::new(ErrorKind::NotFound, "could not find game"))?;
        Ok(cfg.clone())
    }
}

struct LazyGamesLoader {
    games_dir: PathBuf,
}

impl LazyGamesLoader {
    fn new(games_dir: &Path) -> io::Result<Self> {
        info!("cache disabled: games will be loaded on-request by iterating over subdirs in {:?}", games_dir);
        Ok(LazyGamesLoader { games_dir: games_dir.clone().to_path_buf() })
    }
}

impl GamesLoader for LazyGamesLoader {
    fn load_games_list_as_json(&self) -> std::io::Result<Bytes> {
        debug!("loading game configs");
        let games_configs = load_game_configs(&self.games_dir)?;
        let games_response_json = Bytes::from(to_games_json(&games_configs)?);
        Ok(games_response_json)
    }

    fn get_game_by_id(&self, id: &str) -> std::io::Result<Arc<GameConfig>> {
        let games_configs = load_game_configs(&self.games_dir)?;
        let configs_by_id: HashMap<String, Arc<GameConfig>> = games_configs.iter().map(|(k, v)| (k.clone(), Arc::new(v.clone()))).collect();
        let config = configs_by_id.get(id).ok_or(io::Error::new(ErrorKind::NotFound, "could not find game"))?;
        Ok(config.clone())
    }
}

#[actix_rt::main]
async fn main() -> std::io::Result<()> {
    std::env::set_var("RUST_LOG", "info");
    env_logger::init();

    let cli_args = parse_cli_args()?;
    info!("bootup args: games_dir = {}, socket_addr = {:?}, workers = {}", cli_args.games_dir, cli_args.socket_addr, cli_args.workers);

    let games_dir = PathBuf::from(&cli_args.games_dir);

    info!("starting bootup");
    let games_loader: Arc<dyn GamesLoader> =
        if cli_args.cache_game_configs {
            Arc::new(StaticGamesLoader::new(&games_dir)?)
        } else {
            Arc::new(LazyGamesLoader::new(&games_dir)?)
        };

    info!("starting actix HttpServer");
    HttpServer::new(move || {
        let st = ServerState {
            next_game_id: AtomicUsize::new(0),
            configs: games_loader.clone(),
            games_dir: games_dir.clone(),
        };

        App::new()
            .data(st)
            .wrap(actix_web::middleware::Logger::default())
            .route("/api/games", web::get().to(games))
            .route("/api/play/{gameid}", web::get().to(play))
            .route("/{asset}", web::get().to(get_static_asset))
            .route("/", web::get().to(get_index))
    })
    .workers(cli_args.workers)
    .bind(cli_args.socket_addr)?
    .run()
    .await
}
